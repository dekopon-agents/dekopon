//! Deny-by-default authorization, execution, evidence, and audit core for Dekopon.
//!
//! This crate is broker-owned machinery. A transport supplies an [`AuthenticatedContext`] derived
//! from trusted peer identity, never payload claims. [`Broker`] asks a `dekopon-policy`
//! [`PolicyEngine`] whether that context may act, binds an allow to the owner-authored
//! [`ConstraintSet`] for the requested capability, creates a single-use authorization, executes it
//! through `dekopon-broker-host`, and records metadata-only hash-linked audit events.
//!
//! Authorization and execution constraints are deliberately separate concerns. Cedar decides *who
//! may do what*; the constraint catalog decides *how narrowly the broker will then do it*, and it
//! is validated against loaded provider manifests, the component host's own ceilings, and the
//! credential store at startup. A policy edit therefore cannot widen a timeout, an output ceiling,
//! an HTTP destination, or a credential binding.
//!
//! Trusted context is intentionally not deserializable from a request payload:
//!
//! ```compile_fail
//! use dekopon_broker::AuthenticatedContext;
//! use serde::de::DeserializeOwned;
//!
//! fn require_deserializable<T: DeserializeOwned>() {}
//!
//! fn main() {
//!     require_deserializable::<AuthenticatedContext>();
//! }
//! ```

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    io::{self, SeekFrom},
    ops::ControlFlow,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use async_trait::async_trait;
use dekopon_broker_host::{
    BoundCredential, BrokerHostError, BrokerProviderRegistry, CommandResolution, HttpCallEvidence,
    ProviderCapability,
};
pub use dekopon_broker_protocol::{
    AvailableCapability, ChatAttestation, ChatMemorySurface, ChatScopeClaim, ChatSessionClaim,
    ChatTransportKind, DeliveredTurnRequest, DeliveryIdentity, InvocationRequest,
    SubjectAttestation,
};
use dekopon_capability::{
    AuthorizationError, DecisionReference, EffectKind, Evidence, ExecutionConstraints,
    HttpConstraintsError, Idempotency, InvocationOutcome, InvocationResult, ProposedInvocation,
    SecretUseGrant, StorageAccess, StorageInterface, StorageNamespace, broker::AuthorizationGate,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, ProviderId,
    RiskLevel, SecretBytes, SecretDrn, SecretSinkKind, SecretUseProposal, SubjectError,
    SubjectService, TraceId,
};
pub use dekopon_policy::{AGENT_PROMPT_ACTION, PolicyBuildError, PolicyEngine, PolicyWorld};
use dekopon_policy::{PolicyContext, PolicyDecision, PolicyRequest, PolicyTarget};
use dekopon_storage_host::{
    ContinuityPolicy, StorageEvidence, StorageGrantPreparation, StorageGrantRequest,
    StorageScopeCommitment,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _, BufReader},
    sync::Mutex,
};
use tracing::Instrument as _;

const MAX_POLICY_REVISION_BYTES: usize = 256;
const MAX_POLICY_SCOPE_ENTRIES: usize = 64;
/// Maximum owner-authored secret-use bindings one broker retains.
pub const MAX_SECRET_BINDINGS: usize = 1024;
const AUDIT_HASH_DOMAIN: &[u8] = b"dekopon-audit-record-v1\0";
const EVIDENCE_HASH_DOMAIN: &[u8] = b"dekopon-evidence-v1\0";
const POLICY_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.policy-decision+json";
const PROVIDER_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.provider-response+json";
const HTTP_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.http-evidence+json";
const STORAGE_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.storage-evidence+json";

pub const MEMORY_PROVIDER: &str = "memory-chat";
pub const MEMORY_WORD: &str = "memory";
pub const MEMORY_RECORD: &str = "memory.chat.record";
pub const MEMORY_RECENT: &str = "memory.chat.recent";
pub const MEMORY_SEARCH: &str = "memory.chat.search";
/// Conservative complete line bound for broker-curated HMAC dedup records.
///
/// The current canonical JSON is 227 bytes including LF. Keeping explicit headroom decouples
/// composition safety from incidental serde field formatting while remaining a fixed trusted
/// bound rather than a guest claim.
const MEMORY_DEDUP_LINE_BYTES: u64 = 256;
/// Exact minimum canonical turn line with empty text and broker-generated HMAC fields.
const MEMORY_MIN_TURN_LINE_BYTES: u64 = 251;
/// SDK success/failure envelope around the provider's already-bounded result value.
const MEMORY_PROVIDER_OUTPUT_OVERHEAD_BYTES: u64 = 1_024;
/// Curated record/query fields and worst-case JSON string escaping at the component boundary.
const MEMORY_PROVIDER_INPUT_OVERHEAD_BYTES: u64 = 4 * 1024;
const MEMORY_QUERY_JSON_EXPANSION: u64 = 6;
/// Raw/decoded collections, canonical-ABI copies, allocator metadata, and component static state.
const MEMORY_WORKING_SET_OVERHEAD_BYTES: u64 = 4 * 1024 * 1024;
/// Smallest serialized `{"turns":[],"truncated":false}` result.
const MEMORY_MIN_RESULT_BYTES: u64 = 30;
/// The provider owns exactly the turn and permanent-dedup logical files.
const MEMORY_LOGICAL_FILES: u64 = 2;
/// Size/read calls for both files plus two appends and the worst-case replacement.
const MEMORY_RECORD_FIXED_HOST_CALLS: u64 = 5;
/// Fixed setup/serde headroom plus a conservative instruction allowance per processed byte.
const MEMORY_FUEL_BASE: u64 = 10_000_000;
const MEMORY_FUEL_PER_WORK_BYTE: u64 = 256;
/// Fixed JSONL chunk requested by the generated memory provider.
const MEMORY_READ_CHUNK_BYTES: u64 = 256 * 1024;

/// Broker-owned bounds for the optional all-or-nothing durable chat-memory surface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatMemoryConfig {
    #[serde(default)]
    pub continuity_policy: ContinuityPolicy,
    pub enabled_agents: Vec<AgentId>,
    pub max_lookback_turns: u32,
    pub max_recent_turns: u32,
    pub max_search_results: u32,
    pub max_query_bytes: u64,
    pub max_result_bytes: u64,
    pub max_turn_bytes: u64,
    pub max_dedup_records: u64,
    pub max_dedup_bytes: u64,
    pub compaction_target_bytes: u64,
    pub compaction_threshold_bytes: u64,
}

impl ChatMemoryConfig {
    /// Validates cross-file storage and memory bounds with checked arithmetic.
    pub fn validate(
        &self,
        storage: &dekopon_storage_host::StorageLimits,
    ) -> Result<(), BrokerBuildError> {
        let positive = [
            u64::from(self.max_lookback_turns),
            u64::from(self.max_recent_turns),
            u64::from(self.max_search_results),
            self.max_query_bytes,
            self.max_result_bytes,
            self.max_turn_bytes,
            self.max_dedup_records,
            self.max_dedup_bytes,
            self.compaction_target_bytes,
            self.compaction_threshold_bytes,
        ];
        let unique_agents = self.enabled_agents.iter().collect::<BTreeSet<_>>();
        if self.enabled_agents.is_empty()
            || unique_agents.len() != self.enabled_agents.len()
            || positive.contains(&0)
            || self.max_recent_turns > self.max_lookback_turns
            || self.max_search_results > self.max_lookback_turns
            || self.compaction_target_bytes >= self.compaction_threshold_bytes
            || self.compaction_threshold_bytes > storage.max_file_bytes
            || self.max_turn_bytes < MEMORY_MIN_TURN_LINE_BYTES
            || self.max_result_bytes < MEMORY_MIN_RESULT_BYTES
            || self.max_dedup_bytes < MEMORY_DEDUP_LINE_BYTES
        {
            return Err(BrokerBuildError::InvalidChatMemory);
        }
        let retained = u64::from(self.max_lookback_turns)
            .checked_mul(self.max_turn_bytes)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        // `read-file` asks for a full CHUNK even on its final partial call, and the host
        // intentionally charges the requested bound. Round both files independently so a valid
        // near-threshold store cannot become unreadable only because its length is not aligned.
        let dedup_read_budget = round_up(self.max_dedup_bytes, MEMORY_READ_CHUNK_BYTES)?;
        let turns_read_budget = round_up(self.compaction_threshold_bytes, MEMORY_READ_CHUNK_BYTES)?;
        let read_budget = dedup_read_budget
            .checked_add(turns_read_budget)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let host_calls = dedup_read_budget
            .checked_div(MEMORY_READ_CHUNK_BYTES)
            .and_then(|value| {
                value.checked_add(turns_read_budget.checked_div(MEMORY_READ_CHUNK_BYTES)?)
            })
            .and_then(|value| value.checked_add(MEMORY_RECORD_FIXED_HOST_CALLS))
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let write_budget = self
            .compaction_target_bytes
            // One record appends a bounded turn and one fixed-shape permanent dedup line before
            // an optional replacement. Account each independently rather than assuming one bound
            // happens to dominate the other.
            .checked_add(self.max_turn_bytes)
            .and_then(|value| value.checked_add(MEMORY_DEDUP_LINE_BYTES))
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let threshold_with_append = self
            .compaction_threshold_bytes
            .checked_add(self.max_turn_bytes)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let namespace_headroom = self
            // Immediately before compaction, the old turn file may sit just below the high
            // threshold and then receive one maximum turn. Its staged replacement can approach
            // the target. Using two thresholds is conservative for the replacement, but the
            // post-append maximum turn is independent and must not disappear into entry overhead.
            .compaction_threshold_bytes
            .checked_mul(2)
            .and_then(|value| value.checked_add(self.max_turn_bytes))
            .and_then(|value| value.checked_add(self.max_dedup_bytes.checked_mul(2)?))
            // Generation/transaction directories, files, markers, manifest, and replacement
            // temporaries. The fixed memory transaction has only two logical files; 32 logical
            // entry charges conservatively cover its canonical manifest as well.
            .and_then(|value| value.checked_add(32 * 4_096))
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        if retained > self.compaction_target_bytes
            || MEMORY_READ_CHUNK_BYTES > storage.max_read_bytes_per_call
            || self.max_turn_bytes > storage.max_write_bytes_per_call
            || MEMORY_DEDUP_LINE_BYTES > storage.max_write_bytes_per_call
            || self.compaction_target_bytes > storage.max_write_bytes_per_call
            || self.max_dedup_bytes > storage.max_file_bytes
            || threshold_with_append > storage.max_file_bytes
            || read_budget > storage.max_read_bytes_per_invocation
            || write_budget > storage.max_write_bytes_per_invocation
            || host_calls > storage.max_host_calls_per_invocation
            || storage.max_files_per_namespace < MEMORY_LOGICAL_FILES
            || namespace_headroom > storage.max_namespace_bytes
            || self.max_query_bytes > 256 * 1024
            || self.max_result_bytes > 1024 * 1024
        {
            return Err(BrokerBuildError::InvalidChatMemory);
        }
        Ok(())
    }

    fn maximum_provider_input_bytes(&self) -> Result<u64, BrokerBuildError> {
        let record = self
            .max_turn_bytes
            .checked_add(MEMORY_PROVIDER_INPUT_OVERHEAD_BYTES)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let search = self
            .max_query_bytes
            .checked_mul(MEMORY_QUERY_JSON_EXPANSION)
            .and_then(|value| value.checked_add(MEMORY_PROVIDER_INPUT_OVERHEAD_BYTES))
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        Ok(record.max(search))
    }

    fn maximum_provider_working_set_bytes(&self) -> Result<u64, BrokerBuildError> {
        self.compaction_threshold_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(self.max_dedup_bytes.checked_mul(2)?))
            .and_then(|bytes| bytes.checked_add(self.compaction_target_bytes.checked_mul(2)?))
            .and_then(|bytes| bytes.checked_add(self.max_turn_bytes))
            .and_then(|bytes| bytes.checked_add(self.max_result_bytes))
            .and_then(|bytes| bytes.checked_add(MEMORY_WORKING_SET_OVERHEAD_BYTES))
            .ok_or(BrokerBuildError::InvalidChatMemory)
    }

    fn minimum_provider_fuel(&self) -> Result<u64, BrokerBuildError> {
        let record_work = self
            .max_dedup_bytes
            .checked_add(self.compaction_threshold_bytes)
            .and_then(|value| value.checked_add(self.compaction_target_bytes))
            .and_then(|value| value.checked_add(self.max_turn_bytes))
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let search_work = self
            .compaction_threshold_bytes
            .checked_add(self.max_query_bytes)
            .and_then(|value| value.checked_add(self.max_result_bytes))
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        record_work
            .max(search_work)
            .checked_mul(MEMORY_FUEL_PER_WORK_BYTE)
            .and_then(|value| value.checked_add(MEMORY_FUEL_BASE))
            .ok_or(BrokerBuildError::InvalidChatMemory)
    }

    /// Validates the memory algorithm's serialized-input, result, working-set, and fuel needs
    /// against the independent component-host ceilings.
    pub fn validate_host_limits(
        &self,
        host: &dekopon_broker_host::BrokerHostLimits,
    ) -> Result<(), BrokerBuildError> {
        let max_input = u64::try_from(host.max_input_bytes).unwrap_or(u64::MAX);
        let max_output = u64::try_from(host.max_output_bytes).unwrap_or(u64::MAX);
        let max_memory = u64::try_from(host.max_memory_bytes).unwrap_or(u64::MAX);
        let provider_output = self
            .max_result_bytes
            .checked_add(MEMORY_PROVIDER_OUTPUT_OVERHEAD_BYTES)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        if self.maximum_provider_input_bytes()? > max_input
            || provider_output > max_output
            || self.maximum_provider_working_set_bytes()? > max_memory
            || self.minimum_provider_fuel()? > host.fuel
        {
            return Err(BrokerBuildError::InvalidChatMemory);
        }
        Ok(())
    }

    #[must_use]
    pub fn enabled_for(&self, agent: &AgentId) -> bool {
        self.enabled_agents.contains(agent)
    }
}

fn round_up(value: u64, multiple: u64) -> Result<u64, BrokerBuildError> {
    value
        .checked_add(multiple.saturating_sub(1))
        .map(|value| value / multiple * multiple)
        .ok_or(BrokerBuildError::InvalidChatMemory)
}

/// Default maximum owner-authored constraint sets in one broker instance.
pub const DEFAULT_MAX_CONSTRAINT_SETS: usize = 1_024;
/// Default process-lifetime invocation identifiers retained for replay rejection.
pub const DEFAULT_MAX_REPLAY_IDS: usize = 100_000;
/// Default maximum records retained by an in-memory or durable audit log.
pub const DEFAULT_MAX_AUDIT_RECORDS: usize = 200_000;
/// Default maximum serialized bytes in one durable JSONL audit record (64 KiB).
pub const DEFAULT_MAX_AUDIT_LINE_BYTES: usize = 64 * 1024;

/// Identity established by a trusted broker transport.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatedContext {
    principal: PrincipalId,
    actor: Actor,
    /// The authenticated attestor peer this context was derived through, when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<PrincipalId>,
    /// The canonical external subject an attested context stands for.
    #[serde(skip_serializing_if = "Option::is_none")]
    attested_subject: Option<ExternalSubject>,
    /// Invocation-authorized chat transport scope; absent for every legacy operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_scope: Option<ChatScopeClaim>,
}

impl AuthenticatedContext {
    /// Binds a transport-authenticated principal to its trusted actor identity.
    ///
    /// Human and service actors must carry the same principal established by the transport.
    /// Agent actors may be represented by an authenticated daemon/service principal and are
    /// therefore bound by an explicit exact policy rule instead.
    pub fn new(principal: PrincipalId, actor: Actor) -> Result<Self, ContextError> {
        Self::build(principal, actor, None, None, None)
    }

    /// Binds a broker-mapped principal derived through an authenticated attestor peer.
    ///
    /// `via` is the attestor's own transport principal, and it is the deny-by-default hinge for
    /// attested authority: policy rules match `via` exactly, so a rule written for direct peers
    /// (`via` absent) can never authorize an attested context and vice versa. The same
    /// human/service principal-match rule applies as for direct contexts.
    pub fn attested(
        principal: PrincipalId,
        actor: Actor,
        via: PrincipalId,
        subject: ExternalSubject,
    ) -> Result<Self, ContextError> {
        Self::build(principal, actor, Some(via), Some(subject), None)
    }

    /// Binds an invocation-authorized chat scope to an attested context.
    pub fn attested_chat(
        principal: PrincipalId,
        actor: Actor,
        via: PrincipalId,
        subject: ExternalSubject,
        scope: ChatScopeClaim,
    ) -> Result<Self, ContextError> {
        Self::build(principal, actor, Some(via), Some(subject), Some(scope))
    }

    fn build(
        principal: PrincipalId,
        actor: Actor,
        via: Option<PrincipalId>,
        attested_subject: Option<ExternalSubject>,
        chat_scope: Option<ChatScopeClaim>,
    ) -> Result<Self, ContextError> {
        let actor_principal = match &actor {
            Actor::Human { principal } | Actor::Service { principal } => Some(principal),
            Actor::Agent { .. } => None,
        };
        if actor_principal.is_some_and(|actor_principal| actor_principal != &principal) {
            return Err(ContextError::PrincipalMismatch);
        }
        Ok(Self {
            principal,
            actor,
            via,
            attested_subject,
            chat_scope,
        })
    }

    /// The peer's own context annotated with a subject whose attestation was refused.
    ///
    /// Used to attribute an `attestation-denied` or `unmapped-subject` decision: no trusted
    /// mapping happened, so the decision belongs to the connecting peer, but the canonical
    /// subject it claimed is still route metadata worth auditing.
    #[must_use]
    fn with_refused_subject(&self, subject: ExternalSubject) -> Self {
        Self {
            principal: self.principal.clone(),
            actor: self.actor.clone(),
            via: None,
            attested_subject: Some(subject),
            chat_scope: None,
        }
    }

    /// Returns the authenticated peer principal.
    #[must_use]
    pub fn principal(&self) -> &PrincipalId {
        &self.principal
    }

    /// Returns the actor bound by trusted workload mapping.
    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// Returns the attestor peer this context was derived through, when it was.
    #[must_use]
    pub fn via(&self) -> Option<&PrincipalId> {
        self.via.as_ref()
    }

    /// Returns the canonical external subject this context stands for, when attested.
    #[must_use]
    pub fn attested_subject(&self) -> Option<&ExternalSubject> {
        self.attested_subject.as_ref()
    }

    /// Returns the trusted chat scope, only for new chat operations.
    #[must_use]
    pub fn chat_scope(&self) -> Option<&ChatScopeClaim> {
        self.chat_scope.as_ref()
    }
}

/// Invalid trusted identity binding.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ContextError {
    /// A human/service payload identity disagreed with transport authentication.
    #[error("authenticated principal does not match the human or service actor")]
    PrincipalMismatch,
}

/// Owner-authored execution constraints for one capability.
///
/// A constraint set is not a grant. It answers "if some policy permits this capability, how
/// narrowly does the broker execute it": which provider route, which trusted classification, which
/// broker-held credential — for everyone, or per acting agent — and which timeout/output/HTTP
/// bounds. Cedar decides whether anyone may reach it at all.
///
/// Splitting the two is what keeps a policy edit from widening an execution bound. Every field
/// here is validated at construction against the loaded provider manifest, the component host's
/// independent ceilings, and the credential store; none of it is reachable from policy text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConstraintSet {
    /// Expected provider selected by the trusted route.
    pub provider: ProviderId,
    /// Trusted effect classification, which must match the loaded manifest byte for byte.
    pub effect: EffectKind,
    /// Trusted risk classification, which must match the loaded manifest byte for byte.
    pub risk: RiskLevel,
    /// Trusted retry classification, which must match the loaded manifest byte for byte.
    pub idempotency: Idempotency,
    /// Symbolic name of the broker-held credential presented for this capability's HTTP calls.
    ///
    /// Binding is per capability rather than per provider on purpose: the confused-deputy scenario
    /// is "same provider component, different operation", and only the capability knows both. A
    /// set with no credential — and no [`credential_by_agent`](Self::credential_by_agent) entry for
    /// the acting agent — transacts unauthenticated. Construction validates the name against the
    /// credential store and requires every allowed host to sit inside the credential's destination
    /// binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Per-agent overrides of [`credential`](Self::credential), keyed by acting agent.
    ///
    /// This is the second axis of credential binding, and it answers a different question from the
    /// first. `credential` decides which secret an *operation* presents; this decides which secret
    /// a *caller* presents for that operation, so one capability can reach two organizations
    /// through two tokens without being duplicated under a second capability namespace.
    ///
    /// The key is the agent, because that is the identity the deployment already partitions on:
    /// routes name agents, so a per-agent credential is per-team and per-channel scoping for free.
    /// It is also trusted input rather than a caller claim — the agent name arrives in the
    /// [`AuthenticatedContext`] the broker itself derived from an owner-configured attestor grant,
    /// never from an invocation payload. A caller with no agent at all, such as a direct
    /// `dekopon-run` peer carrying [`Actor::Service`], matches no override and takes the default.
    ///
    /// Every override is validated at construction exactly as the default is: the name must exist
    /// in the credential store and its destinations must cover every allowed host of this set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_by_agent: BTreeMap<AgentId, String>,
    /// Execution and optional HTTP authority granted when this capability is permitted.
    pub constraints: ExecutionConstraints,
}

impl ConstraintSet {
    /// Selects the symbolic credential this set presents for one trusted actor.
    ///
    /// An agent actor takes its [`credential_by_agent`](Self::credential_by_agent) entry when the
    /// set declares one, and otherwise the default; every other actor takes the default. `None`
    /// means this invocation transacts unauthenticated, which is what a set with no credential at
    /// all has always meant.
    #[must_use]
    pub fn credential_for(&self, actor: &Actor) -> Option<&str> {
        let selected = match actor {
            Actor::Agent { agent } => self.credential_by_agent.get(agent),
            Actor::Human { .. } | Actor::Service { .. } => None,
        };
        selected.or(self.credential.as_ref()).map(String::as_str)
    }

