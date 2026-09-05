//! Bounded local broker wire protocol and unprivileged Unix-socket client.
//!
//! Wire requests carry no trusted identity or authorization. A server derives
//! `dekopon_broker::AuthenticatedContext` from operating-system peer credentials and trusted
//! mapping before dispatching these untrusted requests.

#![forbid(unsafe_code)]

mod control;
#[cfg(unix)]
pub use control::ControlClient;
pub use control::{
    ControlDecision, ControlOutcome, ControlProposal, ControlScope, ControlTarget,
    ControlTargetsError, MAX_CONTROL_ATTEMPTS, MAX_CONTROL_TARGETS, VerifiedControlDecision,
    validate_control_targets,
};

use std::{collections::BTreeSet, fmt, io, time::Duration};

#[cfg(unix)]
use std::{
    env,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

pub use dekopon_capability::{InvocationOutcome, InvocationResult, Permission};
use dekopon_core::{
    AgentId, CapabilityId, ExternalSubject, InvocationId, ProviderId, SecretUseProposal,
    SurfaceEpoch, TraceId, TransportId,
};
use dekopon_provider_sdk::ProviderCapability;
pub use dekopon_provider_sdk::{CommandRunOutcome, ComponentFailure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    time::timeout,
};

#[cfg(unix)]
use tokio::net::UnixStream;

/// Current local broker protocol identifier.
pub const PROTOCOL_VERSION: &str = "dekopon.dev/broker/v1alpha3";
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
/// Stable pre-execution storage failure codes. No provider work began, so a corrected request may
/// use a fresh invocation identifier.
pub const ERROR_STORAGE_QUOTA: &str = "storage-quota";
pub const ERROR_STORAGE_BUSY: &str = "storage-busy";
pub const ERROR_STORAGE_TIMEOUT: &str = "storage-timeout";
pub const ERROR_STORAGE_CORRUPT: &str = "storage-corrupt";
pub const ERROR_STORAGE_IO: &str = "storage-io";

/// Stable failure code: a bounded broker resource is exhausted and nothing executed.
///
/// Distinct from [`ERROR_BROKER_UNAVAILABLE`] because the exhaustion is permanent rather than
/// momentary: the replay ledger never evicts and is restored from durable history on restart, and
/// the audit log does not rotate. Resubmission under a fresh invocation identifier is *safe* — no
/// provider work began — and it is also futile, because it fails identically until an operator
/// raises `maxReplayIds` / `auditMaxRecords` or moves the audit file aside. A client must not
/// retry this.
pub const ERROR_CAPACITY_EXHAUSTED: &str = "capacity-exhausted";

/// Exact protocol version carried by every envelope.
///
/// One variant, so there is no negotiation: every envelope is strict-decoded and any other string
/// fails to deserialize. `v1alpha3` adds bound core-session controls and a required startup epoch.
/// Older envelopes refuse before dispatch; there is no compatibility fallback.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProtocolVersion {
    /// Strict JSON protocol with one operation per verb.
    #[serde(rename = "dekopon.dev/broker/v1alpha3")]
    V1Alpha3,
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
#[allow(
    clippy::map_err_ignore,
    reason = "the guards below already proved exact width and all-lowercase ASCII hex, so the digit-pair ParseIntError is unreachable"
)]
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
    /// Optional typed, untrusted intent to use a public DRN in a broker-native sink.
    ///
    /// The field is proposal data, not a credential or bearer grant. Providers never receive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_use: Option<SecretUseProposal>,
    /// Capability-specific untrusted input.
    pub input: Value,
}

/// Transport family that authenticated one chat scope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatTransportKind {
    Slack,
    Discord,
    Telegram,
    Whatsapp,
    Local,
}

impl fmt::Display for ChatTransportKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Whatsapp => "whatsapp",
            Self::Local => "local",
        })
    }
}

/// Bounded transport-derived channel and conversation scope.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatScopeClaim {
    pub transport: TransportId,
    pub kind: ChatTransportKind,
    #[serde(deserialize_with = "deserialize_scope_part")]
    pub channel: String,
    #[serde(deserialize_with = "deserialize_scope_part")]
    pub conversation: String,
}

impl fmt::Debug for ChatScopeClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChatScopeClaim([REDACTED])")
    }
}

impl ChatScopeClaim {
    /// Defensive wire bounds common to every service-specific canonical form.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        bounded_scope_part(&self.channel) && bounded_scope_part(&self.conversation)
    }
}

fn bounded_scope_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.bytes().any(|byte| byte.is_ascii_control())
        && !value.contains(['/', '\\'])
}

fn deserialize_scope_part<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    bounded_scope_part(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("chat scope part is not canonical"))
}

fn deserialize_bounded_string<'de, D, const MAXIMUM: usize>(
    deserializer: D,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor<const MAXIMUM: usize>;

    impl<'de, const MAXIMUM: usize> serde::de::Visitor<'de> for Visitor<MAXIMUM> {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a string no longer than {MAXIMUM} bytes")
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(value)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value.len() > MAXIMUM {
                return Err(E::invalid_length(value.len(), &self));
            }
            Ok(value.to_owned())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value.len() > MAXIMUM {
                return Err(E::invalid_length(value.len(), &self));
            }
            Ok(value)
        }
    }

    deserializer.deserialize_string(Visitor::<MAXIMUM>)
}

/// One on-behalf-of claim accompanying an operation, absent when a peer speaks as itself.
///
/// Attestation shape is one axis, not one operation per shape. A subject-only claim carries no
/// `scope` and derives the legacy attested context; a chat claim adds the transport scope that
/// invocation-bound chat authority is checked against. Both are the same claim about the same two
/// things — which authenticated external identity the peer is relaying, and which agent is
/// orchestrating for it — so both travel in this one structure and every operation takes it
/// optionally.
///
/// It is a *claim*, never authority. It carries no principal, because the subject-to-principal
/// mapping is owner-controlled broker state; the broker honors the claim only when the connected
/// peer's configuration grants attestor authority over the subject's namespace, and the broker
/// alone performs the mapping. It is a separate structure rather than fields on
/// [`InvocationRequest`] so that an invocation payload stays identity-free whether or not one
/// accompanies it.
///
/// `invocation` is present exactly for the operations that carry a proposal, where it must equal
/// that proposal's identifier. The two already travel in one frame, so this is defense in depth
/// against a future refactor separating them; a disagreement — or an identifier on an operation
/// with no proposal to bind to — is a decode-level protocol error rather than a policy decision.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Attestation {
    /// The transport-authenticated external subject the operation is made on behalf of.
    pub subject: ExternalSubject,
    /// The named agent orchestrating on the subject's behalf.
    pub agent: AgentId,
    /// The claimed chat transport scope; absent for a subject-only attestation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ChatScopeClaim>,
    /// The proposal identifier this claim is bound to; absent when no proposal accompanies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<InvocationId>,
}

