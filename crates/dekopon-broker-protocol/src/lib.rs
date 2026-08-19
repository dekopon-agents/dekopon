//! Bounded local broker wire protocol and unprivileged Unix-socket client.
//!
//! Wire requests carry no trusted identity or authorization. A server derives
//! `dekopon_broker::AuthenticatedContext` from operating-system peer credentials and trusted
//! mapping before dispatching these untrusted requests.

#![forbid(unsafe_code)]

use std::{collections::BTreeSet, fmt, io, time::Duration};

#[cfg(unix)]
use std::{
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

pub use dekopon_capability::{InvocationOutcome, InvocationResult, Permission};
use dekopon_core::{AgentId, CapabilityId, ExternalSubject, InvocationId, ProviderId, TraceId};
use dekopon_provider_sdk::ProviderCapability;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    time::timeout,
};

#[cfg(unix)]
use tokio::net::UnixStream;

/// Initial local broker protocol identifier.
pub const PROTOCOL_VERSION: &str = "dekopon.dev/broker/v1alpha1";
/// Default complete request/response frame bound (2 MiB).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
/// Hard ceiling accepted for any configured frame bound (16 MiB).
pub const HARD_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Default connection/read/write deadline.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum agents accepted in one informational gateway inventory.
pub const MAX_REPORTED_AGENTS: usize = 1_024;
/// Maximum capabilities accepted for one reported agent.
pub const MAX_REPORTED_AGENT_CAPABILITIES: usize = 1_024;
/// Maximum providers accepted for one reported agent.
pub const MAX_REPORTED_AGENT_PROVIDERS: usize = 256;
/// Maximum provider permissions accepted for one reported capability.
pub const MAX_REPORTED_PERMISSIONS: usize = 256;
/// Maximum bytes retained from one operator-authored informational string.
pub const MAX_REPORTED_TEXT_BYTES: usize = 4 * 1024;
/// Defensive ceiling on model calls represented by one accounting delta.
pub const MAX_REPORTED_MODEL_CALLS: u64 = 1_000_000;
/// Defensive ceiling on any one token-accounting delta.
pub const MAX_REPORTED_TOKENS: u64 = 1_000_000_000_000_000;

/// Stable failure code: the connected peer is not mapped by broker policy.
pub const ERROR_UNAUTHENTICATED: &str = "unauthenticated";
/// Stable failure code: the request frame could not be decoded.
pub const ERROR_INVALID_REQUEST: &str = "invalid-request";
/// Stable failure code: the broker could not complete the request and **nothing executed**.
///
/// No provider work began, so the same work may be resubmitted under a fresh invocation
/// identifier without risking a duplicate external effect.
pub const ERROR_BROKER_UNAVAILABLE: &str = "broker-unavailable";

/// A loaded provider failed to rewrite a command word.
///
/// Deliberately opaque: the guest's own failure text is provider-controlled and reaches no caller
/// through this path. An operator correlates the code with the audit event that names the word.
pub const ERROR_PROVIDER: &str = "provider-error";
/// Stable failure code: provider work may already have completed and its outcome was not audited.
///
/// The external effect may have taken place. The request must **not** be resubmitted under any
/// identifier; the durable audit is the only record of what happened.
pub const ERROR_OUTCOME_UNAUDITED: &str = "outcome-unaudited";

/// Exact protocol version carried by every envelope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProtocolVersion {
    /// Initial strict JSON protocol.
    #[serde(rename = "dekopon.dev/broker/v1alpha1")]
    V1Alpha1,
}

/// W3C `traceparent`, carrying the client's OpenTelemetry span as a remote parent.
///
/// This is distinct from [`TraceId`] and does not replace it. `TraceId` identifies a Dekopon
/// session for the audit chain and replay accounting; `TraceParent` exists only so broker spans
/// join the client's trace instead of starting an unrelated one. Two identifiers, two jobs.
///
/// Like every other request field this is untrusted: it influences telemetry correlation and
/// nothing else. It is never an authorization, routing, or replay input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceParent {
    trace_id: [u8; 16],
    parent_id: [u8; 8],
    flags: u8,
}