    /// Every credential this set could ever select, for construction-time validation.
    ///
    /// Validating the reachable set rather than only the default is what keeps an override from
    /// being the one credential nobody proved: a name the store does not hold, or destinations
    /// that do not cover this set's allowed hosts, must refuse startup wherever it appears.
    fn selectable_credentials(&self) -> impl Iterator<Item = &String> {
        self.credential
            .iter()
            .chain(self.credential_by_agent.values())
    }
}

/// Whether startup refuses configuration that cannot apply, or reports it and continues.
///
/// This governs *startup* only. No decision the broker makes afterwards depends on it, and in
/// particular the runtime `unconstrained-capability` denial is unconditional in both modes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Leniency {
    /// Refuse to start. Every mismatch is a [`BrokerBuildError`].
    #[default]
    Strict,
    /// Start anyway, reporting each mismatch as a [`StartupWarning`].
    Tolerant,
}

/// Configuration that could not apply, reported instead of refusing startup.
///
/// Every variant describes something already inert: the broker behaves identically whether the
/// offending configuration is present or absent. The warning exists so an operator learns their
/// config says something the deployment cannot honor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupWarning {
    /// A constraint set named a capability no loaded provider routes, and was dropped.
    UnroutedConstraintSet {
        /// The unrouted capability.
        capability: CapabilityId,
    },
    /// A capability a policy could permit has no constraint set bounding it.
    ///
    /// Every invocation of it is denied `unconstrained-capability` before Cedar is consulted, so
    /// the grant is unreachable rather than unbounded.
    UnconstrainedCapability {
        /// The capability with no constraint set.
        capability: CapabilityId,
    },
}

impl StartupWarning {
    /// Returns the stable machine-readable reason recorded alongside the message.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::UnroutedConstraintSet { .. } => "unrouted-constraint-set",
            Self::UnconstrainedCapability { .. } => "unconstrained-capability",
        }
    }

    /// Returns the capability the warning is about.
    #[must_use]
    pub const fn capability(&self) -> &CapabilityId {
        match self {
            Self::UnroutedConstraintSet { capability }
            | Self::UnconstrainedCapability { capability } => capability,
        }
    }
}

impl fmt::Display for StartupWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnroutedConstraintSet { capability } => write!(
                formatter,
                "constraint set for {capability} names no loaded provider route; it was ignored"
            ),
            Self::UnconstrainedCapability { capability } => write!(
                formatter,
                "policy could permit {capability}, which has no constraint set; every invocation \
                 of it will be denied unconstrained-capability"
            ),
        }
    }
}

/// Every capability this broker knows how to execute, and how.
///
/// A capability with no constraint set is not deployable: the broker refuses it before consulting
/// policy at all. Under [`Leniency::Strict`] it also refuses to start if any policy could ever
/// permit one; under [`Leniency::Tolerant`] that becomes a [`StartupWarning`] and the invocation-
/// time refusal — which is the part that actually enforces anything — is unchanged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConstraintCatalog {
    sets: BTreeMap<CapabilityId, ConstraintSet>,
}

impl ConstraintCatalog {
    /// A catalog holding nothing; every capability invocation then denies
    /// `unconstrained-capability`.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a catalog, rejecting a capability declared twice.
    ///
    /// # Errors
    ///
    /// Returns [`BrokerBuildError::DuplicateConstraintSet`] when one capability appears more than
    /// once. Two constraint sets for one capability would make execution bounds ambiguous.
    pub fn new(
        entries: impl IntoIterator<Item = (CapabilityId, ConstraintSet)>,
    ) -> Result<Self, BrokerBuildError> {
        let mut sets = BTreeMap::new();
        for (capability, set) in entries {
            if sets.insert(capability.clone(), set).is_some() {
                return Err(BrokerBuildError::DuplicateConstraintSet { capability });
            }
        }
        Ok(Self { sets })
    }

    /// Drops every set naming a capability no loaded provider routes, returning what was dropped.
    ///
    /// Used only under [`Leniency::Tolerant`]. Such a set is inert either way — the broker cannot
    /// execute a capability it has no route for — so removing it changes no decision. It exists so
    /// an operator can keep constraint sets for a provider they have not dropped in yet without the
    /// broker refusing to start.
    pub fn retain_routed(&mut self, registry: &BrokerProviderRegistry) -> Vec<CapabilityId> {
        let routed = registry
            .capabilities()
            .map(|(_, capability)| capability.id.clone())
            .collect::<BTreeSet<_>>();
        let dropped = self
            .sets
            .keys()
            .filter(|capability| !routed.contains(*capability))
            .cloned()
            .collect::<Vec<_>>();
        for capability in &dropped {
            self.sets.remove(capability);
        }
        dropped
    }

    /// Returns the constraint set for one capability, if the deployment declared one.
    #[must_use]
    pub fn get(&self, capability: &CapabilityId) -> Option<&ConstraintSet> {
        self.sets.get(capability)
    }

    /// Iterates constraint sets in capability-identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (&CapabilityId, &ConstraintSet)> {
        self.sets.iter()
    }

    /// Number of declared constraint sets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sets.len()
    }

    /// Whether the catalog declares nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    /// Proves every declared set against the loaded routes, host ceilings, and credential store.
    fn validate(
        &self,
        registry: &BrokerProviderRegistry,
        credentials: &CredentialStore,
        maximum: usize,
    ) -> Result<(), BrokerBuildError> {
        if self.sets.len() > maximum {
            return Err(BrokerBuildError::TooManyConstraintSets {
                count: self.sets.len(),
                maximum,
            });
        }
        for (capability_id, set) in &self.sets {
            validate_set_constraints(set)?;
            validate_set_credential(capability_id, set, credentials)?;
            registry
                .validate_constraints(&set.constraints)
                .map_err(|source| BrokerBuildError::HostConstraint { source })?;
            let (provider, capability) = registry
                .capabilities()
                .find(|(_, capability)| &capability.id == capability_id)
                .ok_or_else(|| BrokerBuildError::UnknownCapability {
                    capability: capability_id.clone(),
                })?;
            validate_trusted_metadata(capability_id, set, provider, capability)?;
        }
        Ok(())
    }
}

/// Broker-held credentials resolvable from constraint sets by symbolic name.
///
/// The store is constructed by the deploying process from owner-only storage and handed to the
/// broker whole; constraint sets refer to entries by name only, so serialized configuration never
/// contains secret material. Values inside are [`BoundCredential`]s, whose secrets are `Redacted` end to end.
#[derive(Debug, Default)]
pub struct CredentialStore {
    entries: BTreeMap<String, BoundCredential>,
}

impl CredentialStore {
    /// A store holding no credentials; every credentialed constraint set then fails construction.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a store, rejecting duplicate symbolic names.
    pub fn new(
        entries: impl IntoIterator<Item = (String, BoundCredential)>,
    ) -> Result<Self, BrokerBuildError> {
        let mut store = BTreeMap::new();
        for (name, credential) in entries {
            if store.insert(name.clone(), credential).is_some() {
                return Err(BrokerBuildError::DuplicateCredential { name });
            }
        }
        Ok(Self { entries: store })
    }

    fn get(&self, name: &str) -> Option<&BoundCredential> {
        self.entries.get(name)
    }
}

/// One owner-authored executable binding for a public DRN.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretUseBinding {
    pub binding_id: String,
    pub secret: SecretDrn,
    pub capability: CapabilityId,
    pub sink: SecretSinkKind,
    pub basic_username: Option<String>,
    pub allowed_hosts: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub allowed_paths: Vec<dekopon_capability::HttpPathRule>,
    pub allow_query: bool,
    pub max_injections: u32,
}

impl SecretUseBinding {
    /// Validates the binding's native sink and complete exact scope.
    pub fn validate(&self) -> Result<(), dekopon_capability::SecretUseGrantError> {
        self.grant().validate()
    }

    fn grant(&self) -> SecretUseGrant {
        SecretUseGrant {
            secret: self.secret.clone(),
            sink: self.sink,
            basic_username: self.basic_username.clone(),
            allowed_hosts: self.allowed_hosts.clone(),
            allowed_methods: self.allowed_methods.clone(),
            allowed_paths: self.allowed_paths.clone(),
            allow_query: self.allow_query,
            max_injections: self.max_injections,
            binding_id: self.binding_id.clone(),
            map_revision: None,
        }
    }

    fn proposal(&self) -> SecretUseProposal {
        match self.sink {
            SecretSinkKind::HttpBearer => SecretUseProposal::HttpBearer {
                secret: self.secret.clone(),
            },
            SecretSinkKind::HttpBasic => SecretUseProposal::HttpBasic {
                secret: self.secret.clone(),
                username: self
                    .basic_username
                    .clone()
                    .expect("validated Basic binding always carries a username"),
            },
        }
    }

    fn matches(&self, capability: &CapabilityId, proposal: &SecretUseProposal) -> bool {
        &self.capability == capability
            && &self.secret == proposal.secret()
            && self.sink == proposal.sink()
            && self.basic_username.as_deref() == proposal.username()
    }
}

/// Bounded secret bytes returned only inside the broker process.
#[derive(Clone)]
pub struct SecretMaterial(SecretBytes);

impl SecretMaterial {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(SecretBytes::new(bytes))
    }

    fn into_secret_bytes(self) -> SecretBytes {
        self.0
    }
}

impl fmt::Debug for SecretMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretMaterial([REDACTED])")
    }
}

/// A private-map adapter that resolves one already-authorized DRN snapshot.
#[async_trait]
pub trait SecretResolver: Send + Sync + fmt::Debug {
    async fn resolve(&self, secret: &SecretDrn) -> Result<SecretMaterial, SecretResolutionError>;
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("secret source resolution failed ({category})")]
pub struct SecretResolutionError {
    pub category: &'static str,
}

#[derive(Debug)]
struct EmptySecretResolver;

#[async_trait]
impl SecretResolver for EmptySecretResolver {
    async fn resolve(&self, _secret: &SecretDrn) -> Result<SecretMaterial, SecretResolutionError> {
        Err(SecretResolutionError {
            category: "missing",
        })
    }
}

/// Private-map bindings plus the broker-owned resolver that can materialize them.
pub struct SecretCatalog {
    bindings: Vec<SecretUseBinding>,
    resolver: Arc<dyn SecretResolver>,
    authority_revision: Option<String>,
}

impl fmt::Debug for SecretCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretCatalog")
            .field("bindings", &self.bindings.len())
            .field("resolver", &"[BROKER-PRIVATE]")
            .field("authority_revision", &self.authority_revision)
            .finish()
    }
}

impl Default for SecretCatalog {
    fn default() -> Self {
        Self {
            bindings: Vec::new(),
            resolver: Arc::new(EmptySecretResolver),
            authority_revision: None,
        }
    }
}

impl SecretCatalog {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new(
        bindings: Vec<SecretUseBinding>,
        resolver: Arc<dyn SecretResolver>,
    ) -> Result<Self, BrokerBuildError> {
        if bindings.len() > MAX_SECRET_BINDINGS {
            return Err(BrokerBuildError::TooManySecretBindings {
                count: bindings.len(),
                maximum: MAX_SECRET_BINDINGS,
            });
        }
        let mut ids = BTreeSet::new();
        let mut tuples = BTreeSet::new();
        for binding in &bindings {
            binding.grant().validate().map_err(|source| {
                BrokerBuildError::InvalidSecretBinding {
                    binding: binding.binding_id.clone(),
                    source,
                }
            })?;
            if !ids.insert(binding.binding_id.clone()) {
                return Err(BrokerBuildError::DuplicateSecretBinding {
                    binding: binding.binding_id.clone(),
                });
            }
            let tuple = (
                binding.secret.clone(),
                binding.capability.clone(),
                binding.sink,
                binding.basic_username.clone(),
            );
            if !tuples.insert(tuple) {
                return Err(BrokerBuildError::ConflictingSecretBinding {
                    secret: binding.secret.clone(),
                    capability: binding.capability.clone(),
                });
            }
        }
        Ok(Self {
            bindings,
            resolver,
            authority_revision: None,
        })
    }

    pub fn drns(&self) -> impl Iterator<Item = &SecretDrn> {
        self.bindings.iter().map(|binding| &binding.secret)
    }

    /// Attaches the owner-authored private-map revision used by authority-bound continuity.
    pub fn with_authority_revision(mut self, revision: String) -> Result<Self, BrokerBuildError> {
        if revision.is_empty()
            || revision.len() > 128
            || revision.trim() != revision
            || revision.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(BrokerBuildError::InvalidSecretMapRevision);
        }
        self.authority_revision = Some(revision);
        Ok(self)
    }

    fn grant(&self, binding: &SecretUseBinding) -> SecretUseGrant {
        let mut grant = binding.grant();
        grant.map_revision = self.authority_revision.clone();
        grant
    }

    fn authority_revision(&self) -> Option<&str> {
        self.authority_revision.as_deref()
    }

    fn authority_bindings(&self) -> Vec<&SecretUseBinding> {
        let mut bindings = self.bindings.iter().collect::<Vec<_>>();
        bindings.sort_by(|left, right| left.binding_id.cmp(&right.binding_id));
        bindings
    }

    fn validate(&self, constraints: &ConstraintCatalog) -> Result<(), BrokerBuildError> {
        for binding in &self.bindings {
            let set = constraints.get(&binding.capability).ok_or_else(|| {
                BrokerBuildError::SecretBindingUnknownCapability {
                    binding: binding.binding_id.clone(),
                    capability: binding.capability.clone(),
                }
            })?;
            let http = set.constraints.http.as_ref().ok_or_else(|| {
                BrokerBuildError::SecretBindingWithoutHttp {
                    binding: binding.binding_id.clone(),
                }
            })?;
            if binding.max_injections > http.max_requests
                || binding
                    .allowed_hosts
                    .iter()
                    .any(|host| !http.allowed_hosts.contains(host))
                || binding
                    .allowed_methods
                    .iter()
                    .any(|method| !http.allowed_methods.contains(method))
            {
                return Err(BrokerBuildError::SecretBindingExceedsCapability {
                    binding: binding.binding_id.clone(),
                });
            }
        }
        Ok(())
    }

    fn binding(
        &self,
        capability: &CapabilityId,
        proposal: &SecretUseProposal,
    ) -> Option<&SecretUseBinding> {
        self.bindings
            .iter()
            .find(|binding| binding.matches(capability, proposal))
    }
}

/// Explicit breadth of one owner-authored chat-scope grant.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "breadth",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ChatScopeGrant {
    /// Every canonical channel/conversation on one configured transport.
    TransportWide {
        kind: ChatTransportKind,
        transport: dekopon_core::TransportId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_subject_service: Option<String>,
    },
    /// Every canonical conversation in one exact channel.
    ExactChannel {
        kind: ChatTransportKind,
        transport: dekopon_core::TransportId,
        channel: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_subject_service: Option<String>,
    },
    /// One exact canonical conversation.
    ExactConversation {
        kind: ChatTransportKind,
        transport: dekopon_core::TransportId,
        channel: String,
        conversation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_subject_service: Option<String>,
    },
}

impl fmt::Debug for ChatScopeGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChatScopeGrant([REDACTED])")
    }
}

impl ChatScopeGrant {
    fn validate(&self) -> Result<(), BrokerBuildError> {
        let (kind, scope, local_service) = match self {
            Self::TransportWide {
                kind,
                local_subject_service,
                ..
            } => (*kind, None, local_subject_service),
            Self::ExactChannel {
                kind,
                channel,
                local_subject_service,
                ..
            } => (*kind, Some((channel.as_str(), None)), local_subject_service),
            Self::ExactConversation {
                kind,
                channel,
                conversation,
                local_subject_service,
                ..
            } => (
                *kind,
                Some((channel.as_str(), Some(conversation.as_str()))),
                local_subject_service,
            ),
        };
        if kind == ChatTransportKind::Local {
            let Some(service) = local_service else {
                return Err(BrokerBuildError::InvalidChatScope);
            };
            service
                .parse::<SubjectService>()
                .map_err(|source| BrokerBuildError::InvalidChatScopeService { source })?;
        } else if local_service.is_some() {
            return Err(BrokerBuildError::InvalidChatScope);
        }
        if let Some((channel, conversation)) = scope {
            let claim = ChatScopeClaim {
                transport: self.transport().clone(),
                kind,
                channel: channel.to_owned(),
                conversation: conversation.unwrap_or(channel).to_owned(),
            };
            if !canonical_chat_scope_shape(&claim) {
                return Err(BrokerBuildError::InvalidChatScope);
            }
        }
        Ok(())
    }

    fn transport(&self) -> &dekopon_core::TransportId {
        match self {
            Self::TransportWide { transport, .. }
            | Self::ExactChannel { transport, .. }
            | Self::ExactConversation { transport, .. } => transport,
        }
    }

    fn permits(&self, subject: &ExternalSubject, scope: &ChatScopeClaim) -> bool {
        let (kind, transport, channel, conversation, local_service) = match self {
            Self::TransportWide {
                kind,
                transport,
                local_subject_service,
            } => (*kind, transport, None, None, local_subject_service),
            Self::ExactChannel {
                kind,
                transport,
                channel,
                local_subject_service,
            } => (*kind, transport, Some(channel), None, local_subject_service),
            Self::ExactConversation {
                kind,
                transport,
                channel,
                conversation,
                local_subject_service,
            } => (
                *kind,
                transport,
                Some(channel),
                Some(conversation),
                local_subject_service,
            ),
        };
        kind == scope.kind
            && transport == &scope.transport
            && channel.is_none_or(|value| value == &scope.channel)
            && conversation.is_none_or(|value| value == &scope.conversation)
            && (kind != ChatTransportKind::Local
                || local_service.as_deref() == Some(subject.service().as_str()))
    }
}

/// One peer's authority to attest external subjects, scoped to canonical namespaces.
///
/// Grants belong to owner-controlled deployment configuration, exactly like peer identity
/// mapping. A grant does not confer any capability by itself: it only lets the broker derive an
/// attested context, which still has to match a `via`-scoped policy rule to do anything.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttestorGrant {
    /// Canonical-prefix namespaces this peer may attest, matched on segment boundaries.
    pub namespaces: Vec<String>,
    /// Explicit chat scope authority. Empty preserves legacy subject-only attestation behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chat_scopes: Vec<ChatScopeGrant>,
}

impl AttestorGrant {
    /// Validates namespace grammar: a service name optionally followed by canonical segments.
    pub fn validate(&self) -> Result<(), BrokerBuildError> {
        if self.namespaces.is_empty() || self.namespaces.len() > MAX_POLICY_SCOPE_ENTRIES {
            return Err(BrokerBuildError::InvalidAttestorScope {
                scope: self.namespaces.len().to_string(),
            });
        }
        if self.chat_scopes.len() > MAX_POLICY_SCOPE_ENTRIES {
            return Err(BrokerBuildError::InvalidChatScope);
        }
        for scope in &self.chat_scopes {
            scope.validate()?;
        }
        for scope in &self.namespaces {
            let mut segments = scope.split('.');
            let service = segments.next().unwrap_or_default();
            let service_valid = service.parse::<SubjectService>().is_ok();
            let segments_valid = segments.clone().all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
            if !service_valid || !segments_valid || segments.count() > 2 {
                return Err(BrokerBuildError::InvalidAttestorScope {
                    scope: scope.clone(),
                });
            }
        }
        Ok(())
    }

    /// Whether this grant covers one canonical subject, on segment boundaries.
    #[must_use]
    pub fn permits(&self, subject: &ExternalSubject) -> bool {
        self.namespaces
            .iter()
            .any(|namespace| subject.in_namespace(namespace))
    }

    /// Requires both existing subject namespace authority and one exact/bounded chat grant.
    #[must_use]
    pub fn permits_chat(&self, subject: &ExternalSubject, scope: &ChatScopeClaim) -> bool {
        self.permits(subject)
            && canonical_chat_scope(subject, scope)
            && self
                .chat_scopes
                .iter()
                .any(|grant| grant.permits(subject, scope))
    }
}

/// Owner-controlled mapping from canonical external subjects to stable principals.
///
/// This is the trusted half of chat identity: the transport authenticates *which subject* sent a
/// message, and this directory alone decides *who that is* inside Dekopon. Unmapped subjects
/// resolve to nothing and fail closed; principals are never minted on demand.
#[derive(Debug, Default)]
pub struct IdentityDirectory {
    // Keyed by the subject itself rather than its rendered canonical form: the canonical string is
    // injective over the segments, so the two keys are equivalent, and a lookup on the attested
    // path no longer allocates one just to throw it away.
    mappings: BTreeMap<ExternalSubject, PrincipalId>,
}

impl IdentityDirectory {
    /// A directory with no mappings; every attested proposal then denies `unmapped-subject`.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a directory, rejecting duplicate subjects.
    pub fn new(
        entries: impl IntoIterator<Item = (ExternalSubject, PrincipalId)>,
    ) -> Result<Self, BrokerBuildError> {
        let mut mappings = BTreeMap::new();
        for (subject, principal) in entries {
            if mappings.contains_key(&subject) {
                return Err(BrokerBuildError::DuplicateSubjectMapping {
                    subject: subject.canonical(),
                });
            }
            mappings.insert(subject, principal);
        }
        Ok(Self { mappings })
    }

    /// Resolves one canonical subject to its stable principal.
    #[must_use]
    pub fn resolve(&self, subject: &ExternalSubject) -> Option<&PrincipalId> {
        self.mappings.get(subject)
    }

    /// Iterates the mapped principals, for construction-time policy validation.
    pub fn principals(&self) -> impl Iterator<Item = &PrincipalId> {
        self.mappings.values()
    }
}

/// Independent broker limits for constraint and replay state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrokerLimits {
    /// Maximum owner-authored constraint sets accepted at construction.
    pub max_constraint_sets: usize,
    /// Maximum invocation IDs retained for this process lifetime.
    ///
    /// Size this against `auditMaxRecords` rather than below it. The ledger never evicts, and
    /// restart restores one entry per durable Decision event, so the bound is cumulative across
    /// restarts rather than per process. A denial costs one audit record and one ledger slot,
    /// which means a denial-heavy history exhausts a ledger sized at half the audit budget first
    /// — before the designed [`AuditError::Full`] refusal ever fires. Reaching this bound is
    /// `capacity-exhausted`: permanent, and an operator's problem rather than a client's.
    pub max_replay_ids: usize,
}