impl fmt::Debug for Attestation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Attestation([REDACTED])")
    }
}

impl Attestation {
    /// Builds a subject-only claim for an operation that carries no proposal.
    #[must_use]
    pub const fn for_subject(subject: ExternalSubject, agent: AgentId) -> Self {
        Self {
            subject,
            agent,
            scope: None,
            invocation: None,
        }
    }

    /// Builds a chat-scoped claim for an operation that carries no proposal.
    #[must_use]
    pub const fn for_chat(subject: ExternalSubject, agent: AgentId, scope: ChatScopeClaim) -> Self {
        Self {
            subject,
            agent,
            scope: Some(scope),
            invocation: None,
        }
    }

    /// The same claim bound to the proposal it accompanies.
    #[must_use]
    pub fn bound_to(&self, invocation: InvocationId) -> Self {
        Self {
            invocation: Some(invocation),
            ..self.clone()
        }
    }

    /// Whether the claimed scope, if any, is inside the defensive wire bounds.
    ///
    /// Structural only: it consults no grant and decides nothing about authority.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.scope.as_ref().is_none_or(ChatScopeClaim::is_bounded)
    }

    /// Whether this claim is bound to exactly the proposal it travelled with.
    #[must_use]
    pub fn binds(&self, invocation: &InvocationId) -> bool {
        self.invocation.as_ref() == Some(invocation)
    }
}

/// Service-specific identity of the inbound delivery whose answer was accepted.
///
/// The tagged shape prevents a Slack timestamp, Discord snowflake, Telegram message, WhatsApp
/// message ID, or local nonce from being replayed under another transport kind. Scope fields are checked
/// against the separately attested chat scope before any namespace is derived.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DeliveryIdentity {
    Slack {
        #[serde(deserialize_with = "deserialize_scope_part")]
        channel: String,
        #[serde(deserialize_with = "deserialize_scope_part")]
        timestamp: String,
    },
    Discord {
        #[serde(deserialize_with = "deserialize_scope_part")]
        channel: String,
        #[serde(deserialize_with = "deserialize_scope_part")]
        message: String,
    },
    Telegram {
        #[serde(deserialize_with = "deserialize_signed_service_decimal")]
        chat: String,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_positive_service_decimal"
        )]
        topic: Option<String>,
        #[serde(deserialize_with = "deserialize_positive_service_decimal")]
        message: String,
    },
    Whatsapp {
        #[serde(deserialize_with = "deserialize_meta_decimal")]
        waba: String,
        #[serde(deserialize_with = "deserialize_meta_decimal")]
        phone_number: String,
        #[serde(deserialize_with = "deserialize_whatsapp_message_id")]
        message: String,
    },
    Local {
        transport: TransportId,
        #[serde(deserialize_with = "deserialize_scope_part")]
        conversation: String,
        #[serde(deserialize_with = "deserialize_scope_part")]
        boot_nonce: String,
        connection: u64,
        sequence: u64,
    },
}

impl fmt::Debug for DeliveryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeliveryIdentity([REDACTED])")
    }
}

impl DeliveryIdentity {
    #[must_use]
    pub fn is_canonical_for(&self, scope: &ChatScopeClaim) -> bool {
        match (self, scope.kind) {
            (Self::Slack { channel, timestamp }, ChatTransportKind::Slack) => {
                channel == &scope.channel && canonical_slack_timestamp(timestamp)
            }
            (Self::Discord { channel, message }, ChatTransportKind::Discord) => {
                channel == &scope.channel
                    && canonical_unsigned_decimal(channel)
                    && canonical_unsigned_decimal(message)
            }
            (
                Self::Telegram {
                    chat,
                    topic,
                    message,
                },
                ChatTransportKind::Telegram,
            ) => {
                let expected_topic = scope
                    .conversation
                    .strip_prefix(&format!("{}:topic:", scope.channel));
                chat == &scope.channel
                    && canonical_signed_decimal(chat)
                    && topic.as_deref() == expected_topic
                    && topic
                        .as_deref()
                        .is_none_or(canonical_positive_service_decimal)
                    && canonical_positive_service_decimal(message)
            }
            (
                Self::Whatsapp {
                    waba,
                    phone_number,
                    message,
                },
                ChatTransportKind::Whatsapp,
            ) => {
                let mut parts = scope.channel.split(':');
                let canonical = parts.next() == Some(waba.as_str())
                    && parts.next() == Some(phone_number.as_str())
                    && parts.next().is_some_and(canonical_meta_decimal)
                    && parts.next().is_none();
                canonical
                    && scope.conversation == scope.channel
                    && canonical_meta_decimal(waba)
                    && canonical_meta_decimal(phone_number)
                    && canonical_whatsapp_message_id(message)
            }
            (
                Self::Local {
                    transport,
                    conversation,
                    boot_nonce,
                    connection,
                    sequence,
                },
                ChatTransportKind::Local,
            ) => {
                transport == &scope.transport
                    && conversation == &scope.conversation
                    && boot_nonce.len() == 32
                    && boot_nonce
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    && *connection > 0
                    && *sequence > 0
            }
            _ => false,
        }
    }
}

fn deserialize_whatsapp_message_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    canonical_whatsapp_message_id(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("WhatsApp message ID is not canonical"))
}

fn canonical_whatsapp_message_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn deserialize_meta_decimal<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 64>(deserializer)?;
    canonical_meta_decimal(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("identifier is not a canonical Meta decimal"))
}

fn canonical_meta_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn deserialize_positive_service_decimal<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    canonical_positive_service_decimal(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("identifier is outside the positive service range"))
}

fn deserialize_signed_service_decimal<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    canonical_signed_decimal(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("identifier is outside the signed service range"))
}

fn deserialize_optional_positive_service_decimal<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalServiceDecimal;

    impl<'de> serde::de::Visitor<'de> for OptionalServiceDecimal {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("null or a canonical positive signed-service identifier")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<Inner>(self, deserializer: Inner) -> Result<Self::Value, Inner::Error>
        where
            Inner: serde::Deserializer<'de>,
        {
            deserialize_positive_service_decimal(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(OptionalServiceDecimal)
}

fn canonical_slack_timestamp(value: &str) -> bool {
    value.split_once('.').is_some_and(|(seconds, fraction)| {
        seconds.len() == 10
            && fraction.len() == 6
            && !seconds.starts_with('0')
            && seconds.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn canonical_unsigned_decimal(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|number| number != 0 && number.to_string() == value)
}

fn canonical_positive_service_decimal(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|number| number > 0 && number.to_string() == value)
}

fn canonical_signed_decimal(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|number| number != 0 && number.to_string() == value)
}

/// Exact turn accepted by a transport and proposed once for model-hidden recording.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeliveredTurnRequest {
    pub id: InvocationId,
    pub trace: TraceId,
    pub trace_parent: Option<TraceParent>,
    pub delivery: DeliveryIdentity,
    #[serde(deserialize_with = "deserialize_turn_text")]
    pub user: String,
    #[serde(deserialize_with = "deserialize_turn_text")]
    pub assistant: String,
}

impl fmt::Debug for DeliveredTurnRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeliveredTurnRequest([REDACTED])")
    }
}