impl TraceParent {
    /// Builds a `traceparent` from raw identifier bytes.
    ///
    /// # Errors
    ///
    /// Returns [`TraceParentError::ZeroTraceId`] or [`TraceParentError::ZeroParentId`] when an
    /// identifier is all zeroes, which W3C defines as invalid.
    pub const fn new(
        trace_id: [u8; 16],
        parent_id: [u8; 8],
        flags: u8,
    ) -> Result<Self, TraceParentError> {
        if u128::from_be_bytes(trace_id) == 0 {
            return Err(TraceParentError::ZeroTraceId);
        }
        if u64::from_be_bytes(parent_id) == 0 {
            return Err(TraceParentError::ZeroParentId);
        }
        Ok(Self {
            trace_id,
            parent_id,
            flags,
        })
    }

    /// 16-byte trace identifier.
    #[must_use]
    pub const fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    /// 8-byte identifier of the span that should parent the broker's work.
    #[must_use]
    pub const fn parent_id(&self) -> [u8; 8] {
        self.parent_id
    }

    /// W3C trace flags; bit 0 is the sampled flag.
    #[must_use]
    pub const fn flags(&self) -> u8 {
        self.flags
    }
}

impl fmt::Display for TraceParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("00-")?;
        for byte in self.trace_id {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str("-")?;
        for byte in self.parent_id {
            write!(formatter, "{byte:02x}")?;
        }
        write!(formatter, "-{:02x}", self.flags)
    }
}

impl std::str::FromStr for TraceParent {
    type Err = TraceParentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Exactly four hyphen-separated fields. Longer forms belong to future `traceparent`
        // versions this protocol does not accept.
        let mut fields = value.split('-');
        let (Some(version), Some(trace), Some(parent), Some(flags), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(TraceParentError::Malformed);
        };
        if version != "00" {
            return Err(TraceParentError::UnsupportedVersion {
                version: version.to_owned(),
            });
        }
        let mut trace_id = [0_u8; 16];
        decode_hex(trace, &mut trace_id)?;
        let mut parent_id = [0_u8; 8];
        decode_hex(parent, &mut parent_id)?;
        let mut flag_byte = [0_u8; 1];
        decode_hex(flags, &mut flag_byte)?;
        Self::new(trace_id, parent_id, flag_byte[0])
    }
}

/// Decodes exact-width lowercase hex into `output`.
fn decode_hex(text: &str, output: &mut [u8]) -> Result<(), TraceParentError> {
    if text.len() != output.len() * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TraceParentError::Malformed);
    }
    // Uppercase hex is rejected: W3C specifies lowercase, and accepting both would let one logical
    // context serialize two ways.
    if text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(TraceParentError::Malformed);
    }
    for (index, slot) in output.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&text[start..start + 2], 16)
            .map_err(|_| TraceParentError::Malformed)?;
    }
    Ok(())
}

impl Serialize for TraceParent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TraceParent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Failures raised while parsing a `traceparent`.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TraceParentError {
    /// The value was not four hyphen-separated fields of the expected widths.
    #[error("traceparent must be `00-<32 hex>-<16 hex>-<2 hex>`")]
    Malformed,
    /// The version field named a version this protocol does not implement.
    #[error("unsupported traceparent version {version:?}; only `00` is accepted")]
    UnsupportedVersion {
        /// Version field as received.
        version: String,
    },
    /// The trace identifier was all zeroes.
    #[error("traceparent trace identifier must not be all zeroes")]
    ZeroTraceId,
    /// The parent span identifier was all zeroes.
    #[error("traceparent parent identifier must not be all zeroes")]
    ZeroParentId,
}

/// Unprivileged invocation fields accepted from a broker client.
///
/// Actor and principal are deliberately absent: the server derives them from transport identity.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InvocationRequest {
    /// Client-selected identifier reserved for replay rejection.
    pub id: InvocationId,
    /// Requested exact capability.
    pub capability: CapabilityId,
    /// End-to-end correlation identifier.
    pub trace: TraceId,
    /// Client span that should parent broker telemetry for this invocation.
    ///
    /// Always written by Dekopon's own client; `null` and an omitted key both mean the client
    /// exports no telemetry. Correlation-only, and untrusted: never an authorization, routing, or
    /// replay input. A malformed value is a decode failure rather than a silent `None`, because
    /// attaching broker spans to a trace that does not exist is worse than sending none.
    pub trace_parent: Option<TraceParent>,
    /// Capability-specific untrusted input.
    pub input: Value,
}