impl Default for BrokerLimits {
    fn default() -> Self {
        Self {
            max_constraint_sets: DEFAULT_MAX_CONSTRAINT_SETS,
            max_replay_ids: DEFAULT_MAX_REPLAY_IDS,
        }
    }
}

fn validate_policy_revision(revision: &str) -> Result<(), BrokerBuildError> {
    if revision.trim().is_empty()
        || revision.trim() != revision
        || revision.len() > MAX_POLICY_REVISION_BYTES
    {
        return Err(BrokerBuildError::InvalidPolicyRevision);
    }
    Ok(())
}

/// Renders the trusted routing metadata a policy may condition on.
///
/// Every value comes from the broker's own view of the authenticated context. The proposal's input
/// is deliberately absent, so no policy can be made to depend on a value the caller supplies.
fn policy_context(context: &AuthenticatedContext) -> PolicyContext {
    PolicyContext {
        via: context.via().map(|via| via.as_str().to_owned()),
        subject: context.attested_subject().map(ExternalSubject::canonical),
        agent: match context.actor() {
            Actor::Agent { agent } => Some(agent.as_str().to_owned()),
            Actor::Human { .. } | Actor::Service { .. } => None,
        },
        transport_kind: context.chat_scope().map(|scope| scope.kind.to_string()),
        transport: context
            .chat_scope()
            .map(|scope| scope.transport.to_string()),
        channel: context.chat_scope().map(|scope| scope.channel.clone()),
        conversation: context.chat_scope().map(|scope| scope.conversation.clone()),
    }
}

fn validate_trusted_metadata(
    capability_id: &CapabilityId,
    set: &ConstraintSet,
    provider: &ProviderId,
    capability: &ProviderCapability,
) -> Result<(), BrokerBuildError> {
    if &set.provider != provider {
        return Err(BrokerBuildError::ProviderMismatch {
            capability: capability_id.clone(),
            expected: set.provider.clone(),
            actual: provider.clone(),
        });
    }
    for (field, matches) in [
        ("effect", set.effect == capability.effect),
        ("risk", set.risk == capability.risk),
        ("idempotency", set.idempotency == capability.idempotency),
    ] {
        if !matches {
            return Err(BrokerBuildError::CapabilityMetadataMismatch {
                capability: capability_id.clone(),
                field,
            });
        }
    }
    Ok(())
}

/// Proves every credentialed constraint set's destination binding at construction time.
///
/// The runtime injector refuses destinations outside the binding as defense in depth, but this
/// check is the load-bearing one: with every `allowedHosts` entry required verbatim in the
/// credential's destinations, no authorized request can ever reach the runtime mismatch path.
///
/// It runs over every credential the set can select — the default and each per-agent override —
/// because selection happens per invocation and an unproven override is a credential the first
/// caller to match it would discover at execution time.
fn validate_set_credential(
    capability_id: &CapabilityId,
    set: &ConstraintSet,
    credentials: &CredentialStore,
) -> Result<(), BrokerBuildError> {
    let mut names = set.selectable_credentials().peekable();
    if names.peek().is_none() {
        return Ok(());
    }
    let Some(http) = &set.constraints.http else {
        return Err(BrokerBuildError::CredentialWithoutHttp {
            capability: capability_id.clone(),
        });
    };
    for name in names {
        let credential =
            credentials
                .get(name)
                .ok_or_else(|| BrokerBuildError::UnknownCredential {
                    capability: capability_id.clone(),
                    name: name.clone(),
                })?;
        for host in &http.allowed_hosts {
            if !credential.covers(host) {
                return Err(BrokerBuildError::CredentialDestinationMismatch {
                    capability: capability_id.clone(),
                    name: name.clone(),
                    host: host.clone(),
                });
            }
        }
    }
    Ok(())
}

fn validate_set_constraints(set: &ConstraintSet) -> Result<(), BrokerBuildError> {
    let constraints = &set.constraints;
    if constraints.timeout_ms == 0
        || constraints.max_output_bytes == 0
        || constraints.secret_use.is_some()
        || (constraints.http.is_some() && constraints.storage.is_some())
    {
        return Err(BrokerBuildError::InvalidPolicyConstraints);
    }
    if let Some(storage) = &constraints.storage {
        let valid_effect = matches!(
            (storage.access, set.effect),
            (StorageAccess::ReadOnly, EffectKind::ReadOnly)
                | (StorageAccess::ReadWrite, EffectKind::LocalWrite)
        );
        if !valid_effect || set.effect == EffectKind::ExternalWrite {
            return Err(BrokerBuildError::InvalidPolicyConstraints);
        }
    }
    let Some(http) = &constraints.http else {
        return Ok(());
    };
    // The same grammar the capability gate and the HTTP host enforce. A constraint set this
    // broker accepted but they rejected would authorize calls nothing can serve.
    http.validate()
        .map_err(|source| BrokerBuildError::InvalidHttpConstraints { source })?;
    Ok(())
}

fn is_memory_capability(capability: &CapabilityId) -> bool {
    matches!(
        capability.as_str(),
        MEMORY_RECORD | MEMORY_RECENT | MEMORY_SEARCH
    )
}

/// The model-facing note announcing durable chat memory.
///
/// Shared by the live surface and the startup frame ceiling so the check measures the exact bytes
/// a session would receive.
fn memory_prompt_note(max_lookback_turns: u32) -> String {
    format!(
        "Durable chat memory is available on demand. Use `memory recent --last N` or `memory \
         search --query TEXT`. Searches inspect at most {max_lookback_turns} prior turns. Do not \
         claim recall without retrieving it."
    )
}

fn is_reserved_memory_route(capability: &CapabilityId, set: Option<&ConstraintSet>) -> bool {
    capability.as_str().starts_with("memory.chat.")
        || set.is_some_and(|set| set.provider.as_str() == MEMORY_PROVIDER)
}

fn canonical_chat_scope(subject: &ExternalSubject, scope: &ChatScopeClaim) -> bool {
    let subject_matches = match (scope.kind, subject.service()) {
        (ChatTransportKind::Slack, SubjectService::Slack)
        | (ChatTransportKind::Discord, SubjectService::Discord)
        | (ChatTransportKind::Telegram, SubjectService::Telegram) => true,
        (ChatTransportKind::Whatsapp, SubjectService::Whatsapp) => scope
            .channel
            .rsplit_once(':')
            .is_some_and(|(_, sender)| sender == subject.subject()),
        (ChatTransportKind::Local, _) => true,
        _ => false,
    };
    subject_matches && canonical_chat_scope_shape(scope)
}

fn canonical_chat_scope_shape(scope: &ChatScopeClaim) -> bool {
    if !scope.is_bounded() {
        return false;
    }
    match scope.kind {
        ChatTransportKind::Slack => {
            lowercase_token(&scope.channel)
                && (scope.conversation == scope.channel
                    || scope
                        .conversation
                        .split_once(':')
                        .is_some_and(|(channel, timestamp)| {
                            channel == scope.channel && slack_timestamp(timestamp)
                        }))
        }
        ChatTransportKind::Discord => {
            // A Discord native thread is itself the channel used for routing and replies. There is
            // no second thread identifier, so accepting two different decimals would create an
            // alias for one transport-derived conversation.
            scope.conversation == scope.channel && canonical_unsigned_decimal(&scope.channel)
        }
        ChatTransportKind::Telegram => {
            canonical_signed_decimal(&scope.channel)
                && (scope.conversation == scope.channel
                    || scope
                        .conversation
                        .strip_prefix(&format!("{}:topic:", scope.channel))
                        .is_some_and(canonical_positive_service_decimal))
        }
        ChatTransportKind::Whatsapp => {
            let mut parts = scope.channel.split(':');
            let canonical = parts.next().is_some_and(canonical_meta_decimal)
                && parts.next().is_some_and(canonical_meta_decimal)
                && parts.next().is_some_and(canonical_meta_decimal)
                && parts.next().is_none();
            canonical && scope.conversation == scope.channel
        }
        ChatTransportKind::Local => {
            lowercase_scope_value(&scope.channel) && lowercase_scope_value(&scope.conversation)
        }
    }
}

fn lowercase_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn lowercase_scope_value(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b'_' | b':')
        })
}

fn slack_timestamp(value: &str) -> bool {
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

fn canonical_meta_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
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

/// Failure to construct a coherent broker boundary.
#[derive(Debug, Error)]
pub enum BrokerBuildError {
    /// A broker limit was zero.
    #[error("broker limit {field} must be greater than zero")]
    ZeroLimit {
        /// Invalid field.
        field: &'static str,
    },
    /// Policy revision was empty or over its bound.
    #[error("policy revision must contain at most 256 bytes")]
    InvalidPolicyRevision,
    /// Constraint-set count exceeded the broker ceiling.
    #[error("configuration contains {count} constraint sets; broker maximum is {maximum}")]
    TooManyConstraintSets {
        /// Actual count.
        count: usize,
        /// Maximum count.
        maximum: usize,
    },
    /// A policy could permit a capability the deployment declared no constraint set for.
    ///
    /// Refusing at startup rather than at decision time is the point: a grant nothing knows how to
    /// execute is a configuration mistake, and the alternative is discovering it in a denial log
    /// the first time someone exercises the grant.
    #[error("policy permits capability {capability}, which has no constraint set")]
    UnconstrainedCapability {
        /// Capability referenced by policy with no constraint set.
        capability: CapabilityId,
    },
    /// Verified durable state contained more IDs than the replay ledger can retain.
    #[error("durable replay state exceeds its {maximum}-identifier bound")]
    TooManyReplayIds {
        /// Configured maximum.
        maximum: usize,
    },
    /// One capability was given two constraint sets.
    #[error("configuration duplicates a constraint set for capability {capability}")]
    DuplicateConstraintSet {
        /// Duplicated capability.
        capability: CapabilityId,
    },
    /// A constraint set named no loaded capability.
    #[error("constraint set capability {capability} has no loaded provider route")]
    UnknownCapability {
        /// Unknown capability.
        capability: CapabilityId,
    },
    /// Trusted provider did not match the loaded route.
    #[error(
        "constraint set expected provider {expected} for {capability}, but route selects {actual}"
    )]
    ProviderMismatch {
        /// Capability.
        capability: CapabilityId,
        /// Trusted expected provider.
        expected: ProviderId,
        /// Loaded route provider.
        actual: ProviderId,
    },
    /// Trusted effect/risk/idempotency did not match the component manifest.
    #[error("constraint set metadata {field} does not match loaded capability {capability}")]
    CapabilityMetadataMismatch {
        /// Capability.
        capability: CapabilityId,
        /// Mismatched field.
        field: &'static str,
    },
    /// A constraint set omitted a positive bound or supplied an overbroad scope value.
    #[error("execution constraints are incomplete or overbroad")]
    InvalidPolicyConstraints,
    /// A constraint set's HTTP scope did not satisfy the grammar the gate and HTTP host enforce.
    ///
    /// Separate from [`Self::InvalidPolicyConstraints`] because the grammar names the rule that
    /// refused the set — an empty host list, an overlong scope — and an operator editing a
    /// constraint file needs that rule, not the fact that something was wrong somewhere.
    #[error("http execution constraints are invalid")]
    InvalidHttpConstraints {
        /// The grammar rule that refused the set.
        #[source]
        source: HttpConstraintsError,
    },
    /// A constraint set attempted to exceed the component host's independent ceilings.
    #[error("execution constraints exceed component host ceilings")]
    HostConstraint {
        /// Host validation failure.
        #[source]
        source: BrokerHostError,
    },
    /// A constraint set named a credential the store does not hold.
    #[error("constraint set for {capability} names unknown credential {name:?}")]
    UnknownCredential {
        /// Capability whose constraint set referenced the credential.
        capability: CapabilityId,
        /// Symbolic credential name.
        name: String,
    },
    /// A credentialed constraint set granted no HTTP authority to present the credential over.
    #[error("constraint set for {capability} binds a credential but grants no HTTP authority")]
    CredentialWithoutHttp {
        /// Capability whose constraint set referenced the credential.
        capability: CapabilityId,
    },
    /// A credentialed constraint set allowed a host outside the credential's destination binding.
    ///
    /// Enforcing coverage at construction is what makes a runtime destination/credential
    /// mismatch unreachable: every host the set can authorize is a host the credential is
    /// explicitly bound to.
    #[error(
        "constraint set for {capability} allows host {host:?} outside credential {name:?} \
         destinations"
    )]
    CredentialDestinationMismatch {
        /// Capability whose constraint set referenced the credential.
        capability: CapabilityId,
        /// Symbolic credential name.
        name: String,
        /// The allowed host missing from the credential's destinations.
        host: String,
    },
    /// Two credential entries shared one symbolic name.
    #[error("credential store duplicates name {name:?}")]
    DuplicateCredential {
        /// Duplicated symbolic name.
        name: String,
    },
    #[error("configuration contains {count} secret bindings; broker maximum is {maximum}")]
    TooManySecretBindings { count: usize, maximum: usize },
    #[error("private secret map revision is invalid")]
    InvalidSecretMapRevision,
    #[error("secret binding {binding:?} is invalid")]
    InvalidSecretBinding {
        binding: String,
        #[source]
        source: dekopon_capability::SecretUseGrantError,
    },
    #[error("secret binding identifier {binding:?} is duplicated")]
    DuplicateSecretBinding { binding: String },
    #[error("secret {secret} has conflicting bindings for capability {capability}")]
    ConflictingSecretBinding {
        secret: SecretDrn,
        capability: CapabilityId,
    },
    #[error("secret binding {binding:?} names capability {capability} with no constraint set")]
    SecretBindingUnknownCapability {
        binding: String,
        capability: CapabilityId,
    },
    #[error("secret binding {binding:?} names a capability with no HTTP authority")]
    SecretBindingWithoutHttp { binding: String },
    #[error("secret binding {binding:?} exceeds its capability HTTP constraints")]
    SecretBindingExceedsCapability { binding: String },
    /// An attestor grant named an empty, overbroad, or non-canonical namespace.
    #[error("attestor namespace scope {scope:?} is not a canonical subject prefix")]
    InvalidAttestorScope {
        /// The offending scope (or entry count when the list itself is invalid).
        scope: String,
    },
    /// An owner-authored chat scope grant was noncanonical, overbroad, or malformed.
    #[error("attestor chat scope is invalid")]
    InvalidChatScope,
    /// A local chat scope grant named a service the external subject grammar does not define.
    ///
    /// Separate from [`Self::InvalidChatScope`] because it is the one chat-scope rejection whose
    /// cause is a free-form operator string rather than a structural rule: the source names the
    /// offending value, which is what turns a typo in the owner config into a one-line fix.
    #[error("attestor chat scope names an invalid local subject service")]
    InvalidChatScopeService {
        /// Why the service segment was rejected; its message quotes the offending value.
        #[source]
        source: SubjectError,
    },
    /// Chat-memory bounds or their composition with storage ceilings are invalid.
    #[error("chat-memory bounds do not compose with provider/storage ceilings")]
    InvalidChatMemory,
    /// Two identity mappings named one canonical subject.
    #[error("identity mapping duplicates subject {subject:?}")]
    DuplicateSubjectMapping {
        /// Duplicated canonical subject.
        subject: String,
    },
}

/// Metadata-only event committed to the broker audit chain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum AuditEvent {
    /// Authorization allowed or denied before provider execution.
    Decision {
        /// Invocation identifier.
        invocation: InvocationId,
        /// Trace identifier.
        trace: TraceId,
        /// Authenticated caller principal; omitted for storage-backed records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<PrincipalId>,
        /// Trusted actor; omitted for storage-backed records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<Actor>,
        /// Attestor peer for attested contexts; absent for direct peers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<PrincipalId>,
        /// Canonical external subject for attested (or refused-attestation) proposals.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attested_subject: Option<ExternalSubject>,
        /// Requested capability.
        capability: CapabilityId,
        /// Public DRN proposed for separate authorization, when any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<SecretDrn>,
        /// Native sink proposed with the public DRN.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_sink: Option<SecretSinkKind>,
        /// Selected provider when a rule matched.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<ProviderId>,
        /// Broker principal that owns the authorization transition; omitted for storage records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorized_by: Option<PrincipalId>,
        /// Broker decision identifier.
        decision_id: String,
        /// Evaluated policy revision; omitted for storage records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_revision: Option<String>,
        /// Identifiers of the Cedar policies that determined this decision.
        ///
        /// Empty for a decision no policy reached — a deny-by-default refusal, an attestation
        /// refusal, or a capability with no constraint set — which is itself the explanation.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        policy_ids: Vec<String>,
        /// Fingerprint of the policy set and world this decision was evaluated against.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_digest: Option<String>,
        /// Whether execution was authorized.
        allowed: bool,
        /// Stable denial class; absent for an allow.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Digest binding the complete decision material without logging it.
        decision_digest: String,
        /// Keyed scope commitment for storage records, distinct from physical path tokens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_scope_commitment: Option<StorageScopeCommitment>,
        /// Content-free storage evidence, normally present on terminal execution only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage: Option<StorageEvidence>,
    },
    /// Terminal provider execution metadata.
    Execution {
        /// Invocation identifier.
        invocation: InvocationId,
        /// Trace identifier.
        trace: TraceId,
        /// Authenticated caller principal; omitted for storage-backed records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<PrincipalId>,
        /// Trusted actor; omitted for storage-backed records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<Actor>,
        /// Attestor peer for attested contexts; absent for direct peers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<PrincipalId>,
        /// Canonical external subject for attested proposals.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attested_subject: Option<ExternalSubject>,
        /// Executed capability.
        capability: CapabilityId,
        /// Separately authorized public DRN, when any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<SecretDrn>,
        /// Native sink in which the DRN was consumed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_sink: Option<SecretSinkKind>,
        /// Trusted selected provider; omitted for storage-backed records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<ProviderId>,
        /// Broker principal that owned the authorization transition; omitted for storage records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorized_by: Option<PrincipalId>,
        /// Broker decision identifier.
        decision_id: String,
        /// Evaluated policy revision; omitted for storage records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_revision: Option<String>,
        /// Identifiers of the Cedar policies that authorized this execution.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        policy_ids: Vec<String>,
        /// Fingerprint of the policy set and world this execution was authorized against.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_digest: Option<String>,
        /// Trusted effect classification.
        effect: EffectKind,
        /// Trusted risk classification.
        risk: RiskLevel,
        /// Trusted retry classification.
        idempotency: Idempotency,
        /// Symbolic name of the credential this invocation selected; absent when it had none.
        ///
        /// The name is owner-authored configuration that already sits in `broker.yaml`, not
        /// secret material, and recording it is what keeps two external writes to two
        /// organizations from producing identical records once one capability can present two
        /// credentials. `credentialInjected` in the HTTP evidence still says whether a given call
        /// actually presented it; this says which authority the broker selected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
        /// Terminal execution outcome.
        outcome: InvocationOutcome,
        /// Bounded monotonic execution duration.
        duration_ms: u64,
        /// Stable public failure class.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Digest of successful provider output; output itself is never audited.
        #[serde(skip_serializing_if = "Option::is_none")]
        output_digest: Option<String>,
        /// Sanitized HTTP metadata; never paths, queries, headers, or bodies.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        http_calls: Vec<HttpCallEvidence>,
        /// Keyed scope commitment for storage records, distinct from physical path tokens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_scope_commitment: Option<StorageScopeCommitment>,
        /// Content-free coarse storage evidence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage: Option<StorageEvidence>,
    },
}

/// One immutable record in a process-local audit hash chain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AuditRecord {
    /// One-based contiguous sequence.
    pub sequence: u64,
    /// Previous record hash, absent only for the first record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_hash: Option<String>,
    /// Metadata-only event.
    pub event: AuditEvent,
    /// Domain-separated SHA-256 of sequence, previous hash, and event.
    pub record_hash: String,
}

/// Asynchronous append boundary owned by a broker deployment.
pub trait AuditLog: Send + Sync {
    /// Atomically appends one event after the current chain head.
    fn append(
        &self,
        event: AuditEvent,
    ) -> impl Future<Output = Result<AuditRecord, AuditError>> + Send;
}

/// Bounded process-local audit implementation for tests and embedding.
#[derive(Debug)]
pub struct InMemoryAuditLog {
    maximum: usize,
    state: Mutex<Vec<AuditRecord>>,
}

impl InMemoryAuditLog {
    /// Creates an empty bounded log.
    pub fn new(maximum: usize) -> Result<Self, AuditConfigurationError> {
        if maximum == 0 {
            return Err(AuditConfigurationError::ZeroMaximum);
        }
        Ok(Self {
            maximum,
            state: Mutex::new(Vec::new()),
        })
    }

    /// Returns a snapshot in sequence order.
    pub async fn records(&self) -> Vec<AuditRecord> {
        self.state.lock().await.clone()
    }
}