impl DeliveredTurnRequest {
    /// Validates the complete text bound. Delivery canonicalization additionally needs the
    /// separately attested scope and is checked by the broker.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.user
            .len()
            .checked_add(self.assistant.len())
            .is_some_and(|bytes| bytes <= 64 * 1024)
    }
}

fn deserialize_turn_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string::<D, { 64 * 1024 }>(deserializer)
}

/// Broker-derived optional memory surface for one freshly authorized chat scope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatMemorySurface {
    pub max_lookback_turns: u32,
    pub prompt_note: String,
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
        self.validate().is_ok()
    }

    /// Checks the same bounds as [`AgentInventory::is_valid`], naming the first one violated.
    ///
    /// A server keeps the wire message generic and logs this locally, so an operator whose web UI
    /// inventory went stale can learn which agent and which bound was at fault instead of reading
    /// one fixed string on both ends.
    ///
    /// # Errors
    ///
    /// Returns the [`InventoryError`] describing the first violated bound. The error names
    /// validated identifiers and byte counts only; no operator-authored text reaches it.
    pub fn validate(&self) -> Result<(), InventoryError> {
        if self.agents.len() > MAX_REPORTED_AGENTS {
            return Err(InventoryError::TooManyAgents {
                count: self.agents.len(),
                maximum: MAX_REPORTED_AGENTS,
            });
        }
        let mut reported = BTreeSet::new();
        for agent in &self.agents {
            if !reported.insert(agent.id.clone()) {
                return Err(InventoryError::DuplicateAgent {
                    agent: agent.id.clone(),
                });
            }
            agent.validate()?;
        }
        Ok(())
    }
}

impl ReportedAgent {
    fn validate(&self) -> Result<(), InventoryError> {
        self.check_text("description", self.description.len())?;
        if let Some(model_class) = &self.model_class {
            self.check_text("model class", model_class.len())?;
        }
        self.check_count(
            "providers",
            self.providers.len(),
            MAX_REPORTED_AGENT_PROVIDERS,
        )?;
        self.check_count(
            "capabilities",
            self.capabilities.len(),
            MAX_REPORTED_AGENT_CAPABILITIES,
        )?;
        if let Some(provider) = duplicate(self.providers.iter()) {
            return Err(InventoryError::DuplicateProvider {
                agent: self.id.clone(),
                provider: provider.clone(),
            });
        }
        if let Some(capability) = duplicate(self.capabilities.iter().map(|entry| &entry.id)) {
            return Err(InventoryError::DuplicateCapability {
                agent: self.id.clone(),
                capability: capability.clone(),
            });
        }
        for capability in &self.capabilities {
            if !self.providers.contains(&capability.provider) {
                return Err(InventoryError::UndeclaredProvider {
                    agent: self.id.clone(),
                    capability: capability.id.clone(),
                    provider: capability.provider.clone(),
                });
            }
            if capability.permissions.len() > MAX_REPORTED_PERMISSIONS {
                return Err(InventoryError::TooManyPermissions {
                    agent: self.id.clone(),
                    capability: capability.id.clone(),
                    count: capability.permissions.len(),
                    maximum: MAX_REPORTED_PERMISSIONS,
                });
            }
            for permission in &capability.permissions {
                self.check_text("permission operation", permission.operation.len())?;
                if let Some(resource) = &permission.resource {
                    self.check_text("permission resource", resource.len())?;
                }
            }
        }
        Ok(())
    }

    fn check_text(&self, field: &'static str, bytes: usize) -> Result<(), InventoryError> {
        if bytes > MAX_REPORTED_TEXT_BYTES {
            return Err(InventoryError::TextTooLong {
                agent: self.id.clone(),
                field,
                bytes,
                maximum: MAX_REPORTED_TEXT_BYTES,
            });
        }
        Ok(())
    }

    fn check_count(
        &self,
        collection: &'static str,
        count: usize,
        maximum: usize,
    ) -> Result<(), InventoryError> {
        if count > maximum {
            return Err(InventoryError::TooMany {
                agent: self.id.clone(),
                collection,
                count,
                maximum,
            });
        }
        Ok(())
    }
}