/// One capability visible to an authenticated broker client.
///
/// Routing and effect metadata are overwritten from trusted exact policy. Description and input
/// schema remain bounded provider-supplied model metadata and are not authorization inputs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AvailableCapability {
    /// Trusted selected provider.
    pub provider: ProviderId,
    /// Client-visible capability metadata.
    pub capability: ProviderCapability,
}

/// Informational catalog capability reported by an unprivileged gateway.
///
/// This value is never policy input. It exists only so the broker-hosted read-only web UI can show
/// what the gateway loaded from its catalog without moving catalog ownership into the broker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReportedAgentCapability {
    /// Catalog capability identifier.
    pub id: CapabilityId,
    /// Catalog provider declaration for this capability.
    pub provider: ProviderId,
    /// Catalog-declared least-privilege provider permissions.
    pub permissions: Vec<Permission>,
}

/// One informational catalog agent reported by an unprivileged gateway.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReportedAgent {
    /// Catalog agent identifier.
    pub id: AgentId,
    /// Operator-authored purpose, never standing instructions.
    pub description: String,
    /// Whether the catalog permits the gateway to route to this agent.
    pub enabled: bool,
    /// Optional model class, not a model credential or endpoint.
    pub model_class: Option<String>,
    /// Providers the agent's declared capabilities refer to.
    pub providers: Vec<ProviderId>,
    /// Capabilities the agent may propose; this inventory grants none of them.
    pub capabilities: Vec<ReportedAgentCapability>,
}

/// Complete informational agent inventory loaded by one gateway.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentInventory {
    /// Agents in deterministic identifier order.
    pub agents: Vec<ReportedAgent>,
    /// Whether defensive report bounds omitted or shortened any catalog metadata.
    #[serde(default)]
    pub truncated: bool,
}

impl AgentInventory {
    /// Checks defensive cardinality, text, and duplicate bounds before retaining a report.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if self.agents.len() > MAX_REPORTED_AGENTS {
            return false;
        }
        let mut agents = BTreeSet::new();
        self.agents.iter().all(|agent| {
            agents.insert(agent.id.clone())
                && agent.description.len() <= MAX_REPORTED_TEXT_BYTES
                && agent
                    .model_class
                    .as_ref()
                    .is_none_or(|value| value.len() <= MAX_REPORTED_TEXT_BYTES)
                && agent.providers.len() <= MAX_REPORTED_AGENT_PROVIDERS
                && agent.capabilities.len() <= MAX_REPORTED_AGENT_CAPABILITIES
                && unique(agent.providers.iter())
                && unique(agent.capabilities.iter().map(|capability| &capability.id))
                && agent.capabilities.iter().all(|capability| {
                    agent.providers.contains(&capability.provider)
                        && capability.permissions.len() <= MAX_REPORTED_PERMISSIONS
                        && capability.permissions.iter().all(|permission| {
                            permission.operation.len() <= MAX_REPORTED_TEXT_BYTES
                                && permission.resource.as_ref().is_none_or(|resource| {
                                    resource.len() <= MAX_REPORTED_TEXT_BYTES
                                })
                        })
                })
        })
    }
}

fn unique<'a, T: 'a + Ord>(values: impl IntoIterator<Item = &'a T>) -> bool {
    let mut seen = BTreeSet::new();
    values.into_iter().all(|value| seen.insert(value))
}

/// Provider-reported token accounting accumulated by an unprivileged model process.
///
/// Counts are informational and process-local. They never enter policy, authorization, execution,
/// evidence, or durable broker audit. Missing counts stay explicit rather than becoming zero.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelUsageReport {
    /// Model calls represented by this delta.
    pub model_calls: u64,
    /// Provider-reported input tokens.
    pub input_tokens: u64,
    /// Calls whose provider omitted input-token accounting.
    pub input_unreported_calls: u64,
    /// Provider-reported cached input tokens.
    pub cached_input_tokens: u64,
    /// Calls whose provider omitted cached-input accounting.
    pub cached_input_unreported_calls: u64,
    /// Provider-reported output tokens.
    pub output_tokens: u64,
    /// Calls whose provider omitted output-token accounting.
    pub output_unreported_calls: u64,
    /// Provider-reported reasoning output tokens.
    pub reasoning_output_tokens: u64,
    /// Calls whose provider omitted reasoning-token accounting.
    pub reasoning_unreported_calls: u64,
    /// Provider-reported total tokens.
    pub total_tokens: u64,
    /// Calls whose provider omitted total-token accounting.
    pub total_unreported_calls: u64,
}