impl AuditLog for InMemoryAuditLog {
    async fn append(&self, event: AuditEvent) -> Result<AuditRecord, AuditError> {
        let mut records = self.state.lock().await;
        if records.len() >= self.maximum {
            return Err(AuditError::Full {
                maximum: self.maximum,
            });
        }
        let sequence = u64::try_from(records.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(AuditError::SequenceOverflow)?;
        let previous_hash = records.last().map(|record| record.record_hash.clone());
        let record_hash = audit_record_hash(sequence, previous_hash.as_deref(), &event)?;
        let record = AuditRecord {
            sequence,
            previous_hash,
            event,
            record_hash,
        };
        records.push(record.clone());
        Ok(record)
    }
}

/// Durable, owner-only JSONL audit chain.
///
/// Existing records are bounded and verified before the log accepts an append. Each append is
/// flushed and synchronized before it returns. A partial write poisons the open handle, and a
/// later reopen rejects the unterminated or invalid record.
#[derive(Debug)]
pub struct FileAuditLog {
    path: PathBuf,
    maximum_records: usize,
    maximum_line_bytes: usize,
    state: Mutex<FileAuditState>,
}

#[derive(Debug)]
struct FileAuditState {
    file: File,
    count: usize,
    head: Option<String>,
    /// The hash before `head`: the whole reconcile window, since an audit log can be at most one
    /// append ahead of its checkpoint. Retaining every verified hash instead would hold roughly
    /// 20 MB at the production record cap for the process lifetime, for a startup-only check.
    previous_head: Option<String>,
    /// Decision identifiers restored at startup, until the broker's replay ledger takes them.
    replay_ids: Option<BTreeSet<InvocationId>>,
    poisoned: bool,
}

impl FileAuditLog {
    /// Opens or creates an owner-only log and verifies every retained record.
    pub async fn open(
        path: impl AsRef<Path>,
        maximum_records: usize,
        maximum_line_bytes: usize,
    ) -> Result<Self, FileAuditError> {
        if maximum_records == 0 {
            return Err(FileAuditError::ZeroMaximumRecords);
        }
        if maximum_line_bytes == 0 {
            return Err(FileAuditError::ZeroMaximumLineBytes);
        }
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .await
            .map_err(|source| FileAuditError::Io { source })?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| FileAuditError::Io { source })?;
        if !metadata.is_file() {
            return Err(FileAuditError::NotRegularFile);
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o077 != 0 || metadata.nlink() != 1 {
            return Err(FileAuditError::InsecureFile);
        }
        let standard_file = file.into_std().await;
        standard_file
            .try_lock()
            .map_err(|source| FileAuditError::Lock {
                source: source.into(),
            })?;
        let file = File::from_std(standard_file);

        let mut reader = BufReader::new(file);
        let (count, head, previous_head, replay_ids) =
            scan_audit_file(&mut reader, maximum_records, maximum_line_bytes).await?;
        let mut file = reader.into_inner();
        file.seek(SeekFrom::End(0))
            .await
            .map_err(|source| FileAuditError::Io { source })?;
        Ok(Self {
            path,
            maximum_records,
            maximum_line_bytes,
            state: Mutex::new(FileAuditState {
                file,
                count,
                head,
                previous_head,
                replay_ids: Some(replay_ids),
                poisoned: false,
            }),
        })
    }

    /// Returns the configured file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the verified record count and current chain head.
    pub async fn checkpoint(&self) -> (usize, Option<String>) {
        let state = self.state.lock().await;
        (state.count, state.head.clone())
    }

    /// Reports whether a retained sequence/head pair is an exact verified chain prefix.
    ///
    /// Only the reconcile window is answered for: the current head, the record before it, and the
    /// empty chain. A checkpoint further behind than one append is not a prefix this log will
    /// confirm — reconciliation rejects that gap on its own, and confirming it would mean keeping
    /// every verified hash resident forever.
    pub async fn contains_checkpoint(&self, count: usize, head: Option<&str>) -> bool {
        let state = self.state.lock().await;
        match count {
            0 => head.is_none(),
            count if count == state.count => head == state.head.as_deref(),
            count if Some(count) == state.count.checked_sub(1) => {
                head == state.previous_head.as_deref()
            }
            _ => false,
        }
    }

    /// Returns invocation IDs reconstructed from verified decision records, once.
    ///
    /// Consuming on purpose. The only caller hands them straight to the broker's replay ledger,
    /// which owns them from then on; keeping a second copy here would duplicate the ledger at
    /// startup and then grow it forever on a path nothing reads again. A later call returns
    /// nothing, and appends stop recording once they have been taken.
    pub async fn take_replay_ids(&self) -> Vec<InvocationId> {
        let mut state = self.state.lock().await;
        state
            .replay_ids
            .take()
            .map(|ids| ids.into_iter().collect())
            .unwrap_or_default()
    }
}

impl AuditLog for FileAuditLog {
    async fn append(&self, event: AuditEvent) -> Result<AuditRecord, AuditError> {
        let mut state = self.state.lock().await;
        if state.poisoned {
            return Err(AuditError::Poisoned);
        }
        if state.count >= self.maximum_records {
            return Err(AuditError::Full {
                maximum: self.maximum_records,
            });
        }
        let sequence = u64::try_from(state.count)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(AuditError::SequenceOverflow)?;
        let previous_hash = state.head.clone();
        let encoded = encode_audit_event(&event)?;
        let record_hash = encoded_audit_record_hash(sequence, previous_hash.as_deref(), &encoded)?;
        // The one serialization of the event covers both the hash material and the durable line.
        let mut line = serde_json::to_vec(&AuditRecordLine {
            sequence,
            previous_hash: previous_hash.as_deref(),
            event: &encoded,
            record_hash: &record_hash,
        })
        .map_err(|source| AuditError::Serialize { source })?;
        let record = AuditRecord {
            sequence,
            previous_hash,
            event,
            record_hash,
        };
        if line.len() > self.maximum_line_bytes {
            return Err(AuditError::RecordTooLarge {
                length: line.len(),
                maximum: self.maximum_line_bytes,
            });
        }
        line.push(b'\n');

        state.poisoned = true;
        if let Err(source) = state.file.write_all(&line).await {
            return Err(AuditError::Io { source });
        }
        if let Err(source) = state.file.flush().await {
            return Err(AuditError::Io { source });
        }
        if let Err(source) = state.file.sync_all().await {
            return Err(AuditError::Io { source });
        }
        state.count += 1;
        state.previous_head = state.head.replace(record.record_hash.clone());
        if let Some(ids) = state.replay_ids.as_mut()
            && let AuditEvent::Decision { invocation, .. } = &record.event
        {
            ids.insert(invocation.clone());
        }
        state.poisoned = false;
        Ok(record)
    }
}

async fn scan_audit_file(
    reader: &mut BufReader<File>,
    maximum_records: usize,
    maximum_line_bytes: usize,
) -> Result<
    (
        usize,
        Option<String>,
        Option<String>,
        BTreeSet<InvocationId>,
    ),
    FileAuditError,
> {
    let mut count = 0_usize;
    let mut previous = None::<String>;
    let mut before_previous = None::<String>;
    let mut replay_ids = BTreeSet::new();
    loop {
        let Some(line) = read_bounded_line(reader, maximum_line_bytes, count + 1).await? else {
            return Ok((count, previous, before_previous, replay_ids));
        };
        if count >= maximum_records {
            return Err(FileAuditError::TooManyRecords {
                maximum: maximum_records,
            });
        }
        let record = serde_json::from_slice::<AuditRecord>(&line).map_err(|source| {
            FileAuditError::InvalidRecord {
                line: count + 1,
                source,
            }
        })?;
        verify_file_record(count, previous.as_deref(), &record)?;
        if let AuditEvent::Decision { invocation, .. } = &record.event {
            replay_ids.insert(invocation.clone());
        }
        before_previous = previous.replace(record.record_hash);
        count += 1;
    }
}

async fn read_bounded_line(
    reader: &mut BufReader<File>,
    maximum: usize,
    line_number: usize,
) -> Result<Option<Vec<u8>>, FileAuditError> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|source| FileAuditError::Io { source })?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Err(FileAuditError::UnterminatedRecord { line: line_number });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let chunk_length = newline.unwrap_or(available.len());
        let length =
            line.len()
                .checked_add(chunk_length)
                .ok_or(FileAuditError::RecordTooLarge {
                    line: line_number,
                    maximum,
                })?;
        if length > maximum {
            return Err(FileAuditError::RecordTooLarge {
                line: line_number,
                maximum,
            });
        }
        line.extend_from_slice(&available[..chunk_length]);
        let consumed = chunk_length + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn verify_file_record(
    index: usize,
    previous: Option<&str>,
    record: &AuditRecord,
) -> Result<(), FileAuditError> {
    let expected_sequence = u64::try_from(index)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or(FileAuditError::Integrity {
            line: index + 1,
            source: AuditIntegrityError::Sequence { index },
        })?;
    if record.sequence != expected_sequence {
        return Err(FileAuditError::Integrity {
            line: index + 1,
            source: AuditIntegrityError::Sequence { index },
        });
    }
    if record.previous_hash.as_deref() != previous {
        return Err(FileAuditError::Integrity {
            line: index + 1,
            source: AuditIntegrityError::PreviousHash { index },
        });
    }
    #[allow(
        clippy::map_err_ignore,
        reason = "the only failure `audit_record_hash` reports is `AuditError::Serialize`, and \
                  `AuditHashMaterial` is a derived-Serialize tree of integers, bools, strings, \
                  and string newtypes — no map with non-string keys and no float, so serde_json \
                  has no failure to describe"
    )]
    let expected = audit_record_hash(
        record.sequence,
        record.previous_hash.as_deref(),
        &record.event,
    )
    .map_err(|_| FileAuditError::Integrity {
        line: index + 1,
        source: AuditIntegrityError::Serialize { index },
    })?;
    if record.record_hash != expected {
        return Err(FileAuditError::Integrity {
            line: index + 1,
            source: AuditIntegrityError::RecordHash { index },
        });
    }
    Ok(())
}