fn duplicate<'a, T: 'a + Ord>(values: impl IntoIterator<Item = &'a T>) -> Option<&'a T> {
    let mut seen = BTreeSet::new();
    values.into_iter().find(|value| !seen.insert(*value))
}

/// The first defensive bound one informational agent inventory violated.
///
/// Every field is a validated identifier or a byte count, never operator-authored text, so a
/// server may log this without moving catalog prose or prompt content into its logs.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InventoryError {
    /// More agents than [`MAX_REPORTED_AGENTS`].
    #[error("inventory reports {count} agents; maximum is {maximum}")]
    TooManyAgents {
        /// Reported agents.
        count: usize,
        /// Accepted maximum.
        maximum: usize,
    },
    /// One agent identifier appeared more than once.
    #[error("inventory reports agent `{agent}` more than once")]
    DuplicateAgent {
        /// Repeated agent.
        agent: AgentId,
    },
    /// One operator-authored string exceeded [`MAX_REPORTED_TEXT_BYTES`].
    #[error("agent `{agent}` {field} is {bytes} bytes; maximum is {maximum}")]
    TextTooLong {
        /// Offending agent.
        agent: AgentId,
        /// Which string exceeded the bound.
        field: &'static str,
        /// Reported byte length.
        bytes: usize,
        /// Accepted maximum.
        maximum: usize,
    },
    /// One agent collection exceeded its cardinality bound.
    #[error("agent `{agent}` reports {count} {collection}; maximum is {maximum}")]
    TooMany {
        /// Offending agent.
        agent: AgentId,
        /// Which collection exceeded its bound.
        collection: &'static str,
        /// Reported entries.
        count: usize,
        /// Accepted maximum.
        maximum: usize,
    },
    /// One agent listed the same provider twice.
    #[error("agent `{agent}` reports provider `{provider}` more than once")]
    DuplicateProvider {
        /// Offending agent.
        agent: AgentId,
        /// Repeated provider.
        provider: ProviderId,
    },
    /// One agent listed the same capability twice.
    #[error("agent `{agent}` reports capability `{capability}` more than once")]
    DuplicateCapability {
        /// Offending agent.
        agent: AgentId,
        /// Repeated capability.
        capability: CapabilityId,
    },
    /// One capability named a provider its agent does not declare.
    #[error("agent `{agent}` capability `{capability}` names undeclared provider `{provider}`")]
    UndeclaredProvider {
        /// Offending agent.
        agent: AgentId,
        /// Offending capability.
        capability: CapabilityId,
        /// Provider missing from the agent's own list.
        provider: ProviderId,
    },
    /// One capability exceeded [`MAX_REPORTED_PERMISSIONS`].
    #[error(
        "agent `{agent}` capability `{capability}` reports {count} permissions; maximum is {maximum}"
    )]
    TooManyPermissions {
        /// Offending agent.
        agent: AgentId,
        /// Offending capability.
        capability: CapabilityId,
        /// Reported permissions.
        count: usize,
        /// Accepted maximum.
        maximum: usize,
    },
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
        self.validate().is_ok()
    }

    /// Checks the same bounds as [`ModelUsageReport::is_valid`], naming the first one violated.
    ///
    /// # Errors
    ///
    /// Returns the [`UsageReportError`] describing the first violated bound. Every field is a
    /// count, so a server may log it without moving any model or prompt content into its logs.
    pub fn validate(self) -> Result<(), UsageReportError> {
        if self.model_calls == 0 || self.model_calls > MAX_REPORTED_MODEL_CALLS {
            return Err(UsageReportError::ModelCalls {
                count: self.model_calls,
                maximum: MAX_REPORTED_MODEL_CALLS,
            });
        }
        for (field, count) in [
            ("input", self.input_unreported_calls),
            ("cached input", self.cached_input_unreported_calls),
            ("output", self.output_unreported_calls),
            ("reasoning", self.reasoning_unreported_calls),
            ("total", self.total_unreported_calls),
        ] {
            if count > self.model_calls {
                return Err(UsageReportError::UnreportedCalls {
                    field,
                    count,
                    calls: self.model_calls,
                });
            }
        }
        for (field, count) in [
            ("input", self.input_tokens),
            ("cached input", self.cached_input_tokens),
            ("output", self.output_tokens),
            ("reasoning output", self.reasoning_output_tokens),
            ("total", self.total_tokens),
        ] {
            if count > MAX_REPORTED_TOKENS {
                return Err(UsageReportError::Tokens {
                    field,
                    count,
                    maximum: MAX_REPORTED_TOKENS,
                });
            }
        }
        Ok(())
    }
}

/// The first defensive bound one model-usage accounting delta violated.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum UsageReportError {
    /// The delta represented no calls, or more than [`MAX_REPORTED_MODEL_CALLS`].
    #[error("report covers {count} model calls; expected 1 to {maximum}")]
    ModelCalls {
        /// Reported calls.
        count: u64,
        /// Accepted maximum.
        maximum: u64,
    },
    /// More calls omitted one accounting field than the delta has calls.
    #[error("report omits {field} accounting for {count} of its {calls} model calls")]
    UnreportedCalls {
        /// Which accounting field.
        field: &'static str,
        /// Calls reported as missing that field.
        count: u64,
        /// Calls the delta represents.
        calls: u64,
    },
    /// One token count exceeded [`MAX_REPORTED_TOKENS`].
    #[error("report counts {count} {field} tokens; maximum is {maximum}")]
    Tokens {
        /// Which token field.
        field: &'static str,
        /// Reported tokens.
        count: u64,
        /// Accepted maximum.
        maximum: u64,
    },
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
    /// Creates a capability-inspection request for the peer, or for one attested context.
    #[must_use]
    pub const fn capabilities(attestation: Option<Attestation>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            request: BrokerRequest::Capabilities { attestation },
        }
    }

    /// Creates a command-word run request carrying the value piped into the word, if any.
    #[must_use]
    pub const fn run_command(
        attestation: Option<Attestation>,
        word: String,
        argv: Vec<String>,
        stdin: Option<String>,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            request: BrokerRequest::RunCommand {
                attestation,
                word,
                argv,
                stdin,
            },
        }
    }

    /// Creates an invocation proposal request, optionally attested on behalf of a subject.
    #[must_use]
    pub const fn invoke(attestation: Option<Attestation>, invocation: InvocationRequest) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            request: BrokerRequest::Invoke {
                attestation,
                invocation,
            },
        }
    }

    /// Creates the dedicated model-hidden post-acceptance record proposal.
    #[must_use]
    pub const fn record_delivered_turn(
        attestation: Attestation,
        turn: DeliveredTurnRequest,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            request: BrokerRequest::RecordDeliveredTurn { attestation, turn },
        }
    }

    /// Creates an informational gateway catalog report.
    #[must_use]
    pub const fn publish_agent_inventory(inventory: AgentInventory) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            request: BrokerRequest::PublishAgentInventory { inventory },
        }
    }

    /// Creates an informational model-token accounting report.
    #[must_use]
    pub const fn publish_model_usage(usage: ModelUsageReport) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            request: BrokerRequest::PublishModelUsage { usage },
        }
    }
}