impl ModelUsageReport {
    /// Validates one bounded accounting delta before it reaches a live counter.
    #[must_use]
    pub fn is_valid(self) -> bool {
        self.model_calls > 0
            && self.model_calls <= MAX_REPORTED_MODEL_CALLS
            && [
                self.input_unreported_calls,
                self.cached_input_unreported_calls,
                self.output_unreported_calls,
                self.reasoning_unreported_calls,
                self.total_unreported_calls,
            ]
            .into_iter()
            .all(|missing| missing <= self.model_calls)
            && [
                self.input_tokens,
                self.cached_input_tokens,
                self.output_tokens,
                self.reasoning_output_tokens,
                self.total_tokens,
            ]
            .into_iter()
            .all(|tokens| tokens <= MAX_REPORTED_TOKENS)
    }
}

/// One attested on-behalf-of claim accompanying a gateway's proposal.
///
/// This is deliberately a separate typed structure rather than fields on
/// [`InvocationRequest`]: the invocation payload stays identity-free, and an attestation is a
/// *claim* the server honors only when the connected peer's owner-controlled configuration
/// grants it attestor authority over the subject's namespace. The subject itself is canonical
/// routing metadata (`slack.t0123abc.u9xyz`), never message content, and the mapping from
/// subject to principal happens on the broker side alone.
///
/// `invocation` must equal the accompanying proposal's identifier. The two already travel in one
/// frame, so this is defense in depth against a future refactor separating them; a mismatch is a
/// decode-level protocol error.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SubjectAttestation {
    /// The transport-authenticated external subject the proposal is made on behalf of.
    pub subject: ExternalSubject,
    /// The named agent orchestrating on the subject's behalf.
    pub agent: AgentId,
    /// The proposal identifier this attestation is bound to.
    pub invocation: InvocationId,
}

/// One strict untrusted client request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RequestEnvelope {
    /// Exact wire protocol version.
    pub api_version: ProtocolVersion,
    /// Requested operation.
    pub request: BrokerRequest,
}

impl RequestEnvelope {
    /// Creates a capability-inspection request.
    #[must_use]
    pub const fn capabilities() -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::Capabilities,
        }
    }

    /// Creates a command-word rewrite request.
    #[must_use]
    pub const fn resolve_command(word: String, argv: Vec<String>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::ResolveCommand { word, argv },
        }
    }

    /// Creates an untrusted invocation proposal request.
    #[must_use]
    pub const fn invoke(invocation: InvocationRequest) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::Invoke { invocation },
        }
    }

    /// Creates an attested capability-inspection request.
    #[must_use]
    pub const fn capabilities_for(subject: ExternalSubject, agent: AgentId) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::CapabilitiesFor { subject, agent },
        }
    }

    /// Creates an attested on-behalf-of proposal request.
    #[must_use]
    pub const fn invoke_for(
        invocation: InvocationRequest,
        attestation: SubjectAttestation,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::InvokeFor {
                invocation,
                attestation,
            },
        }
    }

    /// Creates an informational gateway catalog report.
    #[must_use]
    pub const fn publish_agent_inventory(inventory: AgentInventory) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::PublishAgentInventory { inventory },
        }
    }

    /// Creates an informational model-token accounting report.
    #[must_use]
    pub const fn publish_model_usage(usage: ModelUsageReport) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::PublishModelUsage { usage },
        }
    }
}