/// Failure to open and verify a durable audit chain.
#[derive(Debug, Error)]
pub enum FileAuditError {
    /// Record bound was zero.
    #[error("durable audit record maximum must be greater than zero")]
    ZeroMaximumRecords,
    /// Per-record byte bound was zero.
    #[error("durable audit line maximum must be greater than zero")]
    ZeroMaximumLineBytes,
    /// Audit path did not identify a regular file.
    #[error("durable audit path must identify a regular file")]
    NotRegularFile,
    /// Unix file permissions or hard-link count did not preserve exclusive ownership.
    #[error("durable audit file must be owner-only and have exactly one hard link")]
    InsecureFile,
    /// Another process already owns the audit writer lock.
    #[error("durable audit file is already locked by another writer")]
    Lock {
        /// Lock failure.
        #[source]
        source: io::Error,
    },
    /// Existing log exceeded its configured record bound.
    #[error("durable audit log exceeds its {maximum}-record bound")]
    TooManyRecords {
        /// Configured maximum.
        maximum: usize,
    },
    /// One existing record exceeded its byte bound.
    #[error("durable audit record on line {line} exceeds {maximum} bytes")]
    RecordTooLarge {
        /// One-based line number.
        line: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Existing final record was only partially written.
    #[error("durable audit record on line {line} is not newline-terminated")]
    UnterminatedRecord {
        /// One-based line number.
        line: usize,
    },
    /// Existing JSONL record was malformed or had unknown fields.
    #[error("durable audit record on line {line} is invalid JSON")]
    InvalidRecord {
        /// One-based line number.
        line: usize,
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Existing record failed sequence or hash verification.
    #[error("durable audit record on line {line} failed integrity verification")]
    Integrity {
        /// One-based line number.
        line: usize,
        /// Verification failure.
        #[source]
        source: AuditIntegrityError,
    },
    /// File operation failed.
    #[error("durable audit file operation failed")]
    Io {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
}

/// Invalid in-memory audit configuration.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AuditConfigurationError {
    /// A zero-record log could never audit a request.
    #[error("audit record maximum must be greater than zero")]
    ZeroMaximum,
}

/// Audit append failure.
#[derive(Debug, Error)]
pub enum AuditError {
    /// Bounded audit storage was exhausted.
    #[error("audit log reached its {maximum}-record bound")]
    Full {
        /// Configured maximum.
        maximum: usize,
    },
    /// Durable handle encountered an earlier partial or failed append.
    #[error("durable audit handle is poisoned after an incomplete append")]
    Poisoned,
    /// Serialized durable record exceeded its byte ceiling.
    #[error("audit record is {length} bytes; maximum is {maximum}")]
    RecordTooLarge {
        /// Actual serialized bytes.
        length: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Sequence could not be represented.
    #[error("audit sequence overflowed")]
    SequenceOverflow,
    /// Event could not be deterministically serialized for hashing.
    #[error("could not serialize audit event")]
    Serialize {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Durable append, flush, or sync failed.
    #[error("durable audit append failed")]
    Io {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
}

impl AuditError {
    /// Stable low-cardinality classification for logs and span fields.
    ///
    /// A designed refusal, a handle that stays dead until restart, and a filesystem that stopped
    /// accepting writes need three different operator responses, so the failure that reaches
    /// telemetry must name which one happened.
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Full { .. } => "full",
            Self::Poisoned => "poisoned",
            Self::RecordTooLarge { .. } => "record-too-large",
            Self::SequenceOverflow => "sequence-overflow",
            Self::Serialize { .. } => "serialize",
            Self::Io { .. } => "io",
        }
    }
}

/// Audit-chain verification failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AuditIntegrityError {
    /// Record sequence was not one-based and contiguous.
    #[error("audit record at index {index} has a non-contiguous sequence")]
    Sequence {
        /// Zero-based index.
        index: usize,
    },
    /// Previous hash did not match the preceding record.
    #[error("audit record at index {index} has an invalid previous hash")]
    PreviousHash {
        /// Zero-based index.
        index: usize,
    },
    /// Record content did not match its digest.
    #[error("audit record at index {index} has an invalid record hash")]
    RecordHash {
        /// Zero-based index.
        index: usize,
    },
    /// Record could not be reserialized.
    #[error("could not serialize audit record at index {index}")]
    Serialize {
        /// Zero-based index.
        index: usize,
    },
}

/// Verifies sequence, linkage, and record hashes for a retained chain.
pub fn verify_audit_chain(records: &[AuditRecord]) -> Result<(), AuditIntegrityError> {
    let mut previous = None::<&str>;
    for (index, record) in records.iter().enumerate() {
        let expected_sequence = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(AuditIntegrityError::Sequence { index })?;
        if record.sequence != expected_sequence {
            return Err(AuditIntegrityError::Sequence { index });
        }
        if record.previous_hash.as_deref() != previous {
            return Err(AuditIntegrityError::PreviousHash { index });
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "the only failure `audit_record_hash` reports is `AuditError::Serialize`, \
                      and `AuditHashMaterial` is a derived-Serialize tree of integers, bools, \
                      strings, and string newtypes — no map with non-string keys and no float, \
                      so serde_json has no failure to describe"
        )]
        let expected = audit_record_hash(
            record.sequence,
            record.previous_hash.as_deref(),
            &record.event,
        )
        .map_err(|_| AuditIntegrityError::Serialize { index })?;
        if record.record_hash != expected {
            return Err(AuditIntegrityError::RecordHash { index });
        }
        previous = Some(record.record_hash.as_str());
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditHashMaterial<'a> {
    sequence: u64,
    previous_hash: Option<&'a str>,
    event: &'a RawValue,
}

/// The durable JSONL shape of [`AuditRecord`], over an already-serialized event.
///
/// Field names, order, and the absent-`previousHash` rule must stay identical to `AuditRecord`'s
/// derived encoding: this is the same bytes on disk, written without serializing the event a
/// second time. `durable_line_matches_the_record_encoding` fails if the two ever diverge.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuditRecordLine<'a> {
    sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_hash: Option<&'a str>,
    event: &'a RawValue,
    record_hash: &'a str,
}

fn encode_audit_event(event: &AuditEvent) -> Result<Box<RawValue>, AuditError> {
    serde_json::value::to_raw_value(event).map_err(|source| AuditError::Serialize { source })
}

fn audit_record_hash(
    sequence: u64,
    previous_hash: Option<&str>,
    event: &AuditEvent,
) -> Result<String, AuditError> {
    encoded_audit_record_hash(sequence, previous_hash, &encode_audit_event(event)?)
}

fn encoded_audit_record_hash(
    sequence: u64,
    previous_hash: Option<&str>,
    event: &RawValue,
) -> Result<String, AuditError> {
    let bytes = serde_json::to_vec(&AuditHashMaterial {
        sequence,
        previous_hash,
        event,
    })
    .map_err(|source| AuditError::Serialize { source })?;
    Ok(domain_digest(AUDIT_HASH_DOMAIN, &bytes))
}

#[derive(Debug)]
struct ReplayLedger {
    maximum: usize,
    ids: Mutex<BTreeSet<InvocationId>>,
}

impl ReplayLedger {
    async fn reserve(&self, invocation: &InvocationId) -> Result<bool, BrokerError> {
        let mut ids = self.ids.lock().await;
        if ids.contains(invocation) {
            return Ok(false);
        }
        if ids.len() >= self.maximum {
            return Err(BrokerError::ReplayLedgerFull {
                maximum: self.maximum,
            });
        }
        ids.insert(invocation.clone());
        Ok(true)
    }
}

/// Broker-owned authorization, execution, evidence, and audit coordinator.
#[derive(Debug)]
pub struct Broker<A> {
    registry: BrokerProviderRegistry,
    policy: PolicyEngine,
    policy_revision: String,
    policy_digest: String,
    constraints: ConstraintCatalog,
    credentials: CredentialStore,
    secrets: SecretCatalog,
    identities: IdentityDirectory,
    broker_principal: PrincipalId,
    gate: AuthorizationGate,
    audit: Arc<A>,
    replay: ReplayLedger,
    chat_memory: Option<ChatMemoryConfig>,
}

impl<A> Broker<A>
where
    A: AuditLog,
{
    /// Builds a broker around an already validated privileged component registry.
    #[allow(
        clippy::too_many_arguments,
        reason = "each trusted input is a separate owner-controlled store; bundling them into one \
                  struct would let a caller assemble policy, constraints, credentials, and identity \
                  mapping from mismatched sources without the type system noticing"
    )]
    pub fn new(
        registry: BrokerProviderRegistry,
        broker_principal: PrincipalId,
        policy_revision: String,
        policy: PolicyEngine,
        constraints: ConstraintCatalog,
        credentials: CredentialStore,
        identities: IdentityDirectory,
        audit: Arc<A>,
        limits: BrokerLimits,
    ) -> Result<Self, BrokerBuildError> {
        Self::new_with_replay_ids(
            registry,
            broker_principal,
            policy_revision,
            policy,
            constraints,
            credentials,
            identities,
            audit,
            limits,
            std::iter::empty(),
        )
    }

    /// Builds a broker while restoring invocation IDs from a verified durable audit chain.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_replay_ids(
        registry: BrokerProviderRegistry,
        broker_principal: PrincipalId,
        policy_revision: String,
        policy: PolicyEngine,
        constraints: ConstraintCatalog,
        credentials: CredentialStore,
        identities: IdentityDirectory,
        audit: Arc<A>,
        limits: BrokerLimits,
        replay_ids: impl IntoIterator<Item = InvocationId>,
    ) -> Result<Self, BrokerBuildError> {
        Self::start(
            registry,
            broker_principal,
            policy_revision,
            policy,
            constraints,
            credentials,
            identities,
            audit,
            limits,
            Leniency::Strict,
            replay_ids,
        )
        .map(|(broker, _)| broker)
    }

    /// Builds a broker, choosing whether configuration that cannot apply refuses startup.
    ///
    /// This is the full constructor [`Broker::new`] and [`Broker::new_with_replay_ids`] delegate
    /// to; both pin [`Leniency::Strict`], which is exactly today's behavior.
    ///
    /// Under [`Leniency::Tolerant`] two startup refusals become [`StartupWarning`]s instead:
    /// a constraint set naming a capability no loaded provider routes is dropped, and a capability
    /// a policy could permit but no constraint set bounds is reported. Neither weakens a decision.
    /// The runtime `unconstrained-capability` denial is untouched and remains unconditional, so a
    /// tolerated capability is refused at invocation exactly as a strict deployment refuses it at
    /// startup — deny-by-default is preserved, only the moment of complaint moves.
    ///
    /// # Errors
    ///
    /// Every [`BrokerBuildError`] the strict path returns, except the two named above when
    /// tolerating.
    #[allow(
        clippy::too_many_arguments,
        reason = "each trusted input is a separate owner-controlled store; bundling them into one \
                  struct would let a caller assemble policy, constraints, credentials, and identity \
                  mapping from mismatched sources without the type system noticing"
    )]
    pub fn start(
        registry: BrokerProviderRegistry,
        broker_principal: PrincipalId,
        policy_revision: String,
        policy: PolicyEngine,
        constraints: ConstraintCatalog,
        credentials: CredentialStore,
        identities: IdentityDirectory,
        audit: Arc<A>,
        limits: BrokerLimits,
        leniency: Leniency,
        replay_ids: impl IntoIterator<Item = InvocationId>,
    ) -> Result<(Self, Vec<StartupWarning>), BrokerBuildError> {
        let mut constraints = constraints;
        let mut warnings = Vec::new();
        if limits.max_constraint_sets == 0 {
            return Err(BrokerBuildError::ZeroLimit {
                field: "max_constraint_sets",
            });
        }
        if limits.max_replay_ids == 0 {
            return Err(BrokerBuildError::ZeroLimit {
                field: "max_replay_ids",
            });
        }
        validate_policy_revision(&policy_revision)?;
        if leniency == Leniency::Tolerant {
            // Drop before validating: a set naming a capability nothing routes has no manifest to
            // be checked against, so it cannot be proven either way.
            for capability in constraints.retain_routed(&registry) {
                warnings.push(StartupWarning::UnroutedConstraintSet { capability });
            }
        }
        constraints.validate(&registry, &credentials, limits.max_constraint_sets)?;
        // Every capability a policy could permit must be executable. The decision path treats a
        // missing constraint set as a denial anyway, but a grant that can only ever be refused is
        // a configuration mistake worth refusing to start over.
        for capability in policy.referenced_capabilities() {
            if constraints.get(capability).is_none() {
                match leniency {
                    Leniency::Strict => {
                        return Err(BrokerBuildError::UnconstrainedCapability {
                            capability: capability.clone(),
                        });
                    }
                    Leniency::Tolerant => {
                        warnings.push(StartupWarning::UnconstrainedCapability {
                            capability: capability.clone(),
                        });
                    }
                }
            }
        }
        let mut restored_replay_ids = BTreeSet::new();
        for invocation in replay_ids {
            restored_replay_ids.insert(invocation);
            if restored_replay_ids.len() > limits.max_replay_ids {
                return Err(BrokerBuildError::TooManyReplayIds {
                    maximum: limits.max_replay_ids,
                });
            }
        }
        Ok((
            Self {
                registry,
                policy_digest: policy.digest().to_owned(),
                policy,
                policy_revision,
                constraints,
                credentials,
                secrets: SecretCatalog::empty(),
                identities,
                broker_principal,
                gate: AuthorizationGate::new(),
                audit,
                replay: ReplayLedger {
                    maximum: limits.max_replay_ids,
                    ids: Mutex::new(restored_replay_ids),
                },
                chat_memory: None,
            },
            warnings,
        ))
    }

    /// Installs the owner-only private secret map after validating every binding against the
    /// already validated capability constraint catalog.
    pub fn with_secret_catalog(mut self, secrets: SecretCatalog) -> Result<Self, BrokerBuildError> {
        secrets.validate(&self.constraints)?;
        self.secrets = secrets;
        Ok(self)
    }

    /// The fingerprint of the policy set every decision by this broker is evaluated against.
    #[must_use]
    pub fn policy_digest(&self) -> &str {
        &self.policy_digest
    }

    /// Enables the optional all-or-nothing JSONL chat-memory surface after composition checks.
    pub fn with_chat_memory(mut self, config: ChatMemoryConfig) -> Result<Self, BrokerBuildError> {
        let storage_host = self
            .registry
            .storage_host()
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        config.validate(storage_host.limits())?;
        config.validate_host_limits(self.registry.host_limits())?;
        let expected = [
            (
                MEMORY_RECORD,
                EffectKind::LocalWrite,
                RiskLevel::Medium,
                Idempotency::Conditional,
                StorageAccess::ReadWrite,
            ),
            (
                MEMORY_RECENT,
                EffectKind::ReadOnly,
                RiskLevel::High,
                Idempotency::Idempotent,
                StorageAccess::ReadOnly,
            ),
            (
                MEMORY_SEARCH,
                EffectKind::ReadOnly,
                RiskLevel::High,
                Idempotency::Idempotent,
                StorageAccess::ReadOnly,
            ),
        ];
        for (identifier, effect, risk, idempotency, access) in expected {
            #[allow(
                clippy::map_err_ignore,
                reason = "`identifier` is one of the MEMORY_RECORD/RECENT/SEARCH constants this \
                          crate defines, so the parse cannot fail and an IdentifierError could \
                          only restate a literal we control"
            )]
            let capability = identifier
                .parse::<CapabilityId>()
                .map_err(|_| BrokerBuildError::InvalidChatMemory)?;
            let set = self
                .constraints
                .get(&capability)
                .ok_or(BrokerBuildError::InvalidChatMemory)?;
            let storage = set
                .constraints
                .storage
                .as_ref()
                .ok_or(BrokerBuildError::InvalidChatMemory)?;
            if set.provider.as_str() != MEMORY_PROVIDER
                || set.effect != effect
                || set.risk != risk
                || set.idempotency != idempotency
                || set.credential.is_some()
                || !set.credential_by_agent.is_empty()
                || storage.interface != StorageInterface::Jsonl
                || storage.access != access
                || storage.namespace != StorageNamespace::Chat
                || set.constraints.http.is_some()
                || set.constraints.max_output_bytes
                    < if identifier == MEMORY_RECORD {
                        MEMORY_PROVIDER_OUTPUT_OVERHEAD_BYTES
                    } else {
                        config
                            .max_result_bytes
                            .checked_add(MEMORY_PROVIDER_OUTPUT_OVERHEAD_BYTES)
                            .ok_or(BrokerBuildError::InvalidChatMemory)?
                    }
            {
                return Err(BrokerBuildError::InvalidChatMemory);
            }
        }
        let memory_provider = self
            .registry
            .manifests()
            .find(|manifest| manifest.id.as_str() == MEMORY_PROVIDER)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let declared = memory_provider
            .capabilities
            .iter()
            .map(|capability| capability.id.as_str())
            .collect::<BTreeSet<_>>();
        if declared
            != [MEMORY_RECORD, MEMORY_RECENT, MEMORY_SEARCH]
                .into_iter()
                .collect()
            || self.registry.capabilities().any(|(provider, capability)| {
                capability.id.as_str().starts_with("memory.chat.")
                    && (provider.as_str() != MEMORY_PROVIDER
                        || !is_memory_capability(&capability.id))
            })
        {
            return Err(BrokerBuildError::InvalidChatMemory);
        }
        self.chat_memory = Some(config);
        Ok(self)
    }

    /// Whether trusted routing constraints classify this capability as storage-backed.
    ///
    /// Used before outer span construction so generic storage providers receive the same
    /// identity-free telemetry treatment as the built-in memory route.
    #[must_use]
    pub fn capability_uses_storage(&self, capability: &CapabilityId) -> bool {
        is_reserved_memory_route(capability, self.constraints.get(capability))
            || self
                .constraints
                .get(capability)
                .is_some_and(|set| set.constraints.storage.is_some())
    }

    /// Asks policy whether this context may act on one capability at all.
    fn authorize_capability(
        &self,
        context: &AuthenticatedContext,
        capability: &CapabilityId,
        set: &ConstraintSet,
    ) -> PolicyDecision {
        self.policy.authorize(PolicyRequest {
            principal: context.principal().clone(),
            target: PolicyTarget::Capability {
                capability: capability.clone(),
                provider: set.provider.clone(),
                effect: set.effect,
                risk: set.risk,
                idempotency: set.idempotency,
            },
            context: policy_context(context),
        })
    }

    /// Separately asks policy whether this context may use one exact public DRN.
    fn authorize_secret_use(
        &self,
        context: &AuthenticatedContext,
        capability: &CapabilityId,
        set: &ConstraintSet,
        proposal: &SecretUseProposal,
    ) -> PolicyDecision {
        self.policy.authorize(PolicyRequest {
            principal: context.principal().clone(),
            target: PolicyTarget::SecretUse {
                secret: proposal.secret().clone(),
                capability: capability.clone(),
                provider: set.provider.clone(),
                sink: proposal.sink(),
            },
            context: policy_context(context),
        })
    }

    /// Asks policy whether this context may drive one agent's session at all.
    ///
    /// This is the session gate. Permitting a principal to talk to an agent is now its own
    /// explicit policy statement rather than a side effect of holding any capability.
    fn authorize_agent_prompt(
        &self,
        context: &AuthenticatedContext,
        agent: &AgentId,
    ) -> PolicyDecision {
        self.policy.authorize(PolicyRequest {
            principal: context.principal().clone(),
            target: PolicyTarget::AgentPrompt {
                agent: agent.clone(),
            },
            context: policy_context(context),
        })
    }

    /// The constraint sets policy allows this exact context, one Cedar evaluation each.
    ///
    /// Every listing filters this same list. That is what makes a listing identical to the
    /// decision an invocation would receive: there is only ever one evaluation to disagree with.
    fn authorized_sets(
        &self,
        context: &AuthenticatedContext,
    ) -> Vec<(&CapabilityId, &ConstraintSet)> {
        self.constraints
            .iter()
            .filter(|(capability, set)| {
                !is_reserved_memory_route(capability, Some(set))
                    && (set.constraints.storage.is_none() || context.chat_scope().is_some())
                    && self.authorize_capability(context, capability, set).allowed
            })
            .collect()
    }

    /// Returns the command words this context may use.
    ///
    /// A word appears only when policy allows this context at least one capability of the provider
    /// declaring it, so a session is never told a word exists that it could not use — and a
    /// principal granted nothing sees an empty vocabulary rather than a map of the deployment.
    #[must_use]
    pub fn command_words(&self, context: &AuthenticatedContext) -> Vec<String> {
        self.reachable_command_words(context, &self.authorized_sets(context))
    }

    fn reachable_command_words(
        &self,
        context: &AuthenticatedContext,
        authorized: &[(&CapabilityId, &ConstraintSet)],
    ) -> Vec<String> {
        let reachable = authorized
            .iter()
            .map(|(_, set)| &set.provider)
            .collect::<BTreeSet<_>>();
        let storage_providers = self
            .constraints
            .iter()
            .map(|(_, set)| set)
            .filter(|set| set.constraints.storage.is_some())
            .map(|set| set.provider.clone())
            .collect::<BTreeSet<_>>();
        let reserved_providers = self
            .registry
            .capabilities()
            .filter(|(provider, capability)| {
                provider.as_str() == MEMORY_PROVIDER
                    || capability.id.as_str().starts_with("memory.chat.")
            })
            .map(|(provider, _)| provider.clone())
            .collect::<BTreeSet<_>>();
        let mut words = self
            .registry
            .command_words_by_provider()
            .into_iter()
            .filter(|(provider, _)| {
                !reserved_providers.contains(*provider)
                    && reachable.contains(provider)
                    && (context.chat_scope().is_some() || !storage_providers.contains(*provider))
            })
            .flat_map(|(_, words)| {
                words
                    .iter()
                    .filter(|word| word.as_str() != MEMORY_WORD)
                    .cloned()
            })
            .collect::<Vec<_>>();
        words.sort();
        words.dedup();
        words
    }

    /// Rewrites one command word's arguments into a capability proposal.
    ///
    /// Ungated on purpose. This is a pure function inside the declaring component — no imports,
    /// bounded by fuel and timeout — and what it returns is a *proposal*. Authorization happens
    /// where it always happens, on the invocation that follows; a caller who rewrites a word they
    /// may not use receives a denial one step later, having learned nothing they could not learn
    /// by asking for the capability directly.
    ///
    /// # Errors
    ///
    /// Returns a host error when no loaded provider declares the word, when the guest traps, or
    /// when the rewrite reaches for a host import.
    pub async fn resolve_command(
        &self,
        word: &str,
        argv: &[String],
    ) -> Result<CommandResolution, BrokerHostError> {
        let reserved_provider_word =
            self.registry
                .command_words_by_provider()
                .into_iter()
                .any(|(provider, words)| {
                    words.iter().any(|value| value == word)
                        && self.registry.capabilities().any(|(candidate, capability)| {
                            candidate == provider
                                && (candidate.as_str() == MEMORY_PROVIDER
                                    || capability.id.as_str().starts_with("memory.chat."))
                        })
                });
        if word == MEMORY_WORD || reserved_provider_word {
            return Err(BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            });
        }
        let resolution = self.registry.resolve_command(word, argv).await?;
        if matches!(
            &resolution,
            CommandResolution::Resolved { capability, .. }
                if is_reserved_memory_route(capability, self.constraints.get(capability))
        ) {
            return Err(BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            });
        }
        Ok(resolution)
    }

    /// Returns only capabilities policy allows for this exact authenticated context.
    ///
    /// The listing and the invocation decision come from the same evaluation, so a capability can
    /// never appear here and then refuse — or be hidden here and then succeed.
    #[must_use]
    pub fn capabilities(&self, context: &AuthenticatedContext) -> Vec<AvailableCapability> {
        self.available_capabilities(&self.authorized_sets(context))
    }

    /// Returns the capability listing and the command words from one authorization pass.
    ///
    /// Both halves of a capabilities answer are policy filters over the same constraint sets, and
    /// a session opens with one. Computing them separately evaluates every set through Cedar
    /// twice for identical inputs on the broker's most frequent request.
    #[must_use]
    pub fn capability_view(
        &self,
        context: &AuthenticatedContext,
    ) -> (Vec<AvailableCapability>, Vec<String>) {
        let authorized = self.authorized_sets(context);
        (
            self.available_capabilities(&authorized),
            self.reachable_command_words(context, &authorized),
        )
    }

    fn available_capabilities(
        &self,
        authorized: &[(&CapabilityId, &ConstraintSet)],
    ) -> Vec<AvailableCapability> {
        let mut capabilities = authorized
            .iter()
            .map(|(capability_id, set)| {
                let (_, manifest_capability) = self
                    .registry
                    .capability(capability_id)
                    .expect("constraint validation proves every capability route");
                let mut capability = manifest_capability.clone();
                capability.effect = set.effect;
                capability.risk = set.risk;
                capability.idempotency = set.idempotency;
                AvailableCapability {
                    provider: set.provider.clone(),
                    capability,
                }
            })
            .collect::<Vec<_>>();
        capabilities.sort_by(|left, right| left.capability.id.cmp(&right.capability.id));
        capabilities
    }

    /// The widest capability answer this broker could ever produce.
    ///
    /// [`Self::capabilities`], [`Self::command_words`], and the chat surface are all policy
    /// filters over exactly these values, for every context the broker can build — direct peer,
    /// attested, or chat. Enumerating those contexts is not possible here: the agent catalog
    /// belongs to the gateway, and production policy conditions on `context.agent`, so a broker
    /// that guessed an agent would measure a surface no session ever receives. Bounding them is
    /// what a startup frame check actually needs, because a ceiling that fits proves that no
    /// session's answer can overflow the frame.
    #[must_use]
    pub fn capability_ceiling(&self) -> (Vec<AvailableCapability>, Vec<String>) {
        let mut capabilities = self
            .constraints
            .iter()
            .filter_map(|(capability, _)| self.available_capability(capability))
            .collect::<Vec<_>>();
        capabilities.sort_by(|left, right| left.capability.id.cmp(&right.capability.id));
        let mut words = self
            .registry
            .command_words_by_provider()
            .into_iter()
            .flat_map(|(_, words)| words.iter().cloned())
            .collect::<Vec<_>>();
        words.sort();
        words.dedup();
        (capabilities, words)
    }

    /// The chat memory surface a session could be told about, sized for the frame check.
    ///
    /// Policy still decides whether any given session sees it; this is only its byte cost.
    #[must_use]
    pub fn chat_memory_ceiling(&self) -> Option<ChatMemorySurface> {
        let config = self.chat_memory.as_ref()?;
        Some(ChatMemorySurface {
            max_lookback_turns: config.max_lookback_turns,
            prompt_note: memory_prompt_note(config.max_lookback_turns),
        })
    }

    /// Evaluates and, when allowed, executes one authenticated proposal exactly once.
    pub async fn invoke(
        &self,
        context: &AuthenticatedContext,
        request: InvocationRequest,
    ) -> Result<InvocationResult, BrokerError> {
        let refusal = is_reserved_memory_route(
            &request.capability,
            self.constraints.get(&request.capability),
        )
        .then_some(Refusal {
            reason: "chat-scope-required",
            policy_ids: Vec::new(),
        });
        self.invoke_inner(context, request, refusal).await
    }

    /// Evaluates one proposal attested on behalf of an external subject.
    ///
    /// `peer` is the connected transport identity and `grant` is that peer's owner-configured
    /// attestor authority, or `None` when it has none. The broker — never the peer — performs
    /// the subject-to-principal mapping. Every refusal is an audited, replay-consuming denial
    /// under the peer's own identity with a stable reason (`attestation-denied` for a missing or
    /// out-of-scope grant, `unmapped-subject` for a subject the directory does not name), so a
    /// compromised or misconfigured gateway leaves a decision trail rather than a silent error.
    /// A denied `agent.prompt` is reported as `agent-denied` under the attested context: the
    /// attestation itself was honored, and the refusal is about who may drive this agent.
    pub async fn invoke_for(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: &SubjectAttestation,
        request: InvocationRequest,
    ) -> Result<InvocationResult, BrokerError> {
        let (context, refusal) = self.resolve_attestation(peer, grant, attestation);
        let refusal = match refusal {
            Some(reason) => Some(Refusal {
                reason,
                policy_ids: Vec::new(),
            }),
            None if is_reserved_memory_route(
                &request.capability,
                self.constraints.get(&request.capability),
            ) =>
            {
                Some(Refusal {
                    reason: "chat-scope-required",
                    policy_ids: Vec::new(),
                })
            }
            None => {
                let decision = self.authorize_agent_prompt(&context, &attestation.agent);
                (!decision.allowed).then(|| Refusal {
                    reason: denial_reason(&decision, "agent-denied"),
                    policy_ids: decision.determining_policy_ids,
                })
            }
        };
        self.invoke_inner(&context, request, refusal).await
    }

    /// Returns capabilities visible to one attested on-behalf-of context.
    ///
    /// `None` means the request was refused — no grant, out-of-scope subject, unmapped subject, or
    /// a policy that does not let this principal drive this agent at all — which callers must not
    /// conflate with "allowed to ask, granted nothing" (`Some` with an empty list). Answering a
    /// refused caller with an empty list would tell it whether the subject is mapped.
    ///
    /// The bare `Option` keeps the wire answer opaque, so the refusal class is reported here
    /// instead: one `broker_capabilities_refused` event names the class and the canonical subject
    /// on the broker's own side of the socket, where a session that never invokes would otherwise
    /// leave no trace of why it saw nothing.
    #[must_use]
    pub fn capabilities_for(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        subject: &ExternalSubject,
        agent: &AgentId,
    ) -> Option<(Vec<AvailableCapability>, Vec<String>)> {
        if !grant.is_some_and(|grant| grant.permits(subject)) {
            report_inspection_refusal("attestation-denied", peer, subject, agent);
            return None;
        }
        let Some(principal) = self.identities.resolve(subject) else {
            report_inspection_refusal("unmapped-subject", peer, subject, agent);
            return None;
        };
        let context = match AuthenticatedContext::attested(
            principal.clone(),
            Actor::Agent {
                agent: agent.clone(),
            },
            peer.principal().clone(),
            subject.clone(),
        ) {
            Ok(context) => context,
            Err(_) => {
                report_inspection_refusal("attestation-denied", peer, subject, agent);
                return None;
            }
        };
        let decision = self.authorize_agent_prompt(&context, agent);
        if !decision.allowed {
            report_inspection_refusal(
                denial_reason(&decision, "agent-denied"),
                peer,
                subject,
                agent,
            );
            return None;
        }
        Some(self.capability_view(&context))
    }

    /// Returns a freshly authorized chat surface. Scope refusal reveals no mapping or namespace.
    #[must_use]
    pub fn capabilities_for_chat(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        claim: &ChatSessionClaim,
    ) -> Option<(
        Vec<AvailableCapability>,
        Vec<String>,
        Option<ChatMemorySurface>,
    )> {
        let context = match self.resolve_chat_claim(peer, grant, claim) {
            Ok(context) => context,
            Err(reason) => {
                report_inspection_refusal(reason, peer, &claim.subject, &claim.agent);
                return None;
            }
        };
        let (mut capabilities, mut words) = self.capability_view(&context);
        let memory = self.memory_surface(&context, &claim.agent);
        if memory.is_some() {
            for identifier in [MEMORY_RECENT, MEMORY_SEARCH] {
                let capability = identifier.parse::<CapabilityId>().ok()?;
                capabilities.push(self.available_capability(&capability)?);
            }
            capabilities.sort_by(|left, right| left.capability.id.cmp(&right.capability.id));
            words.push(MEMORY_WORD.to_owned());
            words.sort();
            words.dedup();
        }
        Some((capabilities, words, memory))
    }

    /// Resolves a provider command only after chat-scope and all-three memory authority checks.
    pub async fn resolve_command_for_chat(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        claim: &ChatSessionClaim,
        word: &str,
        argv: &[String],
    ) -> Result<CommandResolution, BrokerHostError> {
        let Ok(context) = self.resolve_chat_claim(peer, grant, claim) else {
            return Err(BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            });
        };
        let memory_provider_word =
            self.registry
                .command_words_by_provider()
                .into_iter()
                .any(|(provider, words)| {
                    provider.as_str() == MEMORY_PROVIDER && words.iter().any(|value| value == word)
                });
        let reserved_nonmemory_provider_word = self
            .registry
            .command_words_by_provider()
            .into_iter()
            .any(|(provider, words)| {
                provider.as_str() != MEMORY_PROVIDER
                    && words.iter().any(|value| value == word)
                    && self.registry.capabilities().any(|(candidate, capability)| {
                        candidate == provider && capability.id.as_str().starts_with("memory.chat.")
                    })
            });
        if (word == MEMORY_WORD && self.memory_surface(&context, &claim.agent).is_none())
            || (memory_provider_word && word != MEMORY_WORD)
            || reserved_nonmemory_provider_word
        {
            return Err(BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            });
        }
        let resolution = self.registry.resolve_command(word, argv).await?;
        if matches!(
            &resolution,
            CommandResolution::Resolved { capability, .. }
                if (word != MEMORY_WORD
                    && is_reserved_memory_route(capability, self.constraints.get(capability)))
                    || (word == MEMORY_WORD
                        && !matches!(capability.as_str(), MEMORY_RECENT | MEMORY_SEARCH))
        ) {
            return Err(BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            });
        }
        Ok(resolution)
    }

    /// Executes one generic proposal under invocation-bound chat authority.
    pub async fn invoke_for_chat(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: &ChatAttestation,
        mut request: InvocationRequest,
    ) -> Result<InvocationResult, BrokerError> {
        let claim = ChatSessionClaim {
            subject: attestation.subject.clone(),
            agent: attestation.agent.clone(),
            scope: attestation.scope.clone(),
        };
        let context = self
            .resolve_chat_claim(peer, grant, &claim)
            .unwrap_or_else(|_| peer.with_refused_subject(attestation.subject.clone()));
        let mut refusal = (attestation.invocation != request.id
            || self.resolve_chat_claim(peer, grant, &claim).is_err())
        .then_some(Refusal {
            reason: "chat-attestation-denied",
            policy_ids: Vec::new(),
        });
        if request.capability.as_str() == MEMORY_RECORD {
            refusal = Some(Refusal {
                reason: "record-operation-required",
                policy_ids: Vec::new(),
            });
        } else if matches!(request.capability.as_str(), MEMORY_RECENT | MEMORY_SEARCH) {
            if self.memory_surface(&context, &attestation.agent).is_none() {
                refusal = Some(Refusal {
                    reason: "memory-unavailable",
                    policy_ids: Vec::new(),
                });
            } else if let Err(reason) = self.curate_memory_input(&mut request) {
                refusal = Some(Refusal {
                    reason,
                    policy_ids: Vec::new(),
                });
            }
        } else if is_reserved_memory_route(
            &request.capability,
            self.constraints.get(&request.capability),
        ) {
            refusal = Some(Refusal {
                reason: "memory-unavailable",
                policy_ids: Vec::new(),
            });
        }
        self.invoke_inner(&context, request, refusal).await
    }

    /// Constructs the hidden record proposal from typed post-acceptance fields only.
    pub async fn record_delivered_turn_for_chat(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: &ChatAttestation,
        turn: DeliveredTurnRequest,
    ) -> Result<InvocationResult, BrokerError> {
        let claim = ChatSessionClaim {
            subject: attestation.subject.clone(),
            agent: attestation.agent.clone(),
            scope: attestation.scope.clone(),
        };
        let context = self
            .resolve_chat_claim(peer, grant, &claim)
            .unwrap_or_else(|_| peer.with_refused_subject(attestation.subject.clone()));
        let refusal = if attestation.invocation != turn.id
            || self.resolve_chat_claim(peer, grant, &claim).is_err()
        {
            Some(Refusal {
                reason: "chat-attestation-denied",
                policy_ids: Vec::new(),
            })
        } else if self.memory_surface(&context, &attestation.agent).is_none() {
            Some(Refusal {
                reason: "memory-unavailable",
                policy_ids: Vec::new(),
            })
        } else if !turn.is_bounded() || !turn.delivery.is_canonical_for(&attestation.scope) {
            Some(Refusal {
                reason: "invalid-turn",
                policy_ids: Vec::new(),
            })
        } else {
            None
        };
        let request = InvocationRequest {
            id: turn.id,
            capability: MEMORY_RECORD
                .parse()
                .expect("reserved memory capability is valid"),
            trace: turn.trace,
            trace_parent: turn.trace_parent,
            secret_use: None,
            input: serde_json::json!({
                "delivery": turn.delivery,
                "user": turn.user,
                "assistant": turn.assistant,
            }),
        };
        self.invoke_inner(&context, request, refusal).await
    }

    /// Derives the chat context, or the stable refusal class that stopped it.
    ///
    /// The class exists so an inspection refusal can be reported once by its caller; the wire
    /// answer stays the same opaque nothing it was.
    fn resolve_chat_claim(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        claim: &ChatSessionClaim,
    ) -> Result<AuthenticatedContext, &'static str> {
        let grant = grant.ok_or("attestation-denied")?;
        let principal = self
            .identities
            .resolve(&claim.subject)
            .ok_or("unmapped-subject")?;
        // `chatScopes` was added for storage namespace authority. An existing subject-only
        // attestor must keep ordinary chat capabilities working after a gateway upgrade starts
        // using the chat operations. It receives the legacy context with no trusted chat scope,
        // which makes the complete durable-memory surface structurally unavailable. Once any
        // chat scope is authored, the service-specific canonical checks and exact grant apply.
        #[allow(
            clippy::map_err_ignore,
            reason = "this function answers with a stable refusal class rather than an error, so \
                      that the wire answer stays the opaque nothing it was; a ContextError here \
                      means the broker's own trusted state disagreed with itself, and it has \
                      nowhere to go that would not tell a refused caller what it must not learn"
        )]
        let context = if grant.chat_scopes.is_empty() {
            if !grant.permits(&claim.subject) {
                return Err("attestation-denied");
            }
            AuthenticatedContext::attested(
                principal.clone(),
                Actor::Agent {
                    agent: claim.agent.clone(),
                },
                peer.principal().clone(),
                claim.subject.clone(),
            )
            .map_err(|_| "attestation-denied")?
        } else {
            if !grant.permits_chat(&claim.subject, &claim.scope) {
                return Err("attestation-denied");
            }
            AuthenticatedContext::attested_chat(
                principal.clone(),
                Actor::Agent {
                    agent: claim.agent.clone(),
                },
                peer.principal().clone(),
                claim.subject.clone(),
                claim.scope.clone(),
            )
            .map_err(|_| "attestation-denied")?
        };
        let decision = self.authorize_agent_prompt(&context, &claim.agent);
        if decision.allowed {
            Ok(context)
        } else {
            Err(denial_reason(&decision, "agent-denied"))
        }
    }

    fn memory_surface(
        &self,
        context: &AuthenticatedContext,
        agent: &AgentId,
    ) -> Option<ChatMemorySurface> {
        let config = self.chat_memory.as_ref()?;
        if !config.enabled_for(agent) || context.chat_scope().is_none() {
            return None;
        }
        for identifier in [MEMORY_RECORD, MEMORY_RECENT, MEMORY_SEARCH] {
            let capability = identifier.parse::<CapabilityId>().ok()?;
            let set = self.constraints.get(&capability)?;
            if !self.authorize_capability(context, &capability, set).allowed {
                return None;
            }
        }
        Some(ChatMemorySurface {
            max_lookback_turns: config.max_lookback_turns,
            prompt_note: memory_prompt_note(config.max_lookback_turns),
        })
    }

    fn available_capability(&self, capability: &CapabilityId) -> Option<AvailableCapability> {
        let set = self.constraints.get(capability)?;
        let (_, manifest) = self.registry.capability(capability)?;
        let mut capability = manifest.clone();
        capability.effect = set.effect;
        capability.risk = set.risk;
        capability.idempotency = set.idempotency;
        Some(AvailableCapability {
            provider: set.provider.clone(),
            capability,
        })
    }

    fn canonical_authority_surface(
        &self,
        context: &AuthenticatedContext,
        interface: StorageInterface,
        memory_route: bool,
    ) -> Result<Vec<u8>, BrokerError> {
        let mut encoded = AuthorityEncoder::new();
        encoded.text("format", "dekopon-authority-surface-v1");
        encoded.text(
            "backend",
            if memory_route {
                "jsonl@0.1.0/chat-memory-format-v1"
            } else {
                match interface {
                    StorageInterface::Jsonl => "jsonl@0.1.0",
                    StorageInterface::DurableFiles => "durable-files@0.1.0/rollback-journal-v1",
                }
            },
        );
        // Principal mapping is not part of continuity. The canonical external subject is already
        // in the base namespace, while this generation commits only the resulting effective
        // authority. Remapping the same subject without changing that surface must not rotate.

        let artifacts = self
            .registry
            .loaded_provider_metadata()
            .map(|metadata| {
                (
                    metadata.manifest.id.clone(),
                    metadata.artifact_sha256.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let effective_memory_surface = memory_route
            && match context.actor() {
                Actor::Agent { agent } => self.memory_surface(context, agent).is_some(),
                Actor::Human { .. } | Actor::Service { .. } => false,
            };
        let effective = self
            .constraints
            .iter()
            .filter(|(capability, set)| {
                let reserved = is_reserved_memory_route(capability, Some(set));
                (!reserved || (effective_memory_surface && is_memory_capability(capability)))
                    && self.authorize_capability(context, capability, set).allowed
            })
            .collect::<Vec<_>>();
        let effective_capabilities = effective
            .iter()
            .map(|(capability, _)| (*capability).clone())
            .collect::<BTreeSet<_>>();
        encoded.number("capabilityCount", effective.len() as u128);
        for (capability, set) in effective {
            encoded.text("capability", capability.as_str());
            encoded.text("provider", set.provider.as_str());
            encoded.byte("effect", effect_tag(set.effect));
            encoded.byte("risk", risk_tag(set.risk));
            encoded.byte("idempotency", idempotency_tag(set.idempotency));
            encoded.optional_text("credential", set.credential_for(context.actor()));
            encode_execution_constraints(&mut encoded, &set.constraints);
            let digest = artifacts
                .get(&set.provider)
                .ok_or(BrokerError::MemoryUnavailable)?;
            encoded.text("providerArtifactSha256", digest);
        }

        let secret_bindings = self
            .secrets
            .authority_bindings()
            .into_iter()
            .filter(|binding| {
                effective_capabilities.contains(&binding.capability)
                    && self
                        .constraints
                        .get(&binding.capability)
                        .is_some_and(|set| {
                            self.authorize_secret_use(
                                context,
                                &binding.capability,
                                set,
                                &binding.proposal(),
                            )
                            .allowed
                        })
            })
            .collect::<Vec<_>>();
        if !secret_bindings.is_empty() {
            encoded.number("secretBindingCount", secret_bindings.len() as u128);
            encoded.optional_text("secretMapRevision", self.secrets.authority_revision());
        }
        for binding in secret_bindings {
            encoded.text("secretBinding", &binding.binding_id);
            encoded.text("secretDrn", binding.secret.as_str());
            encoded.text("secretCapability", binding.capability.as_str());
            encoded.text("secretSink", &binding.sink.to_string());
            encoded.optional_text("secretBasicUsername", binding.basic_username.as_deref());
            let mut hosts = binding.allowed_hosts.clone();
            hosts.sort();
            hosts.dedup();
            encoded.number("secretHostCount", hosts.len() as u128);
            for host in hosts {
                encoded.text("secretHost", &host);
            }
            let mut methods = binding.allowed_methods.clone();
            methods.sort();
            methods.dedup();
            encoded.number("secretMethodCount", methods.len() as u128);
            for method in methods {
                encoded.text("secretMethod", &method);
            }
            let mut paths = binding
                .allowed_paths
                .iter()
                .map(|rule| match rule {
                    dekopon_capability::HttpPathRule::Exact { path } => {
                        format!("exact:{path}")
                    }
                    dekopon_capability::HttpPathRule::SegmentPrefix { path } => {
                        format!("segment-prefix:{path}")
                    }
                })
                .collect::<Vec<_>>();
            paths.sort();
            paths.dedup();
            encoded.number("secretPathCount", paths.len() as u128);
            for path in paths {
                encoded.text("secretPath", &path);
            }
            encoded.boolean("secretAllowQuery", binding.allow_query);
            encoded.number("secretMaxInjections", u128::from(binding.max_injections));
        }

        encode_host_limits(&mut encoded, self.registry.host_limits());

        let storage = self
            .registry
            .storage_host()
            .ok_or(BrokerError::MemoryUnavailable)?;
        encode_storage_limits(&mut encoded, storage.limits());
        if let Some(memory) = self.chat_memory.as_ref().filter(|_| memory_route) {
            encode_memory_config(&mut encoded, memory);
        }
        Ok(encoded.finish())
    }

    fn prepare_storage_grant(
        &self,
        context: &AuthenticatedContext,
        request: &InvocationRequest,
        set: &ConstraintSet,
    ) -> Result<Option<StorageGrantPreparation>, BrokerError> {
        let Some(storage) = &set.constraints.storage else {
            return Ok(None);
        };
        let scope = context.chat_scope().ok_or(BrokerError::MemoryUnavailable)?;
        let subject = context
            .attested_subject()
            .ok_or(BrokerError::MemoryUnavailable)?;
        let agent = match context.actor() {
            Actor::Agent { agent } => agent.clone(),
            Actor::Human { .. } | Actor::Service { .. } => {
                return Err(BrokerError::MemoryUnavailable);
            }
        };
        let memory_route =
            set.provider.as_str() == MEMORY_PROVIDER && is_memory_capability(&request.capability);
        let authority =
            self.canonical_authority_surface(context, storage.interface, memory_route)?;
        let host = self
            .registry
            .storage_host()
            .ok_or(BrokerError::MemoryUnavailable)?;
        let grant_request = StorageGrantRequest::new(
            request.id.clone(),
            request.capability.clone(),
            set.provider.clone(),
            storage.interface,
            storage.access,
            storage.namespace,
            agent,
            subject.clone(),
            scope.kind.to_string(),
            scope.transport.to_string(),
            scope.channel.clone(),
            scope.conversation.clone(),
            if memory_route {
                self.chat_memory
                    .as_ref()
                    .map_or(ContinuityPolicy::AuthorityBound, |config| {
                        config.continuity_policy
                    })
            } else {
                ContinuityPolicy::AuthorityBound
            },
            authority,
        );
        host.prepare_grant(grant_request)
            .map(Some)
            .map_err(|source| BrokerError::Storage { source })
    }

    fn curate_memory_input(&self, request: &mut InvocationRequest) -> Result<(), &'static str> {
        let config = self.chat_memory.as_ref().ok_or("memory-unavailable")?;
        request.input = match request.capability.as_str() {
            MEMORY_RECENT => {
                let last = request
                    .input
                    .as_object()
                    .filter(|object| object.len() == 1)
                    .and_then(|object| object.get("last"))
                    .and_then(serde_json::Value::as_u64)
                    .filter(|last| *last > 0 && *last <= u64::from(config.max_recent_turns))
                    .ok_or("invalid-memory-input")?;
                serde_json::json!({
                    "operation": "recent", "last": last,
                    "maxLookbackTurns": config.max_lookback_turns,
                    "maxRecentTurns": config.max_recent_turns,
                    "maxResultBytes": config.max_result_bytes,
                })
            }
            MEMORY_SEARCH => {
                let query = request
                    .input
                    .as_object()
                    .filter(|object| object.len() == 1)
                    .and_then(|object| object.get("query"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|query| {
                        !query.is_empty() && query.len() as u64 <= config.max_query_bytes
                    })
                    .ok_or("invalid-memory-input")?;
                serde_json::json!({
                    "operation": "search", "query": query,
                    "maxLookbackTurns": config.max_lookback_turns,
                    "maxSearchResults": config.max_search_results,
                    "maxResultBytes": config.max_result_bytes,
                })
            }
            _ => return Err("invalid-memory-input"),
        };
        Ok(())
    }

    /// Derives the context an attested proposal is evaluated — or refused — under.
    fn resolve_attestation(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: &SubjectAttestation,
    ) -> (AuthenticatedContext, Option<&'static str>) {
        if !grant.is_some_and(|grant| grant.permits(&attestation.subject)) {
            return (
                peer.with_refused_subject(attestation.subject.clone()),
                Some("attestation-denied"),
            );
        }
        let Some(principal) = self.identities.resolve(&attestation.subject) else {
            return (
                peer.with_refused_subject(attestation.subject.clone()),
                Some("unmapped-subject"),
            );
        };
        match AuthenticatedContext::attested(
            principal.clone(),
            Actor::Agent {
                agent: attestation.agent.clone(),
            },
            peer.principal().clone(),
            attestation.subject.clone(),
        ) {
            Ok(context) => (context, None),
            // Unreachable while agent actors carry no principal, but a refusal is the only
            // acceptable fallback if that invariant ever changes.
            Err(_) => (
                peer.with_refused_subject(attestation.subject.clone()),
                Some("attestation-denied"),
            ),
        }
    }

    async fn invoke_inner(
        &self,
        context: &AuthenticatedContext,
        request: InvocationRequest,
        refusal: Option<Refusal>,
    ) -> Result<InvocationResult, BrokerError> {
        // Identifiers and the decision only. Request input never reaches a span field, exactly as
        // it never reaches an audit field — a refusal must be visible without the payload that
        // was refused being visible with it.
        let storage_candidate = self.capability_uses_storage(&request.capability);
        let authorize = if storage_candidate {
            tracing::info_span!(
                "broker.authorize",
                invocation = %request.id,
                outcome = tracing::field::Empty,
                policy.errors_present = tracing::field::Empty,
            )
        } else {
            tracing::info_span!(
            "broker.authorize",
            invocation = %request.id,
            capability = %request.capability,
            subject = tracing::field::Empty,
            via = tracing::field::Empty,
            outcome = tracing::field::Empty,
            policy.errors_present = tracing::field::Empty,
            input = tracing::field::Empty,
            )
        };
        if !storage_candidate && let Some(subject) = context.attested_subject() {
            authorize.record("subject", tracing::field::display(subject));
        }
        if !storage_candidate && let Some(via) = context.via() {
            authorize.record("via", tracing::field::display(via));
        }
        // Opt-in only. Provider input is the payload the metadata-only default withholds; a
        // `Redacted` value inside it still renders its marker, because that is a property of the
        // value rather than of this mode.
        if !storage_candidate && dekopon_core::telemetry_payloads() {
            authorize.record("input", tracing::field::display(&request.input));
        }
        // Instrumented rather than entered with a guard: this section awaits the replay ledger and,
        // on every denial, a durable audit append that fsyncs. A guard held across those awaits
        // stays entered on the worker thread while this task is suspended, so another connection's
        // spans parent under this request's authorization and this request's own events lose it
        // when the task resumes elsewhere.
        let authorized = async {
            if !self.replay.reserve(&request.id).await? {
                authorize.record("outcome", "replayed-invocation");
                return self
                    .deny(context, &request, "replayed-invocation", Vec::new())
                    .await
                    .map(ControlFlow::Break);
            }
            // A refused attestation or agent gate still consumes its invocation identifier above:
            // the denial is a decision about this exact proposal, and letting the same identifier
            // come back with a different claim would make the audit trail ambiguous.
            if let Some(refusal) = refusal {
                authorize.record("outcome", refusal.reason);
                return self
                    .deny(context, &request, refusal.reason, refusal.policy_ids)
                    .await
                    .map(ControlFlow::Break);
            }
            // A missing constraint set means there is nothing to execute regardless of what policy
            // says. Under `Leniency::Strict` this is defense in depth behind the startup check;
            // under `Leniency::Tolerant` the startup check is only a warning, so this *is* the
            // enforcement — a capability a policy anticipates but no provider routes dies here.
            // Do not weaken it into an assertion or fold it into the policy decision.
            let Some(mut set) = self.constraints.get(&request.capability).cloned() else {
                authorize.record("outcome", "unconstrained-capability");
                return self
                    .deny(context, &request, "unconstrained-capability", Vec::new())
                    .await
                    .map(ControlFlow::Break);
            };
            if set.constraints.storage.is_some() && context.chat_scope().is_none() {
                authorize.record("outcome", "chat-scope-required");
                return self
                    .deny(context, &request, "chat-scope-required", Vec::new())
                    .await
                    .map(ControlFlow::Break);
            }
            let decision = self.authorize_capability(context, &request.capability, &set);
            // A policy that errors at evaluation time denies exactly like a policy that does not
            // match, so the flag is the only thing that separates a broken rule from a working one.
            // It is a flag rather than the error text on purpose: an explanation must not become a
            // per-request channel for policy source or entity data.
            authorize.record("policy.errors_present", decision.errors_present);
            if !decision.allowed {
                let reason = denial_reason(&decision, "policy-denied");
                authorize.record("outcome", reason);
                if decision.errors_present {
                    // The invocation identifier only: it joins this event to the authorize span,
                    // which is where the capability lives when the route may carry one.
                    tracing::warn!(
                        event = "broker_policy_evaluation_error",
                        invocation = %request.id,
                        policy.target = "capability",
                    );
                }
                // A refusal means policy never ran: the broker asked a question the schema does
                // not admit, which is a deployment defect rather than an authorization outcome.
                // It presents as an ordinary `policy-denied` on the wire — the audit reason stays
                // stable and the denial is still a denial — so this event is the only place the
                // operator learns the difference.
                if let Some(cause) = &decision.refusal {
                    tracing::warn!(
                        target: "dekopon_broker::audit",
                        {
                            audit.event = "policy.request.refused",
                            capability.id = %request.capability,
                            error.reason = %cause,
                        },
                        "policy request could not be constructed"
                    );
                }
                return self
                    .deny(context, &request, reason, decision.determining_policy_ids)
                    .await
                    .map(ControlFlow::Break);
            }
            let mut policy_ids = decision.determining_policy_ids;
            if let Some(secret_use) = request.secret_use.as_ref() {
                let Some(binding) = self
                    .secrets
                    .binding(&request.capability, secret_use)
                    .cloned()
                else {
                    authorize.record("outcome", "secret-denied");
                    return self
                        .deny(context, &request, "secret-denied", policy_ids)
                        .await
                        .map(ControlFlow::Break);
                };
                let secret_decision =
                    self.authorize_secret_use(context, &request.capability, &set, secret_use);
                authorize.record(
                    "policy.errors_present",
                    decision.errors_present || secret_decision.errors_present,
                );
                policy_ids.extend(secret_decision.determining_policy_ids.clone());
                policy_ids.sort();
                policy_ids.dedup();
                if !secret_decision.allowed {
                    authorize.record("outcome", "secret-denied");
                    if secret_decision.errors_present {
                        tracing::warn!(
                            event = "broker_policy_evaluation_error",
                            invocation = %request.id,
                            policy.target = "secret",
                        );
                    }
                    return self
                        .deny(context, &request, "secret-denied", policy_ids)
                        .await
                        .map(ControlFlow::Break);
                }
                set.constraints.secret_use = Some(self.secrets.grant(&binding));
            } else {
                // Secret grants are derived only from a typed proposal after the second policy
                // decision; owner-authored constraint YAML cannot make one ambient.
                set.constraints.secret_use = None;
            }
            authorize.record("outcome", "allowed");
            Ok(ControlFlow::Continue((set, policy_ids)))
        }
        .instrument(authorize.clone())
        .await?;
        let (set, policy_ids) = match authorized {
            ControlFlow::Break(denied) => return Ok(denied),
            ControlFlow::Continue(allowed) => allowed,
        };
        let provider = set.provider.clone();
        let execute = if set.constraints.storage.is_some() {
            tracing::info_span!(
                "broker.execute",
                storage = true,
                outcome = tracing::field::Empty,
                error = tracing::field::Empty,
            )
        } else {
            tracing::info_span!(
                "broker.execute",
                provider = %provider,
                credential = tracing::field::Empty,
                outcome = tracing::field::Empty,
                error = tracing::field::Empty,
            )
        };
        // The symbolic name only, exactly as the audit record carries it: once one capability can
        // present two credentials, a trace that names neither cannot say which organization a
        // write reached. `Redacted` keeps the value itself out of both.
        if let Some(secret) = request.secret_use.as_ref() {
            execute.record("credential", secret.secret().as_str());
        } else if let Some(credential) = set.credential_for(context.actor()) {
            execute.record("credential", credential);
        }
        self.execute(context, request, set, policy_ids)
            .instrument(execute)
            .await
    }

    async fn deny(
        &self,
        context: &AuthenticatedContext,
        request: &InvocationRequest,
        reason: &'static str,
        policy_ids: Vec<String>,
    ) -> Result<InvocationResult, BrokerError> {
        let decision_id = format!("deny-{}", request.id);
        let decision = self.decision_reference(&decision_id);
        let material = DecisionMaterial {
            invocation: &request.id,
            trace: &request.trace,
            principal: context.principal(),
            actor: context.actor(),
            via: context.via(),
            attested_subject: context.attested_subject(),
            capability: &request.capability,
            secret_use: request.secret_use.as_ref(),
            provider: None,
            authorized_by: &self.broker_principal,
            policy_revision: &self.policy_revision,
            policy_ids: &policy_ids,
            policy_digest: &self.policy_digest,
            constraints: None,
            allowed: false,
            reason: Some(reason),
        };
        let storage_host = self.registry.storage_host();
        let storage_backed =
            storage_host.is_some() && self.capability_uses_storage(&request.capability);
        let digest = if storage_backed {
            let bytes = serde_json::to_vec(&material)
                .map_err(|source| BrokerError::DecisionEvidence { source })?;
            storage_host
                .as_ref()
                .expect("storage_backed proves a configured host")
                .evidence_commitment("policy-decision", &bytes)
        } else {
            decision_evidence_digest("policy-decision", &material)?
        };
        self.audit
            .append(AuditEvent::Decision {
                invocation: request.id.clone(),
                trace: request.trace.clone(),
                principal: (!storage_backed).then(|| context.principal().clone()),
                actor: (!storage_backed).then(|| context.actor().clone()),
                via: (!storage_backed).then(|| context.via().cloned()).flatten(),
                attested_subject: (!storage_backed)
                    .then(|| context.attested_subject().cloned())
                    .flatten(),
                capability: request.capability.clone(),
                secret: (!storage_backed)
                    .then(|| {
                        request
                            .secret_use
                            .as_ref()
                            .map(|secret| secret.secret().clone())
                    })
                    .flatten(),
                secret_sink: (!storage_backed)
                    .then(|| request.secret_use.as_ref().map(SecretUseProposal::sink))
                    .flatten(),
                provider: None,
                authorized_by: (!storage_backed).then(|| self.broker_principal.clone()),
                decision_id: decision_id.clone(),
                policy_revision: (!storage_backed).then(|| self.policy_revision.clone()),
                policy_ids: if storage_backed {
                    Vec::new()
                } else {
                    policy_ids
                },
                policy_digest: (!storage_backed).then(|| self.policy_digest.clone()),
                allowed: false,
                reason: Some(reason.to_owned()),
                decision_digest: digest.clone(),
                storage_scope_commitment: None,
                storage: None,
            })
            .await
            .map_err(|source| {
                report_audit_failure("decision", &request.id, &source);
                BrokerError::DecisionAudit { source }
            })?;
        Ok(InvocationResult {
            invocation: request.id.clone(),
            decision,
            outcome: InvocationOutcome::Denied,
            output: None,
            error: Some(reason.to_owned()),
            evidence: vec![Evidence {
                kind: "policy-decision".to_owned(),
                digest,
                media_type: POLICY_EVIDENCE_MEDIA_TYPE.to_owned(),
                uri: None,
            }],
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the terminal record takes the exact authorization identity and policy material already in scope; bundling it would create a second partial invocation type"
    )]
    async fn fail_authorized_before_provider(
        &self,
        context: &AuthenticatedContext,
        invocation: &InvocationId,
        trace: &TraceId,
        capability: &CapabilityId,
        decision_id: &str,
        decision: DecisionReference,
        set: &ConstraintSet,
        credential: Option<&str>,
        policy_ids: &[String],
        policy_evidence: Evidence,
        reason: &'static str,
    ) -> Result<InvocationResult, BrokerError> {
        let event = execution_event(
            context,
            invocation,
            trace,
            capability,
            decision_id,
            &self.policy_revision,
            policy_ids,
            &self.policy_digest,
            &self.broker_principal,
            set,
            credential,
            InvocationOutcome::Failed,
            0,
            Some(reason.to_owned()),
            None,
            Vec::new(),
            None,
            None,
        );
        let execution = tracing::Span::current();
        execution.record("outcome", "failed");
        execution.record("error", reason);
        self.audit.append(event).await.map_err(|source| {
            execution.record("outcome", "authorized-failure-unaudited");
            execution.record("error", source.category());
            report_audit_failure("authorized-failure", invocation, &source);
            BrokerError::AuthorizedFailureAudit { source }
        })?;
        Ok(InvocationResult {
            invocation: invocation.clone(),
            decision,
            outcome: InvocationOutcome::Failed,
            output: None,
            error: Some(reason.to_owned()),
            evidence: vec![policy_evidence],
        })
    }

    async fn execute(
        &self,
        context: &AuthenticatedContext,
        mut request: InvocationRequest,
        set: ConstraintSet,
        policy_ids: Vec<String>,
    ) -> Result<InvocationResult, BrokerError> {
        // Keyed scope/evidence preparation is deliberately non-mutating. The authorization
        // decision is durably appended before `materialize` may create a namespace, rotate a
        // generation pointer, or update lifecycle state.
        let mut storage_preparation = self.prepare_storage_grant(context, &request, &set)?;
        let storage_scope_commitment = storage_preparation
            .as_ref()
            .map(StorageGrantPreparation::scope_commitment);
        if request.capability.as_str() == MEMORY_RECORD {
            let config = self
                .chat_memory
                .as_ref()
                .ok_or(BrokerError::MemoryUnavailable)?;
            let object = request
                .input
                .as_object()
                .filter(|object| object.len() == 3)
                .ok_or(BrokerError::InvalidMemoryInput)?;
            #[allow(
                clippy::map_err_ignore,
                reason = "not wire input: every externally reachable entry point refuses \
                          MEMORY_RECORD, so the only proposal reaching here is the one \
                          `record_delivered_turn_for_chat` builds from a typed DeliveryIdentity \
                          — this is that value's own round trip, and serde has no malformed \
                          input to name"
            )]
            let delivery: DeliveryIdentity = object
                .get("delivery")
                .cloned()
                .ok_or(BrokerError::InvalidMemoryInput)
                .and_then(|value| {
                    serde_json::from_value(value).map_err(|_| BrokerError::InvalidMemoryInput)
                })?;
            #[allow(
                clippy::map_err_ignore,
                reason = "`DeliveryIdentity` is a derived-Serialize enum of strings and integers, \
                          so serde_json has no failure to describe"
            )]
            let delivery =
                serde_json::to_vec(&delivery).map_err(|_| BrokerError::InvalidMemoryInput)?;
            let user = object
                .get("user")
                .and_then(serde_json::Value::as_str)
                .ok_or(BrokerError::InvalidMemoryInput)?;
            let assistant = object
                .get("assistant")
                .and_then(serde_json::Value::as_str)
                .ok_or(BrokerError::InvalidMemoryInput)?;
            let preparation = storage_preparation
                .as_ref()
                .ok_or(BrokerError::MemoryUnavailable)?;
            request.input = serde_json::json!({
                "operation": "record",
                "id": preparation.record_id(&delivery),
                "commitment": preparation.content_commitment(user, assistant),
                "user": user,
                "assistant": assistant,
                "maxTurnBytes": config.max_turn_bytes,
                "maxLookbackTurns": config.max_lookback_turns,
                "maxDedupRecords": config.max_dedup_records,
                "maxDedupBytes": config.max_dedup_bytes,
                "compactionTargetBytes": config.compaction_target_bytes,
                "compactionThresholdBytes": config.compaction_threshold_bytes,
            });
        }
        let decision_id = format!("allow-{}", request.id);
        let decision = self.decision_reference(&decision_id);
        let invocation_id = request.id.clone();
        let trace = request.trace.clone();
        let capability = request.capability.clone();
        let secret_use = request.secret_use.take();
        let proposal = ProposedInvocation::new(
            request.id,
            request.capability,
            context.actor().clone(),
            request.trace,
            request.input,
        )
        .with_secret_use(secret_use);
        let authorized = self
            .gate
            .authorize(
                proposal,
                set.provider.clone(),
                decision_id.clone(),
                self.broker_principal.clone(),
                self.policy_revision.clone(),
                set.constraints.clone(),
            )
            .map_err(|source| BrokerError::Authorization { source })?;
        let decision_digest = if let Some(preparation) = storage_preparation.as_ref() {
            let bytes = serde_json::to_vec(&authorized)
                .map_err(|source| BrokerError::DecisionEvidence { source })?;
            preparation.evidence_commitment("authorized-invocation", &bytes)
        } else {
            decision_evidence_digest("authorized-invocation", &authorized)?
        };
        let policy_evidence = Evidence {
            kind: "policy-decision".to_owned(),
            digest: decision_digest.clone(),
            media_type: POLICY_EVIDENCE_MEDIA_TYPE.to_owned(),
            uri: None,
        };

        self.audit
            .append(AuditEvent::Decision {
                invocation: invocation_id.clone(),
                trace: trace.clone(),
                principal: storage_scope_commitment
                    .is_none()
                    .then(|| context.principal().clone()),
                actor: storage_scope_commitment
                    .is_none()
                    .then(|| context.actor().clone()),
                via: storage_scope_commitment
                    .is_none()
                    .then(|| context.via().cloned())
                    .flatten(),
                attested_subject: storage_scope_commitment
                    .is_none()
                    .then(|| context.attested_subject().cloned())
                    .flatten(),
                capability: capability.clone(),
                secret: authorized
                    .proposal()
                    .secret_use
                    .as_ref()
                    .map(|secret| secret.secret().clone()),
                secret_sink: authorized
                    .proposal()
                    .secret_use
                    .as_ref()
                    .map(SecretUseProposal::sink),
                provider: storage_scope_commitment
                    .is_none()
                    .then(|| set.provider.clone()),
                authorized_by: storage_scope_commitment
                    .is_none()
                    .then(|| self.broker_principal.clone()),
                decision_id: decision_id.clone(),
                policy_revision: storage_scope_commitment
                    .is_none()
                    .then(|| self.policy_revision.clone()),
                policy_ids: if storage_scope_commitment.is_some() {
                    Vec::new()
                } else {
                    policy_ids.clone()
                },
                policy_digest: storage_scope_commitment
                    .is_none()
                    .then(|| self.policy_digest.clone()),
                allowed: true,
                reason: None,
                decision_digest,
                storage_scope_commitment: storage_scope_commitment.clone(),
                storage: None,
            })
            .await
            .map_err(|source| {
                tracing::Span::current().record("outcome", "decision-unaudited");
                tracing::Span::current().record("error", source.category());
                report_audit_failure("decision", &invocation_id, &source);
                BrokerError::DecisionAudit { source }
            })?;

        let storage_grant = match storage_preparation.take() {
            None => None,
            Some(preparation) => Some(
                tokio::task::spawn_blocking(move || preparation.materialize())
                    .await
                    .map_err(|source| BrokerError::StorageTask { source })?
                    .map_err(|source| BrokerError::Storage { source })?,
            ),
        };

        // Legacy credentials remain owner-selected by capability/agent. A public DRN is different:
        // it was untrusted proposal data, passed a separate Cedar decision, matched an owner binding,
        // and is now resolved exactly once for this invocation. The provider receives neither name
        // nor bytes; only the native HTTP context receives the rendered credential beside the
        // authorization that commits to the same DRN/sink/binding.
        let proposed_secret = authorized.proposal().secret_use.clone();
        let legacy_credential_name = set.credential_for(context.actor()).map(str::to_owned);
        let audit_credential = proposed_secret
            .is_none()
            .then_some(legacy_credential_name.as_deref())
            .flatten();
        let credential = if let Some(proposal) = proposed_secret {
            let material = match self.secrets.resolver.resolve(proposal.secret()).await {
                Ok(material) => material,
                Err(source) => {
                    tracing::warn!(
                        event = "broker_secret_resolution_failed",
                        invocation = %invocation_id,
                        category = source.category,
                    );
                    return self
                        .fail_authorized_before_provider(
                            context,
                            &invocation_id,
                            &trace,
                            &capability,
                            &decision_id,
                            decision,
                            &set,
                            audit_credential,
                            &policy_ids,
                            policy_evidence,
                            "secret-resolution",
                        )
                        .await;
                }
            };
            let Some(grant) = authorized.constraints().secret_use.as_ref() else {
                return self
                    .fail_authorized_before_provider(
                        context,
                        &invocation_id,
                        &trace,
                        &capability,
                        &decision_id,
                        decision,
                        &set,
                        audit_credential,
                        &policy_ids,
                        policy_evidence,
                        "secret-authorization",
                    )
                    .await;
            };
            let built = match proposal.sink() {
                SecretSinkKind::HttpBearer => {
                    BoundCredential::secret_bearer(material.into_secret_bytes(), grant)
                }
                SecretSinkKind::HttpBasic => {
                    BoundCredential::secret_basic(material.into_secret_bytes(), grant)
                }
            };
            match built {
                Ok(credential) => Some(credential),
                Err(source) => {
                    tracing::warn!(
                        event = "broker_secret_credential_failed",
                        invocation = %invocation_id,
                        category = "invalid-material",
                        cause_type = std::any::type_name_of_val(&source),
                    );
                    return self
                        .fail_authorized_before_provider(
                            context,
                            &invocation_id,
                            &trace,
                            &capability,
                            &decision_id,
                            decision,
                            &set,
                            audit_credential,
                            &policy_ids,
                            policy_evidence,
                            "secret-credential",
                        )
                        .await;
                }
            }
        } else {
            legacy_credential_name
                .as_deref()
                .and_then(|name| self.credentials.get(name))
                .cloned()
        };
        let started = Instant::now();
        let execution = self
            .registry
            .invoke_with_storage(authorized, credential, storage_grant)
            .await;
        let duration_ms = duration_millis(started.elapsed());
        let (result, audit_event) = match execution {
            Err(failure)
                if matches!(
                    failure.error.as_ref(),
                    BrokerHostError::Storage { source }
                        if matches!(
                            source,
                            dekopon_storage_host::StorageHostError::OutcomeUnaudited
                        )
                ) =>
            {
                return Err(BrokerError::StorageOutcome {
                    invocation: invocation_id,
                });
            }
            Ok(output) => {
                let output_digest = output.storage.as_ref().map_or_else(
                    || outcome_evidence_digest(&invocation_id, "provider-response", &output.output),
                    |storage| {
                        Ok(storage
                            .output_commitment
                            .clone()
                            .unwrap_or_else(|| storage.evidence_commitment.clone()))
                    },
                )?;
                let mut evidence = vec![policy_evidence];
                evidence.push(Evidence {
                    kind: "provider-response".to_owned(),
                    digest: output_digest.clone(),
                    media_type: PROVIDER_EVIDENCE_MEDIA_TYPE.to_owned(),
                    uri: None,
                });
                if let Some(storage) = &output.storage {
                    evidence.push(Evidence {
                        kind: "storage".to_owned(),
                        digest: storage.evidence_commitment.clone(),
                        media_type: STORAGE_EVIDENCE_MEDIA_TYPE.to_owned(),
                        uri: None,
                    });
                }
                if !output.http_calls.is_empty() {
                    evidence.push(Evidence {
                        kind: "http-calls".to_owned(),
                        digest: outcome_evidence_digest(
                            &invocation_id,
                            "http-calls",
                            &output.http_calls,
                        )?,
                        media_type: HTTP_EVIDENCE_MEDIA_TYPE.to_owned(),
                        uri: None,
                    });
                }
                let event = execution_event(
                    context,
                    &invocation_id,
                    &trace,
                    &capability,
                    &decision_id,
                    &self.policy_revision,
                    &policy_ids,
                    &self.policy_digest,
                    &self.broker_principal,
                    &set,
                    audit_credential,
                    InvocationOutcome::Succeeded,
                    duration_ms,
                    None,
                    Some(output_digest),
                    output.http_calls,
                    storage_scope_commitment.clone(),
                    output.storage,
                );
                (
                    InvocationResult {
                        invocation: invocation_id.clone(),
                        decision: decision.clone(),
                        outcome: InvocationOutcome::Succeeded,
                        output: Some(output.output),
                        error: None,
                        evidence,
                    },
                    event,
                )
            }
            Err(failure) => {
                let error = public_host_error(&failure.error).to_owned();
                // A failure can follow calls that already left the host; their sanitized
                // metadata belongs in the terminal record exactly as it would on success.
                let mut evidence = vec![policy_evidence];
                if let Some(storage) = &failure.storage {
                    evidence.push(Evidence {
                        kind: "storage".to_owned(),
                        digest: storage.evidence_commitment.clone(),
                        media_type: STORAGE_EVIDENCE_MEDIA_TYPE.to_owned(),
                        uri: None,
                    });
                }
                if !failure.http_calls.is_empty() {
                    evidence.push(Evidence {
                        kind: "http-calls".to_owned(),
                        digest: outcome_evidence_digest(
                            &invocation_id,
                            "http-calls",
                            &failure.http_calls,
                        )?,
                        media_type: HTTP_EVIDENCE_MEDIA_TYPE.to_owned(),
                        uri: None,
                    });
                }
                let event = execution_event(
                    context,
                    &invocation_id,
                    &trace,
                    &capability,
                    &decision_id,
                    &self.policy_revision,
                    &policy_ids,
                    &self.policy_digest,
                    &self.broker_principal,
                    &set,
                    audit_credential,
                    InvocationOutcome::Failed,
                    duration_ms,
                    Some(error.clone()),
                    None,
                    failure.http_calls,
                    storage_scope_commitment.clone(),
                    failure.storage,
                );
                (
                    InvocationResult {
                        invocation: invocation_id.clone(),
                        decision,
                        outcome: InvocationOutcome::Failed,
                        output: None,
                        error: Some(error),
                        evidence,
                    },
                    event,
                )
            }
        };

        // The same sanitized pair the terminal audit record carries. A trace that ends at "the
        // provider ran" cannot say whether the effect worked, and `error` here is the classified
        // reason the client is already told, never provider output.
        let execution = tracing::Span::current();
        execution.record(
            "outcome",
            if matches!(result.outcome, InvocationOutcome::Succeeded) {
                "succeeded"
            } else {
                "failed"
            },
        );
        if let Some(error) = result.error.as_deref() {
            execution.record("error", error);
        }

        self.audit.append(audit_event).await.map_err(|source| {
            execution.record("outcome", "outcome-unaudited");
            execution.record("error", source.category());
            report_audit_failure("outcome", &invocation_id, &source);
            BrokerError::OutcomeAudit {
                invocation: invocation_id.clone(),
                source,
            }
        })?;
        Ok(result)
    }

    fn decision_reference(&self, decision_id: &str) -> DecisionReference {
        DecisionReference {
            decision_id: decision_id.to_owned(),
            authorized_by: self.broker_principal.clone(),
            policy_revision: self.policy_revision.clone(),
        }
    }
}