/// Operations accepted by the local broker.
///
/// One operation per verb. Whether a caller speaks as its own authenticated peer, on behalf of an
/// external subject, or inside a bounded chat scope is [`Attestation`] — a field on the operation,
/// not a separate operation per shape. The `operation` tag stays strict-decoded, so an operation a
/// broker does not know is a clean protocol error rather than a misread proposal.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", deny_unknown_fields, rename_all = "camelCase")]
pub enum BrokerRequest {
    /// Fresh admission for one core model/effort transition; never invokes a provider.
    AuthorizeControl {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        proposal: ControlProposal,
    },
    /// Lists the capabilities and command words allowed for this context.
    ///
    /// Without an attestation this is the authenticated peer's own listing, which is never
    /// refused. With one it is the attested context's, honored only for peers whose
    /// owner-controlled configuration carries an attestor grant covering the subject's namespace
    /// — and any other peer receives a stable refusal that discloses nothing, not even whether
    /// the subject is mapped. A chat-scoped claim additionally answers with the durable-memory
    /// surface when all three of its grants are effective.
    Capabilities {
        /// The on-behalf-of claim, or `None` to speak as the connected peer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    /// Rewrites one command word's arguments into a capability proposal: the legacy operation.
    ///
    /// Kept so a client that predates [`BrokerRequest::RunCommand`] keeps working against this
    /// broker for one release. This client no longer sends it. A server answers it exactly as it
    /// answers a run with no piped value, except that text a `run-command` guest rendered — a
    /// help page, a usage error — has no shape on this answer and reaches the caller as a decline
    /// carrying that text.
    ///
    /// Deliberately not gated on the caller's grants, for the reason [`BrokerRequest::RunCommand`]
    /// gives.
    ResolveCommand {
        /// The on-behalf-of claim, or `None` to speak as the connected peer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        /// The command word, which must belong to a loaded provider.
        word: String,
        /// Arguments as the script supplied them, **without** the word itself.
        argv: Vec<String>,
    },
    /// Runs one command word as the command-line program its provider declared.
    ///
    /// Deliberately not gated on the caller's grants. The run is a pure function inside the
    /// declaring component — no imports, bounded by fuel and timeout — and what it returns is a
    /// *proposal*, authorized on exactly the path any other proposal takes, or text the guest
    /// rendered itself, which authorizes nothing. Gating it would add a principal check to a
    /// function that grants nothing; what stops an unauthorized caller is the authorization of the
    /// invocation that follows, not the arithmetic that shaped it. An attestation, when one is
    /// supplied, is still validated as a claim: a caller cannot run a word under a subject or
    /// scope the broker refuses it.
    RunCommand {
        /// The on-behalf-of claim, or `None` to speak as the connected peer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        /// The command word, which must belong to a loaded provider.
        word: String,
        /// Arguments as the script supplied them, **without** the word itself.
        ///
        /// The word travels in its own field because the broker selects the declaring provider by
        /// it before the guest runs, so repeating it here would give the guest a second, editable
        /// copy of a routing decision already made.
        argv: Vec<String>,
        /// The value the script piped into the word, already rendered to text by the shell's
        /// display rule; absent when nothing was piped.
        ///
        /// It rides this frame under [`FrameLimits::max_frame_bytes`], and the broker host counts
        /// it with the argv against its own input bound before a store exists, so an oversized
        /// value is refused twice and preallocated for nowhere.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdin: Option<String>,
    },
    /// Submits one untrusted invocation proposal.
    Invoke {
        /// The on-behalf-of claim, bound to `invocation.id`, or `None` for a direct proposal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        /// Proposal fields without principal or actor claims.
        invocation: InvocationRequest,
    },
    /// Dedicated model-hidden recording operation after transport acceptance.
    ///
    /// Its own operation on purpose: the chat-memory record route is unreachable through
    /// [`BrokerRequest::Invoke`], [`BrokerRequest::RunCommand`], and
    /// [`BrokerRequest::ResolveCommand`] whatever attestation accompanies them, so recording
    /// cannot be reached by a proposal a model shaped.
    RecordDeliveredTurn {
        /// The chat-scoped claim, bound to `turn.id`. A subject-only claim cannot record.
        attestation: Attestation,
        /// Typed post-acceptance fields the broker turns into the proposal itself.
        turn: DeliveredTurnRequest,
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
    pub fn capabilities(
        capabilities: Vec<AvailableCapability>,
        command_words: Vec<String>,
        surface_epoch: SurfaceEpoch,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            response: BrokerResponse::Capabilities {
                surface_epoch,
                capabilities,
                command_words,
                chat_memory: None,
            },
        }
    }

    /// Creates a successful freshly authorized chat capability response.
    #[must_use]
    pub fn chat_capabilities(
        capabilities: Vec<AvailableCapability>,
        command_words: Vec<String>,
        chat_memory: Option<ChatMemorySurface>,
        surface_epoch: SurfaceEpoch,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            response: BrokerResponse::Capabilities {
                capabilities,
                command_words,
                chat_memory,
                surface_epoch,
            },
        }
    }

    /// Creates a successful command-word rewrite response.
    #[must_use]
    pub const fn command_resolution(capability: CapabilityId, input: serde_json::Value) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
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
            api_version: ProtocolVersion::V1Alpha3,
            response: BrokerResponse::CommandResolution {
                capability: None,
                input: None,
                message: Some(message),
            },
        }
    }

    /// Creates the response to a command-word run: a proposal, rendered text, or a decline.
    #[must_use]
    pub const fn command_run(result: CommandRunOutcome) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            response: BrokerResponse::CommandRun { result },
        }
    }

    /// Creates a completed invocation response, including denials and provider failures.
    #[must_use]
    pub const fn invocation(result: InvocationResult) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            response: BrokerResponse::Invocation { result },
        }
    }

    /// Acknowledges an informational report that was retained in memory.
    #[must_use]
    pub const fn acknowledged() -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
            response: BrokerResponse::Acknowledged,
        }
    }

    /// Creates a stable protocol/server failure response without internal details.
    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha3,
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
    /// Admission or refusal for one completely echoed control proposal.
    ControlDecision { decision: Box<ControlDecision> },
    /// Capabilities visible under exact policy for the authenticated peer.
    Capabilities {
        /// Deterministically sorted capabilities.
        capabilities: Vec<AvailableCapability>,
        /// Host-only random broker-startup epoch; not model-visible permission.
        surface_epoch: SurfaceEpoch,
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
        /// Present only for an effective all-three chat-memory surface.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_memory: Option<ChatMemorySurface>,
    },
    /// Terminal invocation result.
    Invocation {
        /// Denied, failed, or succeeded result with public evidence.
        result: InvocationResult,
    },
    /// One command word rewritten into a capability proposal: the legacy answer to
    /// [`BrokerRequest::ResolveCommand`].
    ///
    /// The provider may also decline, which is a usage error rather than a failure: `message`
    /// carries the provider's own text for the model to read. Text a guest rendered arrives the
    /// same way, because this shape has nowhere else to put it.
    CommandResolution {
        /// The capability the word maps to, absent when the provider declined.
        capability: Option<CapabilityId>,
        /// The input object assembled from the arguments, absent when the provider declined.
        input: Option<serde_json::Value>,
        /// The provider's message when it declined to rewrite this argv.
        message: Option<String>,
    },
    /// One command word run to completion: the answer to [`BrokerRequest::RunCommand`].
    ///
    /// The guest's own outcome shape travels intact — a proposal to submit, text it rendered with
    /// the exit status it chose, or a decline carrying its stable code and message — so the
    /// script sees exactly what the upstream command-line tool would have printed.
    CommandRun {
        /// What the provider answered.
        result: CommandRunOutcome,
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

/// Length prefix carried at the head of every frame.
const FRAME_PREFIX_BYTES: usize = 4;
/// Payload bytes allocated up front, however large the peer's prefix claims the frame is.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Reads and strictly decodes one complete length-delimited JSON frame.
///
/// Allocation follows the bytes that actually arrive rather than the length the peer claims: a
/// connected peer that sends only a prefix and then stalls holds one chunk until the deadline, not
/// a whole frame's worth of zeroed memory per connection.
pub async fn read_frame<R, T>(reader: &mut R, limits: FrameLimits) -> Result<T, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let limits = limits.validate()?;
    #[allow(
        clippy::map_err_ignore,
        reason = "tokio's Elapsed says only that io_timeout expired, which ProtocolError::Timeout already states"
    )]
    let bytes = timeout(limits.io_timeout, async {
        let mut prefix = [0_u8; FRAME_PREFIX_BYTES];
        reader.read_exact(&mut prefix).await?;
        let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX);
        if length == 0 {
            return Err(ReadFrameError::Empty);
        }
        if length > limits.max_frame_bytes {
            return Err(ReadFrameError::TooLarge { length });
        }
        let mut bytes = Vec::with_capacity(length.min(READ_CHUNK_BYTES));
        (&mut *reader)
            .take(length as u64)
            .read_to_end(&mut bytes)
            .await?;
        // `read_to_end` stops at end of stream as well as at the limit, so a peer that announces
        // more than it sends must still fail rather than decoding a short frame.
        if bytes.len() != length {
            return Err(ReadFrameError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "broker frame ended before its declared length",
            )));
        }
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
///
/// The prefix is patched into space the serialization buffer already reserved, so one frame is one
/// `write_all` rather than a prefix syscall followed by a payload syscall on an unbuffered socket.
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
    let payload = buffer.payload_len();
    #[allow(
        clippy::map_err_ignore,
        reason = "TryFromIntError carries only out-of-range, and FrameTooLarge already names the length and the maximum"
    )]
    let length = u32::try_from(payload).map_err(|_| ProtocolError::FrameTooLarge {
        length: payload,
        maximum: limits.max_frame_bytes,
    })?;
    buffer.frame[..FRAME_PREFIX_BYTES].copy_from_slice(&length.to_be_bytes());
    #[allow(
        clippy::map_err_ignore,
        reason = "tokio's Elapsed says only that io_timeout expired, which ProtocolError::Timeout already states"
    )]
    timeout(limits.io_timeout, async {
        writer.write_all(&buffer.frame).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| ProtocolError::Timeout)?
    .map_err(|source| ProtocolError::Io { source })
}