/// Operations accepted by the local broker.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", deny_unknown_fields, rename_all = "camelCase")]
pub enum BrokerRequest {
    /// Lists capabilities allowed for the authenticated peer context.
    Capabilities,
    /// Submits one untrusted invocation proposal.
    Invoke {
        /// Proposal fields without principal or actor claims.
        invocation: InvocationRequest,
    },
    /// Lists capabilities for an attested on-behalf-of context.
    ///
    /// Honored only for peers whose owner-controlled configuration carries an attestor grant
    /// covering the subject's namespace; any other peer receives a stable refusal. The
    /// `operation` tag is the version seam: a broker without attestation support strict-decodes
    /// this variant into a clean protocol error rather than misreading it.
    CapabilitiesFor {
        /// The transport-authenticated external subject.
        subject: ExternalSubject,
        /// The named agent that would orchestrate on the subject's behalf.
        agent: AgentId,
    },
    /// Submits one proposal attested on behalf of an external subject.
    InvokeFor {
        /// Proposal fields without principal or actor claims.
        invocation: InvocationRequest,
        /// The gateway's on-behalf-of claim, honored only under an attestor grant.
        attestation: SubjectAttestation,
    },
    /// Rewrites one command word's arguments into a capability proposal.
    ///
    /// Deliberately not gated on the caller's grants. The rewrite is a pure function inside the
    /// declaring component — no imports, bounded by fuel and timeout — and what it returns is a
    /// *proposal*, authorized on exactly the path any other proposal takes. Gating it would add a
    /// principal check to a function that grants nothing; what stops an unauthorized caller is the
    /// authorization of the invocation that follows, not the arithmetic that shaped it.
    ResolveCommand {
        /// The command word, which must belong to a loaded provider.
        word: String,
        /// Arguments as the script supplied them, `argv[0]` being the word itself.
        argv: Vec<String>,
    },
    /// Replaces the broker's in-memory informational view of the gateway catalog.
    ///
    /// The server accepts this only from a mapped attestor peer. It grants nothing and is never
    /// consulted by authorization or provider execution.
    PublishAgentInventory {
        /// Bounded catalog metadata with no instructions, prompts, or credentials.
        inventory: AgentInventory,
    },
    /// Adds one provider-reported model-token delta to process-local informational counters.
    PublishModelUsage {
        /// Bounded usage delta with explicit unreported-call counts.
        usage: ModelUsageReport,
    },
}

/// One strict public broker response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResponseEnvelope {
    /// Exact wire protocol version.
    pub api_version: ProtocolVersion,
    /// Operation result.
    pub response: BrokerResponse,
}

impl ResponseEnvelope {
    /// Creates a successful capability response.
    #[must_use]
    pub const fn capabilities(
        capabilities: Vec<AvailableCapability>,
        command_words: Vec<String>,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::Capabilities {
                capabilities,
                command_words,
            },
        }
    }

    /// Creates a successful command-word rewrite response.
    #[must_use]
    pub const fn command_resolution(capability: CapabilityId, input: serde_json::Value) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::CommandResolution {
                capability: Some(capability),
                input: Some(input),
                message: None,
            },
        }
    }

    /// Creates a response for a provider that declined to rewrite an argv.
    #[must_use]
    pub const fn command_declined(message: String) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::CommandResolution {
                capability: None,
                input: None,
                message: Some(message),
            },
        }
    }

    /// Creates a completed invocation response, including denials and provider failures.
    #[must_use]
    pub const fn invocation(result: InvocationResult) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::Invocation { result },
        }
    }

    /// Acknowledges an informational report that was retained in memory.
    #[must_use]
    pub const fn acknowledged() -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::Acknowledged,
        }
    }

    /// Creates a stable protocol/server failure response without internal details.
    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::Error {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

/// Public response variants.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", deny_unknown_fields, rename_all = "camelCase")]
pub enum BrokerResponse {
    /// Capabilities visible under exact policy for the authenticated peer.
    Capabilities {
        /// Deterministically sorted capabilities.
        capabilities: Vec<AvailableCapability>,
        /// Command words this context may use, sorted.
        ///
        /// Carried here rather than fetched separately so a session costs one round trip, and
        /// filtered the same way the capabilities are: a word appears only when policy allows this
        /// context at least one capability of the provider declaring it. A principal granted
        /// nothing receives an empty vocabulary rather than a map of the deployment.
        ///
        /// Defaulted so a client of this version reads a broker that predates it.
        #[serde(default)]
        command_words: Vec<String>,
    },
    /// Terminal invocation result.
    Invocation {
        /// Denied, failed, or succeeded result with public evidence.
        result: InvocationResult,
    },
    /// One command word rewritten into a capability proposal.
    ///
    /// The provider may also decline, which is a usage error rather than a failure: `outcome`
    /// carries the provider's own message for the model to read.
    CommandResolution {
        /// The capability the word maps to, absent when the provider declined.
        capability: Option<CapabilityId>,
        /// The input object assembled from the arguments, absent when the provider declined.
        input: Option<serde_json::Value>,
        /// The provider's message when it declined to rewrite this argv.
        message: Option<String>,
    },
    /// An informational status report was retained in process memory.
    Acknowledged,
    /// Protocol or broker infrastructure failure.
    Error {
        /// Stable public machine code.
        code: String,
        /// Bounded non-sensitive message.
        message: String,
    },
}

/// Independent bound and deadline for one complete frame operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLimits {
    /// Maximum JSON payload bytes, excluding the four-byte prefix.
    pub max_frame_bytes: usize,
    /// Deadline for a complete prefix+payload read or write.
    pub io_timeout: Duration,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            io_timeout: DEFAULT_IO_TIMEOUT,
        }
    }
}