/// Versioned length-prefixed authority encoding. Every field has a fixed label and binary value;
/// no YAML/JSON formatting, map insertion order, path, or telemetry setting can affect it.
struct AuthorityEncoder {
    bytes: Vec<u8>,
}

impl AuthorityEncoder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn field(&mut self, label: &str, value: &[u8]) {
        self.bytes
            .extend_from_slice(&(label.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(label.as_bytes());
        self.bytes
            .extend_from_slice(&(value.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(value);
    }

    fn text(&mut self, label: &str, value: &str) {
        self.field(label, value.as_bytes());
    }

    fn number(&mut self, label: &str, value: u128) {
        self.field(label, &value.to_be_bytes());
    }

    fn byte(&mut self, label: &str, value: u8) {
        self.field(label, &[value]);
    }

    fn boolean(&mut self, label: &str, value: bool) {
        self.byte(label, u8::from(value));
    }

    fn optional_text(&mut self, label: &str, value: Option<&str>) {
        match value {
            Some(value) => {
                self.byte(&format!("{label}.present"), 1);
                self.text(label, value);
            }
            None => self.byte(&format!("{label}.present"), 0),
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

fn effect_tag(value: EffectKind) -> u8 {
    match value {
        EffectKind::ReadOnly => 0,
        EffectKind::LocalWrite => 1,
        EffectKind::ExternalWrite => 2,
    }
}

fn risk_tag(value: RiskLevel) -> u8 {
    match value {
        RiskLevel::Low => 0,
        RiskLevel::Medium => 1,
        RiskLevel::High => 2,
        RiskLevel::Critical => 3,
    }
}

fn idempotency_tag(value: Idempotency) -> u8 {
    match value {
        Idempotency::Idempotent => 0,
        Idempotency::Conditional => 1,
        Idempotency::NonIdempotent => 2,
    }
}

fn encode_execution_constraints(
    encoded: &mut AuthorityEncoder,
    constraints: &ExecutionConstraints,
) {
    encoded.number("execution.timeoutMs", u128::from(constraints.timeout_ms));
    encoded.number(
        "execution.maxOutputBytes",
        u128::from(constraints.max_output_bytes),
    );
    if let Some(http) = &constraints.http {
        encoded.byte("execution.http.present", 1);
        let hosts = http.allowed_hosts.iter().collect::<BTreeSet<_>>();
        encoded.number("execution.http.allowedHostCount", hosts.len() as u128);
        for host in hosts {
            encoded.text("execution.http.allowedHost", host);
        }
        let methods = http.allowed_methods.iter().collect::<BTreeSet<_>>();
        encoded.number("execution.http.allowedMethodCount", methods.len() as u128);
        for method in methods {
            encoded.text("execution.http.allowedMethod", method);
        }
        encoded.number("execution.http.maxRequests", u128::from(http.max_requests));
        encoded.number(
            "execution.http.maxRequestBytes",
            u128::from(http.max_request_bytes),
        );
        encoded.number(
            "execution.http.maxResponseBytes",
            u128::from(http.max_response_bytes),
        );
        encoded.boolean(
            "execution.http.allowPlaintextLoopback",
            http.allow_plaintext_loopback,
        );
    } else {
        encoded.byte("execution.http.present", 0);
    }
    if let Some(storage) = &constraints.storage {
        encoded.byte("execution.storage.present", 1);
        encoded.byte(
            "execution.storage.interface",
            match storage.interface {
                StorageInterface::Jsonl => 0,
                StorageInterface::DurableFiles => 1,
            },
        );
        encoded.byte(
            "execution.storage.access",
            match storage.access {
                StorageAccess::ReadOnly => 0,
                StorageAccess::ReadWrite => 1,
            },
        );
        encoded.byte(
            "execution.storage.namespace",
            match storage.namespace {
                StorageNamespace::Chat => 0,
            },
        );
    } else {
        encoded.byte("execution.storage.present", 0);
    }
}

fn encode_host_limits(
    encoded: &mut AuthorityEncoder,
    limits: &dekopon_broker_host::BrokerHostLimits,
) {
    encoded.number("host.maxMemoryBytes", limits.max_memory_bytes as u128);
    encoded.number("host.maxTableElements", limits.max_table_elements as u128);
    encoded.number("host.maxInstances", limits.max_instances as u128);
    encoded.number("host.maxTables", limits.max_tables as u128);
    encoded.number("host.maxMemories", limits.max_memories as u128);
    encoded.number("host.maxInputBytes", limits.max_input_bytes as u128);
    encoded.number("host.maxOutputBytes", limits.max_output_bytes as u128);
    encoded.number("host.maxHttpRequests", u128::from(limits.max_http_requests));
    encoded.number(
        "host.maxHttpRequestBytes",
        u128::from(limits.max_http_request_bytes),
    );
    encoded.number(
        "host.maxHttpResponseBytes",
        u128::from(limits.max_http_response_bytes),
    );
    encoded.number("host.maxHttpHeaders", limits.max_http_headers as u128);
    encoded.number(
        "host.maxHttpHeaderBytes",
        limits.max_http_header_bytes as u128,
    );
    encoded.number("host.fuel", u128::from(limits.fuel));
    encoded.number("host.maxTimeoutNanos", limits.max_timeout.as_nanos());
}

fn encode_memory_config(encoded: &mut AuthorityEncoder, memory: &ChatMemoryConfig) {
    encoded.byte(
        "memory.continuityPolicy",
        match memory.continuity_policy {
            ContinuityPolicy::Stable => 0,
            ContinuityPolicy::AuthorityBound => 1,
        },
    );
    encoded.number(
        "memory.maxLookbackTurns",
        u128::from(memory.max_lookback_turns),
    );
    encoded.number("memory.maxRecentTurns", u128::from(memory.max_recent_turns));
    encoded.number(
        "memory.maxSearchResults",
        u128::from(memory.max_search_results),
    );
    encoded.number("memory.maxQueryBytes", u128::from(memory.max_query_bytes));
    encoded.number("memory.maxResultBytes", u128::from(memory.max_result_bytes));
    encoded.number("memory.maxTurnBytes", u128::from(memory.max_turn_bytes));
    encoded.number(
        "memory.maxDedupRecords",
        u128::from(memory.max_dedup_records),
    );
    encoded.number("memory.maxDedupBytes", u128::from(memory.max_dedup_bytes));
    encoded.number(
        "memory.compactionTargetBytes",
        u128::from(memory.compaction_target_bytes),
    );
    encoded.number(
        "memory.compactionThresholdBytes",
        u128::from(memory.compaction_threshold_bytes),
    );
}

fn encode_storage_limits(
    encoded: &mut AuthorityEncoder,
    limits: &dekopon_storage_host::StorageLimits,
) {
    for (label, value) in [
        ("storage.maxRootBytes", limits.max_root_bytes),
        ("storage.maxNamespaces", limits.max_namespaces),
        ("storage.maxNamespaceBytes", limits.max_namespace_bytes),
        (
            "storage.maxFilesPerNamespace",
            limits.max_files_per_namespace,
        ),
        ("storage.maxFileBytes", limits.max_file_bytes),
        ("storage.maxOpenHandles", limits.max_open_handles),
        (
            "storage.maxHandlesPerInvocation",
            limits.max_handles_per_invocation,
        ),
        (
            "storage.maxHostCallsPerInvocation",
            limits.max_host_calls_per_invocation,
        ),
        (
            "storage.maxReadBytesPerCall",
            limits.max_read_bytes_per_call,
        ),
        (
            "storage.maxReadBytesPerInvocation",
            limits.max_read_bytes_per_invocation,
        ),
        (
            "storage.maxWriteBytesPerCall",
            limits.max_write_bytes_per_call,
        ),
        (
            "storage.maxWriteBytesPerInvocation",
            limits.max_write_bytes_per_invocation,
        ),
        (
            "storage.maxEntropyBytesPerCall",
            limits.max_entropy_bytes_per_call,
        ),
        (
            "storage.maxEntropyBytesPerInvocation",
            limits.max_entropy_bytes_per_invocation,
        ),
        ("storage.lockTimeoutMs", limits.lock_timeout_ms),
        (
            "storage.finalizationBudgetMs",
            limits.finalization_budget_ms,
        ),
        (
            "storage.maxPendingTransactions",
            limits.max_pending_transactions,
        ),
        ("storage.startupMaxEntries", limits.startup_max_entries),
        (
            "storage.startupMaxTransactions",
            limits.startup_max_transactions,
        ),
        (
            "storage.maxQuarantinedNamespaces",
            limits.max_quarantined_namespaces,
        ),
        (
            "storage.retiredGenerationGraceMs",
            limits.retired_generation_grace_ms,
        ),
        (
            "storage.retiredGenerationTtlMs",
            limits.retired_generation_ttl_ms,
        ),
        (
            "storage.inactiveNamespaceTtlMs",
            limits.inactive_namespace_ttl_ms,
        ),
        ("storage.gcIntervalMs", limits.gc_interval_ms),
        (
            "storage.gcMaxNamespacesPerPass",
            limits.gc_max_namespaces_per_pass,
        ),
        ("storage.gcMaxBytesPerPass", limits.gc_max_bytes_per_pass),
    ] {
        encoded.number(label, u128::from(value));
    }
}

/// A refusal decided before policy evaluation, carried into the audited denial.
struct Refusal {
    reason: &'static str,
    policy_ids: Vec<String>,
}

/// Separates a policy that could not be evaluated from one that simply did not match.
///
/// Both deny, and until now both denied identically in audit and telemetry, so a Cedar evaluation
/// error — an extension call on a malformed value, say — was indistinguishable from a clean
/// no-match by anything an operator can read.
const fn denial_reason(decision: &PolicyDecision, denied: &'static str) -> &'static str {
    if decision.errors_present {
        "policy-error"
    } else {
        denied
    }
}

/// Names why an attested inspection saw nothing, on the broker's own side of the socket.
///
/// The response stays opaque — it must not tell a refused caller whether the subject is mapped —
/// so this event is the only place the refusal class and the canonical subject meet. It is what
/// makes an unmapped sender diagnosable without a payload-carrying gateway span.
fn report_inspection_refusal(
    reason: &'static str,
    peer: &AuthenticatedContext,
    subject: &ExternalSubject,
    agent: &AgentId,
) {
    tracing::warn!(
        event = "broker_capabilities_refused",
        reason,
        subject = %subject,
        agent = %agent,
        via = %peer.principal(),
    );
}

/// Reports why the broker could not durably account for a decision or an outcome.
///
/// This is the most consequential failure the broker can have and it used to be anonymous: the
/// wire code and the connection log both carried a category with no cause, so a bounded log
/// reaching its limit, a poisoned handle, and a full filesystem all read the same.
fn report_audit_failure(stage: &'static str, invocation: &InvocationId, source: &AuditError) {
    tracing::error!(
        event = "broker_audit_append_failed",
        audit.stage = stage,
        category = source.category(),
        invocation = %invocation,
        error = %error_chain(source),
    );
}

/// Renders an error and its sources as one `a: b: c` line.
///
/// The chain is the point: `AuditError::Io`'s own message says only that a durable append failed,
/// and the errno that says *why* — `ENOSPC`, the deployment's named top risk — lives one level
/// down.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(current) = source {
        rendered.push_str(": ");
        rendered.push_str(&current.to_string());
        source = current.source();
    }
    rendered
}

#[allow(
    clippy::too_many_arguments,
    reason = "audit construction keeps every trusted correlation field explicit"
)]
fn execution_event(
    context: &AuthenticatedContext,
    invocation: &InvocationId,
    trace: &TraceId,
    capability: &CapabilityId,
    decision_id: &str,
    policy_revision: &str,
    policy_ids: &[String],
    policy_digest: &str,
    authorized_by: &PrincipalId,
    set: &ConstraintSet,
    credential: Option<&str>,
    outcome: InvocationOutcome,
    duration_ms: u64,
    error: Option<String>,
    output_digest: Option<String>,
    http_calls: Vec<HttpCallEvidence>,
    storage_scope_commitment: Option<StorageScopeCommitment>,
    storage: Option<StorageEvidence>,
) -> AuditEvent {
    let storage_backed = storage_scope_commitment.is_some();
    AuditEvent::Execution {
        invocation: invocation.clone(),
        trace: trace.clone(),
        principal: (!storage_backed).then(|| context.principal().clone()),
        actor: (!storage_backed).then(|| context.actor().clone()),
        via: (!storage_backed).then(|| context.via().cloned()).flatten(),
        attested_subject: (!storage_backed)
            .then(|| context.attested_subject().cloned())
            .flatten(),
        capability: capability.clone(),
        secret: set
            .constraints
            .secret_use
            .as_ref()
            .map(|secret| secret.secret.clone()),
        secret_sink: set
            .constraints
            .secret_use
            .as_ref()
            .map(|secret| secret.sink),
        provider: (!storage_backed).then(|| set.provider.clone()),
        authorized_by: (!storage_backed).then(|| authorized_by.clone()),
        decision_id: decision_id.to_owned(),
        policy_revision: (!storage_backed).then(|| policy_revision.to_owned()),
        policy_ids: if storage_backed {
            Vec::new()
        } else {
            policy_ids.to_vec()
        },
        policy_digest: (!storage_backed).then(|| policy_digest.to_owned()),
        effect: set.effect,
        risk: set.risk,
        idempotency: set.idempotency,
        credential: (!storage_backed)
            .then(|| credential.map(str::to_owned))
            .flatten(),
        outcome,
        duration_ms,
        error,
        output_digest,
        http_calls,
        storage_scope_commitment,
        storage,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecisionMaterial<'a> {
    invocation: &'a InvocationId,
    trace: &'a TraceId,
    principal: &'a PrincipalId,
    actor: &'a Actor,
    via: Option<&'a PrincipalId>,
    attested_subject: Option<&'a ExternalSubject>,
    capability: &'a CapabilityId,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_use: Option<&'a SecretUseProposal>,
    provider: Option<&'a ProviderId>,
    authorized_by: &'a PrincipalId,
    policy_revision: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    policy_ids: &'a [String],
    policy_digest: &'a str,
    constraints: Option<&'a ExecutionConstraints>,
    allowed: bool,
    reason: Option<&'a str>,
}

/// Hashes evidence produced before execution began; a failure means nothing ran.
fn decision_evidence_digest(label: &str, value: &impl Serialize) -> Result<String, BrokerError> {
    evidence_digest(label, value).map_err(|source| BrokerError::DecisionEvidence { source })
}

/// Hashes evidence produced after execution began; a failure leaves the outcome unaudited.
fn outcome_evidence_digest(
    invocation: &InvocationId,
    label: &str,
    value: &impl Serialize,
) -> Result<String, BrokerError> {
    evidence_digest(label, value).map_err(|source| BrokerError::OutcomeEvidence {
        invocation: invocation.clone(),
        source,
    })
}

fn evidence_digest(label: &str, value: &impl Serialize) -> Result<String, serde_json::Error> {
    // Streamed rather than concatenated: the serialized value here is a whole provider response,
    // up to the host output ceiling, and the joined buffer existed only to be hashed once.
    Ok(digest_parts(
        EVIDENCE_HASH_DOMAIN,
        &[label.as_bytes(), &[0], &serde_json::to_vec(value)?],
    ))
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> String {
    digest_parts(domain, &[bytes])
}

fn digest_parts(domain: &[u8], parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(part);
    }
    format!("sha256:{}", hex_bytes(&hasher.finalize()))
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn duration_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn public_host_error(error: &BrokerHostError) -> &'static str {
    match error {
        BrokerHostError::AuthorizationExceedsHostLimit { .. }
        | BrokerHostError::InvalidHttpAuthorization
        | BrokerHostError::MixedHostAuthorization
        | BrokerHostError::StorageDisabled
        | BrokerHostError::MissingStorageGrant
        | BrokerHostError::UnexpectedStorageGrant
        | BrokerHostError::StorageGrantMismatch
        | BrokerHostError::InvalidSecretAuthorization { .. }
        | BrokerHostError::SecretAuthorizationExceedsHttp
        | BrokerHostError::SecretCredentialMismatch
        | BrokerHostError::HttpConfiguration { .. } => "authorization-constraint",
        BrokerHostError::UnknownCapability { .. }
        | BrokerHostError::ProviderDoesNotImplement { .. }
        | BrokerHostError::UnknownCommandWord { .. } => "capability-unavailable",
        // Startup-only failures. They cannot reach a caller — a broker holding a registry has
        // already survived them — but naming them keeps this match exhaustive by proof rather than
        // by a wildcard that would silently absorb a future variant into the wrong public reason.
        BrokerHostError::ConflictingProviders { .. }
        | BrokerHostError::MissingResolveCommand { .. }
        | BrokerHostError::ResolveCommandSignature { .. }
        | BrokerHostError::InvalidArtifactSize { .. }
        | BrokerHostError::InvalidArtifactDigest
        | BrokerHostError::ArtifactTooLarge { .. }
        | BrokerHostError::ArtifactSizeMismatch { .. }
        | BrokerHostError::ArtifactDigestMismatch { .. }
        | BrokerHostError::ProviderIdentityMismatch { .. } => "provider-configuration",
        BrokerHostError::MemoryBudgetExhausted { .. } => "host-memory-budget",
        BrokerHostError::AuthorizedProviderMismatch { .. } => "authorized-provider-mismatch",
        BrokerHostError::InputNotObject { .. }
        | BrokerHostError::SerializeInput { .. }
        | BrokerHostError::InputTooLarge { .. } => "invalid-input",
        BrokerHostError::OutputTooLarge { .. } | BrokerHostError::InvalidOutput { .. } => {
            "invalid-provider-output"
        }
        BrokerHostError::ResolveCommand { .. }
        | BrokerHostError::ResolveCommandUsedHostImport { .. }
        | BrokerHostError::InvalidCommandResolution { .. } => "command-rewrite-failed",
        BrokerHostError::Timeout { .. } => "provider-timeout",
        BrokerHostError::HostCallRejected { .. } => "host-call-rejected",
        BrokerHostError::StorageCallRejected {
            reason: "quota", ..
        } => "storage-quota",
        BrokerHostError::StorageCallRejected {
            reason: "timeout", ..
        } => "storage-timeout",
        BrokerHostError::StorageCallRejected {
            reason: "corrupt", ..
        } => "storage-corrupt",
        BrokerHostError::StorageCallRejected {
            reason: "denied", ..
        } => "storage-io",
        BrokerHostError::StorageCallRejected { .. } => "storage-io",
        BrokerHostError::Storage { source } => match source {
            dekopon_storage_host::StorageHostError::QuotaExceeded => "storage-quota",
            dekopon_storage_host::StorageHostError::Busy => "storage-busy",
            dekopon_storage_host::StorageHostError::Timeout => "storage-timeout",
            dekopon_storage_host::StorageHostError::Corrupt { .. }
            | dekopon_storage_host::StorageHostError::CorruptLayout
            | dekopon_storage_host::StorageHostError::KeyMismatch => "storage-corrupt",
            _ => "storage-io",
        },
        BrokerHostError::Invoke { .. } => "provider-trap",
        BrokerHostError::ProviderFailure {
            provider,
            capability,
            code,
            ..
        } if provider.as_str() == MEMORY_PROVIDER
            && is_memory_capability(capability)
            && matches!(
                code.as_str(),
                "memory-corrupt" | "result-too-large" | "dedup-conflict" | "dedup-capacity"
            ) =>
        {
            match code.as_str() {
                "memory-corrupt" => "memory-corrupt",
                "result-too-large" => "result-too-large",
                "dedup-conflict" => "dedup-conflict",
                "dedup-capacity" => "dedup-capacity",
                _ => "provider-failure",
            }
        }
        BrokerHostError::ProviderFailure { .. } => "provider-failure",
        BrokerHostError::NoProviders
        | BrokerHostError::InvalidLimit { .. }
        | BrokerHostError::Engine { .. }
        | BrokerHostError::Store { .. }
        | BrokerHostError::Linker { .. }
        | BrokerHostError::ArtifactMetadata { .. }
        | BrokerHostError::CompileCache { .. }
        | BrokerHostError::Compile { .. }
        | BrokerHostError::Instantiate { .. }
        | BrokerHostError::DescribeUsedHostImport { .. }
        | BrokerHostError::Describe { .. }
        | BrokerHostError::InvalidManifest { .. }
        | BrokerHostError::Manifest { .. } => "broker-host-failure",
    }
}

/// Failure to evaluate or durably account for one broker invocation.
#[derive(Debug, Error)]
pub enum BrokerError {
    /// Process-lifetime replay ledger reached its configured bound.
    #[error("broker replay ledger reached its {maximum}-identifier bound")]
    ReplayLedgerFull {
        /// Configured maximum.
        maximum: usize,
    },
    /// Optional chat memory was not effective for this trusted context.
    #[error("chat memory is unavailable")]
    MemoryUnavailable,
    /// Trusted memory input construction rejected an impossible shape.
    #[error("chat memory input is invalid")]
    InvalidMemoryInput,
    /// Storage grant derivation or housekeeping failed before provider execution.
    #[error("broker storage authority failed")]
    Storage {
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    /// The blocking task that materializes the storage grant never returned a result.
    ///
    /// Distinct from [`Self::Storage`] because the storage host reported nothing: the task
    /// panicked or the runtime cancelled it. Folding it into `StorageHostError::Io` sent an
    /// operator looking at the filesystem for a bug that is in the code. It keeps `Storage`'s
    /// wire classification — the same preparation step failed, before any provider ran.
    #[error("broker storage materialization did not complete")]
    StorageTask {
        /// Panic payload or cancellation from the blocking task.
        #[source]
        source: tokio::task::JoinError,
    },
    /// A validated policy rule could not create authorization state.
    #[error("broker could not create constrained authorization")]
    Authorization {
        /// Typestate transition failure.
        #[source]
        source: AuthorizationError,
    },
    /// Decision evidence could not be hashed; execution did not begin.
    #[error("broker could not serialize bounded decision evidence")]
    DecisionEvidence {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Authorization decision could not be audited; execution did not begin.
    #[error("broker could not audit its authorization decision")]
    DecisionAudit {
        /// Audit failure.
        #[source]
        source: AuditError,
    },
    /// An authorized pre-provider failure could not append its terminal failed record.
    #[error("broker could not audit an authorized pre-provider failure")]
    AuthorizedFailureAudit {
        #[source]
        source: AuditError,
    },
    /// Terminal evidence could not be hashed after provider work ended.
    #[error("broker could not serialize terminal evidence for {invocation}")]
    OutcomeEvidence {
        /// Invocation whose effect may already have completed.
        invocation: InvocationId,
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Storage crossed its durable marker but live finalization failed.
    #[error("storage outcome for {invocation} is unaudited")]
    StorageOutcome { invocation: InvocationId },
    /// Terminal execution could not be audited after provider work ended.
    #[error("broker could not audit terminal execution for {invocation}")]
    OutcomeAudit {
        /// Invocation whose effect may already have completed.
        invocation: InvocationId,
        /// Audit failure.
        #[source]
        source: AuditError,
    },
}

impl BrokerError {
    /// Stable pre-execution class for a broker-owned storage failure, when that is what happened.
    ///
    /// Nothing executed, so a corrected request may use a fresh invocation identifier — the class
    /// only says which storage condition an operator has to reconcile first.
    #[must_use]
    pub const fn storage_failure_code(&self) -> Option<&'static str> {
        let source = match self {
            Self::Storage { source } => source,
            // A join failure never reached the storage host, but it failed the same preparation
            // step at the same point, so it keeps that step's wire classification.
            Self::StorageTask { .. } => return Some("storage-io"),
            _ => return None,
        };
        Some(match source {
            dekopon_storage_host::StorageHostError::QuotaExceeded
            | dekopon_storage_host::StorageHostError::Arithmetic => "storage-quota",
            dekopon_storage_host::StorageHostError::Busy => "storage-busy",
            dekopon_storage_host::StorageHostError::Timeout => "storage-timeout",
            dekopon_storage_host::StorageHostError::Corrupt { .. }
            | dekopon_storage_host::StorageHostError::CorruptLayout
            | dekopon_storage_host::StorageHostError::KeyMismatch => "storage-corrupt",
            _ => "storage-io",
        })
    }

    /// Stable class for an exhaustion that no resubmission can outlast.
    ///
    /// The retriable class is for a broker that could not complete *this* request. These two
    /// cannot complete any request. The replay ledger never evicts and restart restores it from
    /// durable history, so a fresh invocation identifier fails identically and keeps failing; the
    /// audit log does not rotate, so a full one refuses every append until an operator raises the
    /// bound or moves the file. Reporting either as `broker-unavailable` invites an unbounded
    /// retry loop against a permanently capped broker.
    ///
    /// A *terminal* audit failure is deliberately absent: [`Self::OutcomeAudit`] is an unaudited
    /// outcome first, whatever exhausted it, and that classification must not be weakened here.
    #[must_use]
    pub const fn capacity_failure_code(&self) -> Option<&'static str> {
        match self {
            Self::ReplayLedgerFull { .. } => Some("capacity-exhausted"),
            Self::DecisionAudit {
                source: AuditError::Full { .. },
            }
            | Self::AuthorizedFailureAudit {
                source: AuditError::Full { .. },
            } => Some("capacity-exhausted"),
            Self::MemoryUnavailable
            | Self::InvalidMemoryInput
            | Self::Storage { .. }
            | Self::StorageTask { .. }
            | Self::Authorization { .. }
            | Self::DecisionEvidence { .. }
            | Self::DecisionAudit { .. }
            | Self::AuthorizedFailureAudit { .. }
            | Self::OutcomeEvidence { .. }
            | Self::StorageOutcome { .. }
            | Self::OutcomeAudit { .. } => None,
        }
    }

    /// Invocation whose provider work may already have completed with no terminal audit record.
    ///
    /// `Some` exactly when the failure was raised after [`Broker::invoke`] began provider
    /// execution: the external effect may have taken place, nothing durably recorded its
    /// outcome, and the request must not be resubmitted under any identifier. `None` when
    /// execution provably never began — which makes resubmission *safe*, not useful:
    /// [`Self::capacity_failure_code`] separates the failures a retry can outlive from the
    /// exhaustions it cannot.
    ///
    /// Transports are expected to preserve this distinction; collapsing both cases into one
    /// failure signal invites a resubmission that duplicates a non-idempotent external effect.
    #[must_use]
    pub const fn unaudited_outcome(&self) -> Option<&InvocationId> {
        match self {
            Self::OutcomeEvidence { invocation, .. }
            | Self::OutcomeAudit { invocation, .. }
            | Self::StorageOutcome { invocation } => Some(invocation),
            Self::ReplayLedgerFull { .. }
            | Self::MemoryUnavailable
            | Self::InvalidMemoryInput
            | Self::Storage { .. }
            | Self::StorageTask { .. }
            | Self::Authorization { .. }
            | Self::DecisionEvidence { .. }
            | Self::DecisionAudit { .. }
            | Self::AuthorizedFailureAudit { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests;