/// Serialization target holding the complete frame: a reserved length prefix, then the payload.
struct BoundedJsonBuffer {
    frame: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}

impl BoundedJsonBuffer {
    fn new(maximum: usize) -> Self {
        let mut frame = Vec::with_capacity(FRAME_PREFIX_BYTES + maximum.min(8 * 1024));
        frame.extend_from_slice(&[0_u8; FRAME_PREFIX_BYTES]);
        Self {
            frame,
            maximum,
            exceeded: false,
        }
    }

    /// Serialized bytes so far, excluding the reserved prefix the bound does not count.
    fn payload_len(&self) -> usize {
        self.frame.len() - FRAME_PREFIX_BYTES
    }
}

impl io::Write for BoundedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.payload_len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame overflowed"));
        };
        if length > self.maximum {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame exceeded its limit"));
        }
        self.frame.extend_from_slice(bytes);
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
            limits: limits.validate().map_err(ClientError::Limits)?,
        })
    }

    /// Returns the capabilities, command words, and chat-memory surface visible to one context.
    ///
    /// Without an attestation this is the connected peer's own listing. With one it is the
    /// attested context's, and `Err(ClientError::Remote { code: ERROR_UNAUTHENTICATED, .. })` is
    /// the opaque refusal: this client's peer identity carries no matching attestor grant, the
    /// subject is not mapped, or policy does not let that principal drive that agent. The three
    /// are deliberately indistinguishable here — the broker names the class on its own side of
    /// the socket — so a refused caller cannot learn whether the subject exists.
    ///
    /// The memory surface is present only for a chat-scoped attestation whose three durable-memory
    /// grants are all effective.
    pub async fn session_surface(
        &self,
        attestation: Option<Attestation>,
    ) -> Result<
        (
            Vec<AvailableCapability>,
            Vec<String>,
            Option<ChatMemorySurface>,
            SurfaceEpoch,
        ),
        ClientError,
    > {
        match self
            .exchange(RequestEnvelope::capabilities(attestation))
            .await?
        {
            BrokerResponse::Capabilities {
                capabilities,
                command_words,
                chat_memory,
                surface_epoch,
            } => Ok((capabilities, command_words, chat_memory, surface_epoch)),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::ControlDecision { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::CommandRun { .. }
            | BrokerResponse::Invocation { .. }
            | BrokerResponse::Acknowledged => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Returns the capabilities exact policy makes visible to this authenticated peer.
    ///
    /// The same `capabilities` exchange as [`BrokerClient::session_surface`] with no attestation,
    /// for a caller with no use for command words or a memory surface.
    pub async fn capabilities(&self) -> Result<Vec<AvailableCapability>, ClientError> {
        Ok(self.session_surface(None).await?.0)
    }

    /// Runs one command word through the broker, carrying the piped value in the request frame.
    ///
    /// The answer is the guest's own: a proposal to submit, text it rendered with an exit status,
    /// or a decline the caller reports as a usage error. An attestation, when supplied, must be
    /// honored before any word runs; a refused claim answers exactly as an unknown word does,
    /// because naming the word would disclose the surface the refusal withheld.
    ///
    /// The piped value is bounded on this side by [`FrameLimits::max_frame_bytes`]: an oversized
    /// one fails as [`ClientError::Protocol`] in the request phase before a byte reaches the
    /// socket, and by the broker host's own input bound on the other side.
    pub async fn run_command(
        &self,
        attestation: Option<Attestation>,
        word: String,
        argv: Vec<String>,
        stdin: Option<String>,
    ) -> Result<CommandRunOutcome, ClientError> {
        match self
            .exchange(RequestEnvelope::run_command(attestation, word, argv, stdin))
            .await?
        {
            BrokerResponse::CommandRun { result } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::ControlDecision { .. }
            | BrokerResponse::Capabilities { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::Invocation { .. }
            | BrokerResponse::Acknowledged => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submits one invocation proposal, optionally on behalf of an external subject.
    ///
    /// The attestation binds to the proposal's own identifier here, so a caller cannot construct a
    /// frame whose claim and proposal disagree.
    pub async fn invoke(
        &self,
        attestation: Option<Attestation>,
        request: InvocationRequest,
    ) -> Result<InvocationResult, ClientError> {
        let attestation = attestation.map(|claim| claim.bound_to(request.id.clone()));
        match self
            .exchange(RequestEnvelope::invoke(attestation, request))
            .await?
        {
            BrokerResponse::Invocation { result } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::ControlDecision { .. }
            | BrokerResponse::Capabilities { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::CommandRun { .. }
            | BrokerResponse::Acknowledged => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submits exactly one model-hidden post-acceptance record request.
    pub async fn record_delivered_turn(
        &self,
        attestation: Attestation,
        turn: DeliveredTurnRequest,
    ) -> Result<InvocationResult, ClientError> {
        let attestation = attestation.bound_to(turn.id.clone());
        match self
            .exchange(RequestEnvelope::record_delivered_turn(attestation, turn))
            .await?
        {
            BrokerResponse::Invocation { result } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::ControlDecision { .. }
            | BrokerResponse::Capabilities { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::CommandRun { .. }
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
            BrokerResponse::ControlDecision { .. }
            | BrokerResponse::Capabilities { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::CommandRun { .. }
            | BrokerResponse::Invocation { .. } => Err(ClientError::UnexpectedResponse),
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
            BrokerResponse::ControlDecision { .. }
            | BrokerResponse::Capabilities { .. }
            | BrokerResponse::CommandResolution { .. }
            | BrokerResponse::CommandRun { .. }
            | BrokerResponse::Invocation { .. } => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn exchange(&self, request: RequestEnvelope) -> Result<BrokerResponse, ClientError> {
        validate_socket_path(&self.socket, self.expected_server_uid).await?;
        #[allow(
            clippy::map_err_ignore,
            reason = "tokio's Elapsed says only that io_timeout expired, which ClientError::ConnectTimeout already states"
        )]
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
        // The phase is the whole point: everything up to and including this write leaves the
        // broker with no request to act on, and everything after it leaves this client unable to
        // say whether the request was acted on.
        write_frame(&mut stream, &request, self.limits)
            .await
            .map_err(|source| ClientError::Protocol {
                phase: ExchangePhase::Request,
                source,
            })?;
        let response = read_frame::<_, ResponseEnvelope>(&mut stream, self.limits)
            .await
            .map_err(|source| ClientError::Protocol {
                phase: ExchangePhase::Response,
                source,
            })?;
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
    /// A control scope, sequence or attestation was structurally malformed.
    #[error("control request binding is invalid")]
    InvalidControl,
    /// All four job attempts have been spent (including denials).
    #[error("control attempt budget exhausted")]
    ControlAttempts,
    /// An uncertain or cancelled prior exchange permanently fenced this client.
    #[error("control client is fenced")]
    ControlFenced,
    /// A broker restart invalidated the active job's authority surface.
    #[error("broker surface changed; stop the active job")]
    SurfaceChanged,
    /// The sole pending proposal did not match every echoed response field.
    #[error("control response binding mismatch")]
    ControlBinding,
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
    /// Configured frame bounds were rejected before any connection was attempted.
    #[error("broker client limits are invalid: {0}")]
    Limits(#[source] ProtocolError),
    /// Bounded framing failed during one half of an exchange.
    ///
    /// `phase` carries the resubmission-safety distinction: see [`ExchangePhase`] and
    /// [`ClientError::may_have_executed`] before retrying anything that writes.
    #[error("broker {phase} framing failed: {source}")]
    Protocol {
        /// Which half of the exchange failed.
        phase: ExchangePhase,
        /// Bounded framing failure. Its message names no path and carries no payload content.
        #[source]
        source: ProtocolError,
    },
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

/// Which half of one broker exchange a bounded framing failure belongs to.
///
/// This is the client-local twin of the wire's [`ERROR_BROKER_UNAVAILABLE`] /
/// [`ERROR_OUTCOME_UNAUDITED`] split. Nothing ties a client's `io_timeout` to the broker's own
/// execution deadlines, so a proposal that ran just under the client deadline is delivered,
/// possibly complete, and unreadable — indistinguishable at the socket from one that never left.
/// Collapsing both into one error is what lets a caller resubmit an external write it already made.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExchangePhase {
    /// Serializing or writing the request failed, so no complete request frame was delivered.
    ///
    /// Nothing executed. The same work may be resubmitted under a fresh invocation identifier,
    /// matching [`ERROR_BROKER_UNAVAILABLE`].
    Request,
    /// The complete request frame was delivered and reading its response failed.
    ///
    /// The broker may have executed the request. Treat this exactly like
    /// [`ERROR_OUTCOME_UNAUDITED`]: the work must **not** be resubmitted under any identifier,
    /// because replay rejection keys on the invocation identifier and a fresh one duplicates a
    /// non-idempotent external effect. The broker's audit log is the only record of what happened.
    Response,
}

#[cfg(unix)]
impl fmt::Display for ExchangePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Request => "request",
            Self::Response => "response",
        })
    }
}

/// The stable kind of one broker-client failure, for telemetry and checkpointed records.
///
/// One definition of these names. A client failure reaches an operator through several surfaces —
/// an unobserved-command audit record, a control transition's checkpointed outcome, a session's
/// failure event — and a category token invented separately at each of them is a category that
/// silently disagrees with itself. Every consumer maps [`ClientError`] here and prints
/// [`ClientErrorKind::as_str`].
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientErrorKind {
    /// The socket path could not be inspected.
    SocketMetadata,
    /// The socket failed its ownership or mode check.
    UnsafeSocket,
    /// Connecting to the socket timed out.
    ConnectTimeout,
    /// Connecting to the socket failed.
    Connect,
    /// The peer credentials of the connected socket could not be read.
    PeerCredentials,
    /// The server's identity did not match what this client requires.
    ServerIdentity,
    /// A bound on the exchange — frame size, response size — was exceeded.
    Limits,
    /// Framing, encoding, or decoding failed on one half of the exchange.
    Protocol,
    /// The broker answered with an error envelope.
    Remote,
    /// The broker answered a response variant this request cannot consume.
    UnexpectedResponse,
    /// A control request was malformed or out of order before transmission.
    InvalidControl,
    /// The job's control attempt budget is spent.
    ControlAttempts,
    /// This control client is permanently fenced.
    ControlFenced,
    /// The broker's surface epoch changed under the session.
    SurfaceChanged,
    /// The control decision did not bind to the proposal that was sent.
    ControlBinding,
}

#[cfg(unix)]
impl ClientErrorKind {
    /// The stable token for this kind, as telemetry and audit records spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SocketMetadata => "socket-metadata",
            Self::UnsafeSocket => "unsafe-socket",
            Self::ConnectTimeout => "connect-timeout",
            Self::Connect => "connect",
            Self::PeerCredentials => "peer-credentials",
            Self::ServerIdentity => "server-identity",
            Self::Limits => "limits",
            Self::Protocol => "protocol",
            Self::Remote => "remote",
            Self::UnexpectedResponse => "unexpected-response",
            Self::InvalidControl => "invalid-control",
            Self::ControlAttempts => "control-attempts",
            Self::ControlFenced => "control-fenced",
            Self::SurfaceChanged => "surface-changed",
            Self::ControlBinding => "control-binding",
        }
    }
}

#[cfg(unix)]
impl fmt::Display for ClientErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(unix)]
impl ClientError {
    /// The stable kind of this failure, for telemetry that must not carry the message itself.
    #[must_use]
    pub fn kind(&self) -> ClientErrorKind {
        match self {
            Self::SocketMetadata { .. } => ClientErrorKind::SocketMetadata,
            Self::UnsafeSocket => ClientErrorKind::UnsafeSocket,
            Self::ConnectTimeout => ClientErrorKind::ConnectTimeout,
            Self::Connect { .. } => ClientErrorKind::Connect,
            Self::PeerCredentials { .. } => ClientErrorKind::PeerCredentials,
            Self::ServerIdentity { .. } => ClientErrorKind::ServerIdentity,
            Self::Limits(_) => ClientErrorKind::Limits,
            Self::Protocol { .. } => ClientErrorKind::Protocol,
            Self::Remote { .. } => ClientErrorKind::Remote,
            Self::UnexpectedResponse => ClientErrorKind::UnexpectedResponse,
            Self::InvalidControl => ClientErrorKind::InvalidControl,
            Self::ControlAttempts => ClientErrorKind::ControlAttempts,
            Self::ControlFenced => ClientErrorKind::ControlFenced,
            Self::SurfaceChanged => ClientErrorKind::SurfaceChanged,
            Self::ControlBinding => ClientErrorKind::ControlBinding,
        }
    }

    /// Reports whether the broker may have executed the request this failure ended.
    ///
    /// `true` means the complete request frame was delivered and this client could not establish
    /// the outcome. For an operation that writes — `invoke`, attested or not, and
    /// `recordDeliveredTurn` — the external effect may already have taken place, so the
    /// caller must surface a non-retryable failure rather than resubmit under a fresh identifier.
    /// For a read-only operation it is informational and re-asking is harmless.
    #[must_use]
    pub fn may_have_executed(&self) -> bool {
        match self {
            Self::Protocol { phase, .. } => *phase == ExchangePhase::Response,
            Self::Remote { code, .. } => code == ERROR_OUTCOME_UNAUDITED,
            // The request was delivered in full and the broker answered something this client
            // cannot interpret, which is exactly "delivered, outcome unknown".
            Self::UnexpectedResponse => true,
            _ => false,
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Matched rather than written from the constant so a second variant cannot silently
        // inherit the first one's identifier while serializing correctly.
        formatter.write_str(match self {
            Self::V1Alpha3 => PROTOCOL_VERSION,
        })
    }
}

/// Tier of the broker socket discovery precedence that produced a path.
///
/// The tier is telemetry-safe where the path is not: a socket path is excluded from every signal,
/// but "which tier answered" is exactly what a connection investigation needs.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerSocketTier {
    /// A caller-supplied path, such as a command-line flag or a configuration field.
    Explicit,
    /// `DEKOPON_BROKER_SOCKET`.
    Environment,
    /// `$XDG_RUNTIME_DIR/dekopon/broker.sock`.
    XdgRuntimeDir,
    /// `$HOME/.local/run/dekopon/broker.sock`.
    Home,
}

#[cfg(unix)]
impl BrokerSocketTier {
    /// Stable low-cardinality label for telemetry and diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Environment => "environment",
            Self::XdgRuntimeDir => "xdg-runtime-dir",
            Self::Home => "home",
        }
    }
}

#[cfg(unix)]
impl fmt::Display for BrokerSocketTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// One resolved broker socket and the discovery tier that produced it.
#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedBrokerSocket {
    path: PathBuf,
    tier: BrokerSocketTier,
}

#[cfg(unix)]
impl ResolvedBrokerSocket {
    /// The resolved path. It is never probed for existence; see [`BrokerSocketDiscovery`].
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consumes this resolution and returns the owned path.
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        self.path
    }

    /// Which precedence tier answered.
    #[must_use]
    pub const fn tier(&self) -> BrokerSocketTier {
        self.tier
    }
}

/// Inputs used to resolve the broker socket precedence.
///
/// This is the one definition of that precedence. `dekopon-run`, `dekopond`, and the operator
/// console all consult it, so a socket a client finds here is the socket the documentation
/// describes, and a change lands in one place rather than three that must be kept in step.
///
/// Unlike configuration discovery, no candidate is probed for existence: a broker socket is absent
/// whenever the daemon is not running, so the tightest resolved tier is always trusted and
/// connection failures are reported against that exact path.
///
/// [`Self::resolve`] answers `None` rather than an error because "no tier applied" means something
/// different to each caller — a usage failure to one, a configuration failure to another — and each
/// owns the wording an operator acts on.
#[cfg(unix)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrokerSocketDiscovery {
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    home: Option<PathBuf>,
}