impl FrameLimits {
    /// Validates a configured bound before any I/O or allocation.
    pub fn validate(self) -> Result<Self, ProtocolError> {
        if self.max_frame_bytes == 0 || self.max_frame_bytes > HARD_MAX_FRAME_BYTES {
            return Err(ProtocolError::InvalidFrameLimit {
                maximum: HARD_MAX_FRAME_BYTES,
            });
        }
        if self.io_timeout.is_zero() {
            return Err(ProtocolError::ZeroTimeout);
        }
        Ok(self)
    }
}

/// Reads and strictly decodes one complete length-delimited JSON frame.
pub async fn read_frame<R, T>(reader: &mut R, limits: FrameLimits) -> Result<T, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let limits = limits.validate()?;
    let bytes = timeout(limits.io_timeout, async {
        let mut prefix = [0_u8; 4];
        reader.read_exact(&mut prefix).await?;
        let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX);
        if length == 0 {
            return Err(ReadFrameError::Empty);
        }
        if length > limits.max_frame_bytes {
            return Err(ReadFrameError::TooLarge { length });
        }
        let mut bytes = vec![0_u8; length];
        reader.read_exact(&mut bytes).await?;
        Ok(bytes)
    })
    .await
    .map_err(|_| ProtocolError::Timeout)?
    .map_err(|error| match error {
        ReadFrameError::Io(source) => ProtocolError::Io { source },
        ReadFrameError::Empty => ProtocolError::EmptyFrame,
        ReadFrameError::TooLarge { length } => ProtocolError::FrameTooLarge {
            length,
            maximum: limits.max_frame_bytes,
        },
    })?;
    serde_json::from_slice(&bytes).map_err(|source| ProtocolError::Deserialize { source })
}

#[derive(Debug)]
enum ReadFrameError {
    Io(io::Error),
    Empty,
    TooLarge { length: usize },
}

impl From<io::Error> for ReadFrameError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

/// Strictly serializes and writes one complete length-delimited JSON frame.
pub async fn write_frame<W, T>(
    writer: &mut W,
    value: &T,
    limits: FrameLimits,
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let limits = limits.validate()?;
    let mut buffer = BoundedJsonBuffer::new(limits.max_frame_bytes);
    if let Err(source) = serde_json::to_writer(&mut buffer, value) {
        if buffer.exceeded {
            return Err(ProtocolError::FrameTooLarge {
                length: limits.max_frame_bytes.saturating_add(1),
                maximum: limits.max_frame_bytes,
            });
        }
        return Err(ProtocolError::Serialize { source });
    }
    let length = u32::try_from(buffer.bytes.len()).map_err(|_| ProtocolError::FrameTooLarge {
        length: buffer.bytes.len(),
        maximum: limits.max_frame_bytes,
    })?;
    timeout(limits.io_timeout, async {
        writer.write_all(&length.to_be_bytes()).await?;
        writer.write_all(&buffer.bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| ProtocolError::Timeout)?
    .map_err(|source| ProtocolError::Io { source })
}

struct BoundedJsonBuffer {
    bytes: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}

impl BoundedJsonBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(maximum.min(8 * 1024)),
            maximum,
            exceeded: false,
        }
    }
}