#[cfg(unix)]
impl BrokerSocketDiscovery {
    /// Captures discovery inputs from the current process.
    ///
    /// An environment variable exported with an empty value is ignored rather than resolved to an
    /// empty path, matching configuration discovery elsewhere: an empty export is an unset
    /// variable that happens to exist.
    #[must_use]
    pub fn from_process(explicit: Option<PathBuf>) -> Self {
        Self {
            explicit,
            environment: env::var_os("DEKOPON_BROKER_SOCKET")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            xdg_runtime_dir: env::var_os("XDG_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            home: env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Creates an injectable discovery context, for deterministic tests in any consuming crate.
    #[must_use]
    pub const fn new(
        explicit: Option<PathBuf>,
        environment: Option<PathBuf>,
        xdg_runtime_dir: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            explicit,
            environment,
            xdg_runtime_dir,
            home,
        }
    }

    /// Resolves the highest-precedence broker socket, or `None` when no tier applies.
    #[must_use]
    pub fn resolve(&self) -> Option<ResolvedBrokerSocket> {
        if let Some(path) = &self.explicit {
            return Some(ResolvedBrokerSocket {
                path: path.clone(),
                tier: BrokerSocketTier::Explicit,
            });
        }
        if let Some(path) = &self.environment {
            return Some(ResolvedBrokerSocket {
                path: path.clone(),
                tier: BrokerSocketTier::Environment,
            });
        }
        if let Some(root) = &self.xdg_runtime_dir {
            return Some(ResolvedBrokerSocket {
                path: root.join("dekopon/broker.sock"),
                tier: BrokerSocketTier::XdgRuntimeDir,
            });
        }
        if let Some(home) = &self.home {
            return Some(ResolvedBrokerSocket {
                path: home.join(".local/run/dekopon/broker.sock"),
                tier: BrokerSocketTier::Home,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests;