impl io::Write for BoundedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame overflowed"));
        };
        if length > self.maximum {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame exceeded its limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bounded framing or strict JSON failure.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// Configured frame maximum was zero or exceeded the hard ceiling.
    #[error("frame maximum must be between 1 and {maximum} bytes")]
    InvalidFrameLimit {
        /// Hard maximum.
        maximum: usize,
    },
    /// Configured I/O timeout was zero.
    #[error("frame I/O timeout must be greater than zero")]
    ZeroTimeout,
    /// Complete frame deadline expired.
    #[error("broker frame I/O timed out")]
    Timeout,
    /// Prefix or payload I/O failed.
    #[error("broker frame I/O failed")]
    Io {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Prefix declared an empty JSON payload.
    #[error("broker frame must not be empty")]
    EmptyFrame,
    /// Prefix or serialization exceeded the configured frame bound.
    #[error("broker frame is {length} bytes; maximum is {maximum}")]
    FrameTooLarge {
        /// Actual or minimum known size.
        length: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Public response/request could not be serialized.
    #[error("could not serialize broker frame")]
    Serialize {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Frame was malformed, used an unknown version/variant, or had unknown fields.
    #[error("broker frame is not valid protocol JSON")]
    Deserialize {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
}

/// Unprivileged one-request-per-connection Unix client.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct BrokerClient {
    socket: PathBuf,
    expected_server_uid: u32,
    limits: FrameLimits,
}

#[cfg(unix)]
impl BrokerClient {
    /// Creates a client that authenticates the server by socket metadata and peer UID.
    pub fn new(
        socket: impl Into<PathBuf>,
        expected_server_uid: u32,
        limits: FrameLimits,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            socket: socket.into(),
            expected_server_uid,
            limits: limits.validate().map_err(ClientError::Protocol)?,
        })
    }

    /// Rewrites one command word's arguments into a capability proposal.
    ///
    /// `Ok(Ok((capability, input)))` is a proposal to submit; `Ok(Err(message))` is the provider
    /// declining, which the caller reports to the model as a usage error.
    pub async fn resolve_command(
        &self,
        word: String,
        argv: Vec<String>,
    ) -> Result<Result<(CapabilityId, serde_json::Value), String>, ClientError> {
        match self
            .exchange(RequestEnvelope::resolve_command(word, argv))
            .await?
        {
            BrokerResponse::CommandResolution {
                capability: Some(capability),
                input: Some(input),
                ..
            } => Ok(Ok((capability, input))),
            BrokerResponse::CommandResolution {
                message: Some(message),
                ..
            } => Ok(Err(message)),
            BrokerResponse::CommandResolution { .. } => Err(ClientError::UnexpectedResponse),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Returns capabilities and command words visible to this authenticated peer.
    pub async fn session_surface(
        &self,
    ) -> Result<(Vec<AvailableCapability>, Vec<String>), ClientError> {
        match self.exchange(RequestEnvelope::capabilities()).await? {
            BrokerResponse::Capabilities {
                capabilities,
                command_words,
            } => Ok((capabilities, command_words)),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Returns capabilities and command words for one attested on-behalf-of context.
    pub async fn session_surface_for(
        &self,
        subject: ExternalSubject,
        agent: AgentId,
    ) -> Result<(Vec<AvailableCapability>, Vec<String>), ClientError> {
        match self
            .exchange(RequestEnvelope::capabilities_for(subject, agent))
            .await?
        {
            BrokerResponse::Capabilities {
                capabilities,
                command_words,
            } => Ok((capabilities, command_words)),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Returns capabilities exact policy makes visible to this authenticated peer.
    pub async fn capabilities(&self) -> Result<Vec<AvailableCapability>, ClientError> {
        match self.exchange(RequestEnvelope::capabilities()).await? {
            BrokerResponse::Capabilities { capabilities, .. } => Ok(capabilities),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Invocation { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::Acknowledged => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submits untrusted invocation fields without any identity or authority claim.
    pub async fn invoke(
        &self,
        request: InvocationRequest,
    ) -> Result<InvocationResult, ClientError> {
        match self.exchange(RequestEnvelope::invoke(request)).await? {
            BrokerResponse::Invocation { result } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Capabilities { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::Acknowledged => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Returns capabilities visible to one attested on-behalf-of context.
    ///
    /// The claim is honored only when this client's peer identity carries a matching attestor
    /// grant in the broker's owner-controlled configuration; otherwise the broker refuses with a
    /// stable code and no capability information.
    pub async fn capabilities_for(
        &self,
        subject: ExternalSubject,
        agent: AgentId,
    ) -> Result<Vec<AvailableCapability>, ClientError> {
        match self
            .exchange(RequestEnvelope::capabilities_for(subject, agent))
            .await?
        {
            BrokerResponse::Capabilities { capabilities, .. } => Ok(capabilities),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Invocation { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::Acknowledged => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publishes a bounded, informational gateway catalog inventory.
    ///
    /// A broker accepts this only from a mapped attestor peer. The inventory is never an
    /// authorization input and a reporting failure must not be treated as loss of authority.
    pub async fn publish_agent_inventory(
        &self,
        inventory: AgentInventory,
    ) -> Result<(), ClientError> {
        match self
            .exchange(RequestEnvelope::publish_agent_inventory(inventory))
            .await?
        {
            BrokerResponse::Acknowledged => Ok(()),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publishes one bounded, informational model-token accounting delta.
    pub async fn publish_model_usage(&self, usage: ModelUsageReport) -> Result<(), ClientError> {
        match self
            .exchange(RequestEnvelope::publish_model_usage(usage))
            .await?
        {
            BrokerResponse::Acknowledged => Ok(()),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submits one proposal attested on behalf of an external subject.
    ///
    /// The attestation binds to the proposal's own identifier here, so a caller cannot construct
    /// a frame whose claim and proposal disagree.
    pub async fn invoke_for(
        &self,
        request: InvocationRequest,
        subject: ExternalSubject,
        agent: AgentId,
    ) -> Result<InvocationResult, ClientError> {
        let attestation = SubjectAttestation {
            subject,
            agent,
            invocation: request.id.clone(),
        };
        match self
            .exchange(RequestEnvelope::invoke_for(request, attestation))
            .await?
        {
            BrokerResponse::Invocation { result } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Capabilities { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::Acknowledged => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn exchange(&self, request: RequestEnvelope) -> Result<BrokerResponse, ClientError> {
        validate_socket_path(&self.socket, self.expected_server_uid).await?;
        let mut stream = timeout(self.limits.io_timeout, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| ClientError::ConnectTimeout)?
            .map_err(|source| ClientError::Connect { source })?;
        let credentials = stream
            .peer_cred()
            .map_err(|source| ClientError::PeerCredentials { source })?;
        if credentials.uid() != self.expected_server_uid {
            return Err(ClientError::ServerIdentity {
                expected: self.expected_server_uid,
                actual: credentials.uid(),
            });
        }
        write_frame(&mut stream, &request, self.limits)
            .await
            .map_err(ClientError::Protocol)?;
        let response = read_frame::<_, ResponseEnvelope>(&mut stream, self.limits)
            .await
            .map_err(ClientError::Protocol)?;
        Ok(response.response)
    }
}

#[cfg(unix)]
async fn validate_socket_path(path: &Path, expected_uid: u32) -> Result<(), ClientError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|source| ClientError::SocketMetadata { source })?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(ClientError::UnsafeSocket);
    }
    Ok(())
}

/// Client-side authentication, framing, or remote failure.
#[cfg(unix)]
#[derive(Debug, Error)]
pub enum ClientError {
    /// Socket metadata could not be inspected.
    #[error("could not inspect broker socket")]
    SocketMetadata {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Socket was not a private, single-link socket owned by the expected UID.
    #[error("broker socket is not private or owned by the expected server UID")]
    UnsafeSocket,
    /// Connecting exceeded the configured deadline.
    #[error("broker connection timed out")]
    ConnectTimeout,
    /// Unix connection failed.
    #[error("could not connect to broker socket")]
    Connect {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Peer credentials could not be read.
    #[error("could not authenticate broker peer credentials")]
    PeerCredentials {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Connected server UID disagreed with trusted configuration.
    #[error("broker peer UID {actual} does not match expected UID {expected}")]
    ServerIdentity {
        /// Expected UID.
        expected: u32,
        /// Actual peer UID.
        actual: u32,
    },
    /// Bounded framing failed.
    #[error("broker protocol failed")]
    Protocol(#[source] ProtocolError),
    /// Broker returned a stable public infrastructure failure.
    #[error("broker returned {code}: {message}")]
    Remote {
        /// Stable remote code.
        code: String,
        /// Bounded public message.
        message: String,
    },
    /// Response operation did not match the request.
    #[error("broker returned an unexpected response variant")]
    UnexpectedResponse,
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(PROTOCOL_VERSION)
    }
}

#[cfg(test)]
mod tests;
