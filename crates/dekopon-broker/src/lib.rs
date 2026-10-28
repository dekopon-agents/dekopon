//! Deny-by-default authorization, execution, evidence, and audit core for Dekopon.
//!
//! This crate is broker-owned machinery. A transport supplies an [`AuthenticatedContext`] derived
//! from trusted peer identity, never payload claims. [`Broker`] asks a `dekopon-policy`
//! [`PolicyEngine`] whether that context may act, binds an allow to the owner-authored
//! [`ConstraintSet`] for the requested capability, creates a single-use authorization, executes it
//! through `dekopon-broker-host`, and records metadata-only audit events.
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
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    ops::ControlFlow,
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;
use dekopon_broker_host::{
    BoundCredential, BrokerHostError, BrokerProviderRegistry, CommandRunOutcome, HttpCallEvidence,
    ProviderCapability,
};
pub use dekopon_broker_protocol::{
    Attestation, AvailableCapability, ChatMemorySurface, ChatScopeClaim, ChatTransportKind,
    Conversation, ConversationKind, ConversationKindMatch, ConversationMatch,
    ConversationMatchProblem, DeliveredAnswer, DeliveredTurnRequest, DeliveryIdentity,
    InvocationRequest, Trigger,
};
use dekopon_capability::{
    AuthorizationError, ChatSlot, ChatSlotValues, DecisionReference, EffectKind, Evidence,
    ExecutionConstraints, HttpConstraints, HttpConstraintsError, InvocationOutcome,
    InvocationResult, ProposedInvocation, SecretUseGrant, StorageAccess, StorageConstraints,
    StorageInterface, StorageRetention, StorageScope, broker::AuthorizationGate,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, PrincipalKind,
    ProviderFailureDetail, ProviderId, RiskLevel, SecretBytes, SecretDrn, SecretSinkKind,
    SecretUseProposal, SubjectSegment, SubjectService, TraceId, error_chain,
};
pub use dekopon_policy::{AGENT_PROMPT_ACTION, PolicyBuildError, PolicyEngine, PolicyWorld};
use dekopon_policy::{
    PolicyContext, PolicyConversation, PolicyDecision, PolicyRequest, PolicyTarget,
};
use dekopon_storage_host::{
    ContinuityPolicy, RetentionPolicies, StorageEvidence, StorageGrantPreparation,
    StorageGrantRequest, StorageScopeCommitment,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tracing::Instrument as _;

const MAX_POLICY_SCOPE_ENTRIES: usize = 64;
const POLICY_STACK_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SECRET_BINDINGS: usize = 1024;
const EVIDENCE_HASH_DOMAIN: &[u8] = b"dekopon-evidence-v1\0";
const POLICY_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.policy-decision+json";
const HTTP_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.http-evidence+json";
const STORAGE_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.storage-evidence+json";

const UNROUTED_RECORD_CAPABILITY: &str = "memory.chat.record";
const MEMORY_MIN_TURN_LINE_BYTES: u64 = 241;
const MEMORY_PROVIDER_INPUT_OVERHEAD_BYTES: u64 = 4 * 1024;
const MEMORY_QUERY_JSON_EXPANSION: u64 = 6;
const MEMORY_WORKING_SET_OVERHEAD_BYTES: u64 = 4 * 1024 * 1024;
const MEMORY_MIN_RESULT_BYTES: u64 = 30;
const MEMORY_LOGICAL_FILES: u64 = 1;
const MEMORY_RECORD_FIXED_HOST_CALLS: u64 = 3;
const MEMORY_FUEL_BASE: u64 = 10_000_000;
const MEMORY_FUEL_PER_WORK_BYTE: u64 = 256;
const MEMORY_READ_CHUNK_BYTES: u64 = 256 * 1024;

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
    pub compaction_target_bytes: u64,
    pub compaction_threshold_bytes: u64,
}

impl ChatMemoryConfig {
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
        {
            return Err(BrokerBuildError::InvalidChatMemory);
        }
        let retained = u64::from(self.max_lookback_turns)
            .checked_mul(self.max_turn_bytes)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        // read-file charges a full CHUNK even on a partial final call.
        let read_budget = round_up(self.compaction_threshold_bytes, MEMORY_READ_CHUNK_BYTES)?;
        let host_calls = read_budget
            .checked_div(MEMORY_READ_CHUNK_BYTES)
            .and_then(|value| value.checked_add(MEMORY_RECORD_FIXED_HOST_CALLS))
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let write_budget = self
            .compaction_target_bytes
            .checked_add(self.max_turn_bytes)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let threshold_with_append = self
            .compaction_threshold_bytes
            .checked_add(self.max_turn_bytes)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let namespace_headroom = threshold_with_append
            .checked_add(32 * 4_096)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        if retained > self.compaction_target_bytes
            || MEMORY_READ_CHUNK_BYTES > storage.max_read_bytes_per_call
            || self.max_turn_bytes > storage.max_write_bytes_per_call
            || self.compaction_target_bytes > storage.max_write_bytes_per_call
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
            .and_then(|bytes| bytes.checked_add(self.compaction_target_bytes.checked_mul(2)?))
            .and_then(|bytes| bytes.checked_add(self.max_turn_bytes))
            .and_then(|bytes| bytes.checked_add(self.max_result_bytes))
            .and_then(|bytes| bytes.checked_add(MEMORY_WORKING_SET_OVERHEAD_BYTES))
            .ok_or(BrokerBuildError::InvalidChatMemory)
    }

    fn minimum_provider_fuel(&self) -> Result<u64, BrokerBuildError> {
        let record_work = self
            .compaction_threshold_bytes
            .checked_add(self.compaction_target_bytes)
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

    pub fn validate_host_limits(
        &self,
        host: &dekopon_broker_host::BrokerHostLimits,
    ) -> Result<(), BrokerBuildError> {
        let max_input = u64::try_from(host.max_input_bytes).unwrap_or(u64::MAX);
        let max_memory = u64::try_from(host.max_memory_bytes).unwrap_or(u64::MAX);
        if self.maximum_provider_input_bytes()? > max_input
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

pub const DEFAULT_MAX_CONSTRAINT_SETS: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatedContext {
    principal: PrincipalId,
    actor: Actor,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<PrincipalId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attested_subject: Option<ExternalSubject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_scope: Option<ChatScopeClaim>,
    #[serde(skip_serializing_if = "Option::is_none")]
    space: Option<PrincipalId>,
}

impl AuthenticatedContext {
    pub fn space_binding(&self) -> Option<dekopon_core::SpaceBinding> {
        let principal = self.space.as_ref()?;
        let scope = self.chat_scope()?;
        let agent = match self.actor() {
            Actor::Agent { agent } => agent,
            Actor::Human { .. } | Actor::Service { .. } => return None,
        };
        Some(dekopon_core::SpaceBinding {
            key: dekopon_core::SpaceKey {
                agent: agent.clone(),
                transport: scope.transport.to_string().into(),
                workspace: scope.conversation.container.clone().map(Into::into),
                channel: scope.conversation.id.clone().into(),
            },
            principal: principal.clone(),
        })
    }

    pub fn new(principal: PrincipalId, actor: Actor) -> Result<Self, ContextError> {
        Self::build(principal, actor, None, None, None)
    }

    /// `via` is the deny-by-default hinge for attested authority: a policy rule for direct peers
    /// (`via` absent) can never match an attested context, and vice versa.
    pub fn attested(
        principal: PrincipalId,
        actor: Actor,
        via: PrincipalId,
        subject: ExternalSubject,
    ) -> Result<Self, ContextError> {
        Self::build(principal, actor, Some(via), Some(subject), None)
    }

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
            space: None,
        })
    }

    #[must_use]
    fn with_refused_subject(&self, subject: ExternalSubject) -> Self {
        Self {
            principal: self.principal.clone(),
            actor: self.actor.clone(),
            via: None,
            attested_subject: Some(subject),
            chat_scope: None,
            space: None,
        }
    }

    #[must_use]
    pub fn principal(&self) -> &PrincipalId {
        &self.principal
    }

    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    #[must_use]
    pub fn via(&self) -> Option<&PrincipalId> {
        self.via.as_ref()
    }

    #[must_use]
    pub fn attested_subject(&self) -> Option<&ExternalSubject> {
        self.attested_subject.as_ref()
    }

    #[must_use]
    pub fn chat_scope(&self) -> Option<&ChatScopeClaim> {
        self.chat_scope.as_ref()
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ContextError {
    #[error("authenticated principal does not match the human or service actor")]
    PrincipalMismatch,
}

#[derive(Debug)]
pub struct AssetInvocationResult {
    pub result: InvocationResult,
    pub assets: dekopon_broker_host::asset::AssetOutputs,
}

/// This route is the operator's explicit declaration, not a naming convention: only constraint-set
/// membership marks the reserved chat-memory surface, no capability or provider name does.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "camelCase")]
pub enum CapabilityRoute {
    #[default]
    Generic,
    ChatMemoryRecord,
    ChatMemoryRecent,
    ChatMemorySearch,
}

impl CapabilityRoute {
    pub(crate) const CHAT_MEMORY: [Self; 3] = [
        Self::ChatMemoryRecord,
        Self::ChatMemoryRecent,
        Self::ChatMemorySearch,
    ];

    #[must_use]
    pub const fn is_generic(&self) -> bool {
        matches!(self, Self::Generic)
    }

    #[must_use]
    pub const fn is_chat_memory(self) -> bool {
        !self.is_generic()
    }

    #[must_use]
    pub const fn is_chat_memory_retrieval(self) -> bool {
        matches!(self, Self::ChatMemoryRecent | Self::ChatMemorySearch)
    }

    const fn chat_memory_access(self) -> Option<StorageAccess> {
        match self {
            Self::Generic => None,
            Self::ChatMemoryRecord => Some(StorageAccess::ReadWrite),
            Self::ChatMemoryRecent | Self::ChatMemorySearch => Some(StorageAccess::ReadOnly),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::ChatMemoryRecord => "chatMemoryRecord",
            Self::ChatMemoryRecent => "chatMemoryRecent",
            Self::ChatMemorySearch => "chatMemorySearch",
        }
    }
}

impl fmt::Display for CapabilityRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A constraint set is not a grant: Cedar policy decides reachability, this only bounds how
/// narrowly an already-permitted capability executes, so editing policy alone can't widen execution
/// bounds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConstraintSet {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_default_secret_use"
    )]
    pub default_secret_use: Option<DefaultSecretUse>,
    #[serde(default, skip_serializing_if = "CapabilityRoute::is_generic")]
    pub route: CapabilityRoute,
    pub provider: ProviderId,
    pub effect: EffectKind,
    pub risk: RiskLevel,
    /// Credentials bind per capability rather than per provider to block a confused-deputy attack
    /// where the same provider component performs a different operation than the one that
    /// authorized it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    pub constraints: ExecutionConstraints,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DefaultSecretUse {
    pub drn: SecretDrn,
    pub sink: DefaultSecretSink,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DefaultSecretSink {
    HttpBearer,
}

fn deserialize_default_secret_use<'de, D>(
    deserializer: D,
) -> Result<Option<DefaultSecretUse>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    DefaultSecretUse::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Leniency {
    #[default]
    Strict,
    Tolerant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartupWarning {
    UnroutedConstraintSet {
        capability: CapabilityId,
    },
    /// Invocation is denied unconstrained-capability before Cedar is even consulted, so an
    /// unbounded policy grant is unreachable rather than merely unbounded.
    UnconstrainedCapability {
        capability: CapabilityId,
    },
}

impl StartupWarning {
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::UnroutedConstraintSet { .. } => "unrouted-constraint-set",
            Self::UnconstrainedCapability { .. } => "unconstrained-capability",
        }
    }

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConstraintCatalog {
    sets: BTreeMap<CapabilityId, ConstraintSet>,
    agent_credentials: BTreeMap<AgentId, BTreeMap<String, String>>,
}

impl ConstraintCatalog {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new(
        entries: impl IntoIterator<Item = (CapabilityId, ConstraintSet)>,
    ) -> Result<Self, BrokerBuildError> {
        let mut sets = BTreeMap::new();
        for (capability, set) in entries {
            if sets.insert(capability.clone(), set).is_some() {
                return Err(BrokerBuildError::DuplicateConstraintSet { capability });
            }
        }
        Ok(Self {
            sets,
            agent_credentials: BTreeMap::new(),
        })
    }

    /// Keyed by the agent the broker derived from the attestation, never a caller claim: an agent
    /// rebinds a credential name its capabilities already use, so one capability can reach a
    /// different organization's token per agent.
    #[must_use]
    pub fn with_agent_credentials(
        mut self,
        bindings: BTreeMap<AgentId, BTreeMap<String, String>>,
    ) -> Self {
        self.agent_credentials = bindings;
        self
    }

    #[must_use]
    pub fn credential_for<'a>(&'a self, set: &'a ConstraintSet, actor: &Actor) -> Option<&'a str> {
        let name = set.credential.as_deref()?;
        let rebound = match actor {
            Actor::Agent { agent } => self
                .agent_credentials
                .get(agent)
                .and_then(|bindings| bindings.get(name)),
            Actor::Human { .. } | Actor::Service { .. } => None,
        };
        Some(rebound.map_or(name, String::as_str))
    }

    fn selectable_credentials<'a>(&'a self, set: &'a ConstraintSet) -> Vec<&'a str> {
        let Some(name) = set.credential.as_deref() else {
            return Vec::new();
        };
        let mut names = vec![name];
        names.extend(
            self.agent_credentials
                .values()
                .filter_map(|bindings| bindings.get(name).map(String::as_str)),
        );
        names
    }

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

    #[must_use]
    pub fn get(&self, capability: &CapabilityId) -> Option<&ConstraintSet> {
        self.sets.get(capability)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&CapabilityId, &ConstraintSet)> {
        self.sets.iter()
    }

    fn routed(&self, route: CapabilityRoute) -> Option<(&CapabilityId, &ConstraintSet)> {
        self.sets.iter().find(|(_, set)| set.route == route)
    }

    fn chat_memory_provider(&self) -> Option<&ProviderId> {
        self.sets
            .values()
            .find(|set| set.route.is_chat_memory())
            .map(|set| &set.provider)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

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
        self.validate_routes()?;
        self.validate_storage_namespaces()?;
        self.retention_policies()?;
        for (capability_id, set) in &self.sets {
            validate_set_constraints(set)?;
            validate_set_credential(
                capability_id,
                set,
                &self.selectable_credentials(set),
                credentials,
            )?;
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

    fn retention_policies(&self) -> Result<RetentionPolicies, BrokerBuildError> {
        let mut policies = RetentionPolicies::new();
        for set in self.sets.values() {
            let Some(storage) = &set.constraints.storage else {
                continue;
            };
            if let StorageRetention::IdleTtl(ttl) = storage.retention
                && (ttl.as_millis() == 0 || ttl.as_millis() > u128::from(u64::MAX))
            {
                return Err(BrokerBuildError::InvalidStorageRetention);
            }
            let namespace = storage.namespace.as_ref().unwrap_or(&set.provider).clone();
            let key = (namespace, storage.scope);
            if let Some(previous) = policies.insert(key.clone(), storage.retention)
                && previous != storage.retention
            {
                return Err(BrokerBuildError::ConflictingStorageRetention {
                    namespace: key.0,
                    scope: key.1,
                });
            }
        }
        Ok(policies)
    }

    fn validate_storage_namespaces(&self) -> Result<(), BrokerBuildError> {
        let mut conflicts = Vec::new();
        let mut members = BTreeMap::<&ProviderId, Vec<(&CapabilityId, &StorageConstraints)>>::new();
        for (capability, set) in &self.sets {
            let Some(storage) = &set.constraints.storage else {
                continue;
            };
            if storage.scope != StorageScope::Agent {
                if let Some(namespace) = &storage.namespace {
                    conflicts.push(NamespaceConflict::NotAgentScope {
                        capability: capability.clone(),
                        namespace: namespace.clone(),
                        scope: storage.scope,
                    });
                }
                continue;
            }
            members
                .entry(storage.namespace.as_ref().unwrap_or(&set.provider))
                .or_default()
                .push((capability, storage));
        }
        for (namespace, sets) in members {
            if sets.iter().all(|(_, storage)| storage.namespace.is_none()) {
                continue;
            }
            let interfaces = sets
                .iter()
                .map(|(_, storage)| storage.interface)
                .collect::<BTreeSet<_>>();
            if interfaces.len() > 1 {
                conflicts.push(NamespaceConflict::Mismatch {
                    namespace: namespace.clone(),
                    capabilities: sets
                        .iter()
                        .map(|(capability, storage)| ((*capability).clone(), storage.interface))
                        .collect(),
                });
            }
        }
        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(BrokerBuildError::ConflictingStorageNamespaces { conflicts })
        }
    }

    fn validate_routes(&self) -> Result<(), BrokerBuildError> {
        let mut conflicts = Vec::new();
        for route in CapabilityRoute::CHAT_MEMORY {
            let claimants = self
                .sets
                .iter()
                .filter(|(_, set)| set.route == route)
                .map(|(capability, _)| capability.clone())
                .collect::<Vec<_>>();
            if claimants.len() > 1 {
                conflicts.push(RouteConflict::DuplicateRole {
                    route,
                    capabilities: claimants,
                });
            }
        }
        let providers = self
            .sets
            .values()
            .filter(|set| set.route.is_chat_memory())
            .map(|set| set.provider.clone())
            .collect::<BTreeSet<_>>();
        if providers.len() > 1 {
            conflicts.push(RouteConflict::SplitProvider {
                providers: providers.into_iter().collect(),
            });
        }
        for (capability, set) in &self.sets {
            let Some(access) = set.route.chat_memory_access() else {
                continue;
            };
            let declared = set.constraints.storage.as_ref().filter(|storage| {
                storage.interface == StorageInterface::Jsonl
                    && storage.scope == StorageScope::PrivateConversation
                    && storage.access == access
            });
            if declared.is_none() {
                conflicts.push(RouteConflict::MissingChatStorage {
                    capability: capability.clone(),
                    route: set.route,
                    access,
                });
            }
        }
        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(BrokerBuildError::ConflictingRoutes { conflicts })
        }
    }
}

/// Resolved after authorization and before the execution context is built, so a refresh never
/// appears in evidence; concurrent resolutions of the same credential must be serialized or the
/// refresh token family is revoked.
#[async_trait]
pub trait RefreshingCredential: Send + Sync + fmt::Debug {
    fn destinations(&self) -> &[String];

    async fn resolve(&self) -> Result<BoundCredential, CredentialRefreshError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum CredentialRefreshError {
    #[error("the credential's authorization is gone and must be renewed by an operator")]
    ReauthorizationRequired,
    #[error("the credential could not be refreshed ({category})")]
    Unavailable {
        /// Low-cardinality failure class, never a message derived from credential material.
        category: &'static str,
    },
}

impl CredentialRefreshError {
    #[must_use]
    pub const fn category(self) -> &'static str {
        match self {
            Self::ReauthorizationRequired => "reauthorization-required",
            Self::Unavailable { category } => category,
        }
    }

    const fn permanent(self) -> bool {
        matches!(self, Self::ReauthorizationRequired)
    }

    const fn reason(self) -> &'static str {
        if self.permanent() {
            "credential-unavailable"
        } else {
            "credential-refresh-failed"
        }
    }
}

#[derive(Clone, Debug)]
pub enum StoredCredential {
    Fixed(Box<BoundCredential>),
    Refreshing(Arc<dyn RefreshingCredential>),
}

impl From<BoundCredential> for StoredCredential {
    fn from(credential: BoundCredential) -> Self {
        Self::Fixed(Box::new(credential))
    }
}

impl From<Arc<dyn RefreshingCredential>> for StoredCredential {
    fn from(source: Arc<dyn RefreshingCredential>) -> Self {
        Self::Refreshing(source)
    }
}

impl StoredCredential {
    /// The Refreshing arm must reuse the same coverage comparison Fixed uses, or a covered host
    /// could later be refused.
    fn covers(&self, allowed_host: &str) -> bool {
        match self {
            Self::Fixed(credential) => credential.covers(allowed_host),
            Self::Refreshing(source) => {
                dekopon_broker_host::destinations_cover(source.destinations(), allowed_host)
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct CredentialStore {
    entries: BTreeMap<String, StoredCredential>,
}

impl CredentialStore {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn new<C: Into<StoredCredential>>(
        entries: impl IntoIterator<Item = (String, C)>,
    ) -> Result<Self, BrokerBuildError> {
        let mut store = BTreeMap::new();
        for (name, credential) in entries {
            if store.insert(name.clone(), credential.into()).is_some() {
                return Err(BrokerBuildError::DuplicateCredential { name });
            }
        }
        Ok(Self { entries: store })
    }

    fn get(&self, name: &str) -> Option<&StoredCredential> {
        self.entries.get(name)
    }
}

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttestorGrant {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespaces: Option<Vec<String>>,
}

impl AttestorGrant {
    pub fn validate(&self) -> Result<(), BrokerBuildError> {
        let Some(namespaces) = &self.namespaces else {
            return Ok(());
        };
        if namespaces.is_empty() || namespaces.len() > MAX_POLICY_SCOPE_ENTRIES {
            return Err(BrokerBuildError::InvalidAttestorScope {
                scope: namespaces.len().to_string(),
            });
        }
        for scope in namespaces {
            let mut segments = scope.split('.');
            let valid = segments
                .next()
                .unwrap_or_default()
                .parse::<SubjectService>()
                .is_ok_and(|service| {
                    segments.clone().count() <= 2
                        && segments.enumerate().all(|(index, segment)| {
                            let position = if index == 0 && service.requires_tenant() {
                                SubjectSegment::Tenant
                            } else {
                                SubjectSegment::Subject
                            };
                            service.admits_segment(position, segment)
                        })
                });
            if !valid {
                return Err(BrokerBuildError::InvalidAttestorScope {
                    scope: scope.clone(),
                });
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn permits(&self, subject: &ExternalSubject) -> bool {
        self.namespaces.as_ref().is_none_or(|namespaces| {
            namespaces
                .iter()
                .any(|namespace| subject.in_namespace(namespace))
        })
    }
}

/// This directory alone decides identity from an authenticated subject; unmapped subjects fail
/// closed and principals are never minted on demand.
#[derive(Debug, Default)]
pub struct IdentityDirectory {
    mappings: BTreeMap<ExternalSubject, PrincipalId>,
}

impl IdentityDirectory {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

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

    #[must_use]
    pub fn resolve(&self, subject: &ExternalSubject) -> Option<&PrincipalId> {
        self.mappings.get(subject)
    }

    #[must_use]
    pub fn principal_kind(&self, principal: &PrincipalId) -> PrincipalKind {
        self.mappings
            .iter()
            .find_map(|(subject, mapped)| (mapped == principal).then(|| subject.kind()))
            .unwrap_or(PrincipalKind::Service)
    }

    pub fn principals(&self) -> impl Iterator<Item = &PrincipalId> {
        self.mappings.values()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct BrokerLimits {
    pub max_constraint_sets: usize,
}

impl Default for BrokerLimits {
    fn default() -> Self {
        Self {
            max_constraint_sets: DEFAULT_MAX_CONSTRAINT_SETS,
        }
    }
}

/// Every value here comes from the broker's own authenticated-context view; caller-supplied
/// proposal input is deliberately excluded so no policy can be made to depend on it.
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
        started_by: turn_started_by(context).map(|started| started.as_str().to_owned()),
        trigger: context
            .chat_scope()
            .map(|scope| scope.trigger.as_str().to_owned()),
        github: None,
        conversation: context.chat_scope().map(|scope| PolicyConversation {
            kind: scope.conversation.kind.as_str().to_owned(),
            container: scope.conversation.container.clone(),
            id: scope.conversation.id.clone(),
            thread: scope.conversation.thread.clone(),
        }),
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum TurnStartedBy {
    Person,
    Message,
    Wake,
    Job,
    Probe,
}

impl TurnStartedBy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Message => "message",
            Self::Wake => "wake",
            Self::Job => "job",
            Self::Probe => "probe",
        }
    }
}

fn turn_started_by(context: &AuthenticatedContext) -> Option<TurnStartedBy> {
    context.chat_scope().map(|scope| match scope.trigger {
        Trigger::Message
            if context
                .attested_subject()
                .is_some_and(|subject| subject.kind() == PrincipalKind::Person) =>
        {
            TurnStartedBy::Person
        }
        Trigger::Message => TurnStartedBy::Message,
        Trigger::Wake => TurnStartedBy::Wake,
        Trigger::Job => TurnStartedBy::Job,
        Trigger::Probe => TurnStartedBy::Probe,
    })
}

/// A watch probe runs unattended every tick, so it may only read, whatever owner policy permits.
fn probe_permits(context: &AuthenticatedContext, effect: EffectKind) -> bool {
    context
        .chat_scope()
        .is_none_or(|scope| scope.trigger != Trigger::Probe)
        || effect == EffectKind::ReadOnly
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

/// This is the load-bearing check requiring every allowedHosts entry verbatim in the credential's
/// destinations; it also proves every per-agent override, since an unproven one would surface only
/// as a runtime mismatch when a caller first matches it.
fn validate_set_credential(
    capability_id: &CapabilityId,
    set: &ConstraintSet,
    names: &[&str],
    credentials: &CredentialStore,
) -> Result<(), BrokerBuildError> {
    if names.is_empty() {
        return Ok(());
    }
    let Some(http) = &set.constraints.http else {
        return Err(BrokerBuildError::CredentialWithoutHttp {
            capability: capability_id.clone(),
        });
    };
    for &name in names {
        let credential =
            credentials
                .get(name)
                .ok_or_else(|| BrokerBuildError::UnknownCredential {
                    capability: capability_id.clone(),
                    name: name.to_owned(),
                })?;
        // Coverage must be checked from declared destinations only; resolving a refreshing
        // credential would make startup depend on a live token endpoint.
        for host in &http.allowed_hosts {
            if !credential.covers(host) {
                return Err(BrokerBuildError::CredentialDestinationMismatch {
                    capability: capability_id.clone(),
                    name: name.to_owned(),
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
    if let Some(asset) = &constraints.asset
        && ((asset.send && set.effect != EffectKind::ExternalWrite)
            || ((asset.attach || asset.remove) && set.effect == EffectKind::ReadOnly))
    {
        return Err(BrokerBuildError::InvalidPolicyConstraints);
    }
    let Some(http) = &constraints.http else {
        return Ok(());
    };
    http.validate()
        .map_err(|source| BrokerBuildError::InvalidHttpConstraints { source })?;
    Ok(())
}

const MEMORY_PROMPT_NOTE: &str = "`memory recent` and `memory search` retrieve older turns of \
this chat. Do not claim to remember anything you have not retrieved.";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteConflict {
    DuplicateRole {
        route: CapabilityRoute,
        capabilities: Vec<CapabilityId>,
    },
    /// The chat-memory surface belongs to one provider, all of it or none: a split would let one
    /// provider hold the write half of a conversation another provider reads back.
    SplitProvider { providers: Vec<ProviderId> },
    MissingChatStorage {
        capability: CapabilityId,
        route: CapabilityRoute,
        access: StorageAccess,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NamespaceConflict {
    NotAgentScope {
        capability: CapabilityId,
        namespace: ProviderId,
        scope: StorageScope,
    },
    Mismatch {
        namespace: ProviderId,
        capabilities: Vec<(CapabilityId, StorageInterface)>,
    },
}

impl fmt::Display for NamespaceConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAgentScope {
                capability,
                namespace,
                scope,
            } => write!(
                formatter,
                "capability {capability} names storage namespace {namespace} at scope {scope:?}; \
                 only agent scope may name a namespace"
            ),
            Self::Mismatch {
                namespace,
                capabilities,
            } => {
                write!(formatter, "storage namespace {namespace} is shared by")?;
                for (capability, interface) in capabilities {
                    write!(formatter, " {capability} ({interface:?})")?;
                }
                write!(
                    formatter,
                    "; every agent-scope capability in one namespace declares one interface"
                )
            }
        }
    }
}

impl fmt::Display for RouteConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateRole {
                route,
                capabilities,
            } => {
                write!(formatter, "route {route} is claimed by")?;
                for capability in capabilities {
                    write!(formatter, " {capability}")?;
                }
                write!(
                    formatter,
                    "; exactly one constraint set may declare each chat-memory role"
                )
            }
            Self::SplitProvider { providers } => {
                write!(formatter, "chat-memory routes name providers")?;
                for provider in providers {
                    write!(formatter, " {provider}")?;
                }
                write!(formatter, "; one provider must own the whole surface")
            }
            Self::MissingChatStorage {
                capability,
                route,
                access,
            } => write!(
                formatter,
                "capability {capability} declares route {route} without jsonl chat storage at \
                 {access:?} access"
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum BrokerBuildError {
    #[error("broker limit {field} must be greater than zero")]
    ZeroLimit { field: &'static str },
    #[error("operating system entropy is unavailable for the broker boot nonce")]
    BootEntropy {
        #[source]
        source: std::io::Error,
    },
    #[error("configuration contains {count} constraint sets; broker maximum is {maximum}")]
    TooManyConstraintSets { count: usize, maximum: usize },
    #[error("policy permits capability {capability}, which has no constraint set")]
    UnconstrainedCapability { capability: CapabilityId },
    #[error("configuration duplicates a constraint set for capability {capability}")]
    DuplicateConstraintSet { capability: CapabilityId },
    #[error("constraint set capability {capability} has no loaded provider route")]
    UnknownCapability { capability: CapabilityId },
    #[error(
        "constraint set expected provider {expected} for {capability}, but route selects {actual}"
    )]
    ProviderMismatch {
        capability: CapabilityId,
        expected: ProviderId,
        actual: ProviderId,
    },
    #[error("constraint set metadata {field} does not match loaded capability {capability}")]
    CapabilityMetadataMismatch {
        capability: CapabilityId,
        field: &'static str,
    },
    #[error("execution constraints are incomplete or overbroad")]
    InvalidPolicyConstraints,
    #[error("storage idle TTL must be a positive whole number of milliseconds fitting in u64")]
    InvalidStorageRetention,
    #[error("storage retention conflicts for namespace {namespace} and scope {scope:?}")]
    ConflictingStorageRetention {
        namespace: ProviderId,
        scope: StorageScope,
    },
    #[error("http execution constraints are invalid")]
    InvalidHttpConstraints {
        #[source]
        source: HttpConstraintsError,
    },
    #[error("execution constraints exceed component host ceilings")]
    HostConstraint {
        #[source]
        source: BrokerHostError,
    },
    #[error("constraint set for {capability} names unknown credential {name:?}")]
    UnknownCredential {
        capability: CapabilityId,
        name: String,
    },
    #[error("constraint set for {capability} binds a credential but grants no HTTP authority")]
    CredentialWithoutHttp { capability: CapabilityId },
    #[error(
        "constraint set for {capability} allows host {host:?} outside credential {name:?} \
         destinations"
    )]
    CredentialDestinationMismatch {
        capability: CapabilityId,
        name: String,
        host: String,
    },
    #[error("credential store duplicates name {name:?}")]
    DuplicateCredential { name: String },
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
    #[error("attestor namespace scope {scope:?} is not a canonical subject prefix")]
    InvalidAttestorScope { scope: String },
    #[error("chat-memory bounds do not compose with provider/storage ceilings")]
    InvalidChatMemory,
    #[error(
        "chatMemory is configured but no constraint set declares route: {}; the surface is all \
         three roles — chatMemoryRecord, chatMemoryRecent, chatMemorySearch — with exactly one \
         constraint set declaring each and all of them naming one provider; see \
         docs/upgrading.md",
        roles.iter().map(|role| role.as_str()).collect::<Vec<_>>().join(", ")
    )]
    UnroutedChatMemory { roles: Vec<CapabilityRoute> },
    #[error("constraint sets declare {} conflicting capability route(s): {}", conflicts.len(),
        conflicts.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    ConflictingRoutes { conflicts: Vec<RouteConflict> },
    #[error("constraint sets declare {} conflicting storage namespace(s): {}", conflicts.len(),
        conflicts.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    ConflictingStorageNamespaces { conflicts: Vec<NamespaceConflict> },
    #[error("identity mapping duplicates subject {subject:?}")]
    DuplicateSubjectMapping { subject: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum AuditEvent {
    Decision {
        authority: Box<DecisionAuthority>,
        invocation: InvocationId,
        trace: TraceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<PrincipalId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<Actor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<PrincipalId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attested_subject: Option<ExternalSubject>,
        capability: CapabilityId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<SecretDrn>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_sink: Option<SecretSinkKind>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<ProviderId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorized_by: Option<PrincipalId>,
        decision_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        policy_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        policy_error_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_errors_present: Option<bool>,
        #[serde(default)]
        decision_point: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_digest: Option<String>,
        allowed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        decision_digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_scope_commitment: Option<StorageScopeCommitment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage: Option<StorageEvidence>,
    },
    Execution {
        invocation: InvocationId,
        trace: TraceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<PrincipalId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<Actor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<PrincipalId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attested_subject: Option<ExternalSubject>,
        capability: CapabilityId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<SecretDrn>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_sink: Option<SecretSinkKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<ProviderId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorized_by: Option<PrincipalId>,
        decision_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        policy_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_digest: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_artifact_sha256: Option<String>,
        effect: EffectKind,
        risk: RiskLevel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
        outcome: InvocationOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_status: Option<std::num::NonZeroU8>,
        duration_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_detail: Option<ProviderFailureDetail>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        http_calls: Vec<HttpCallEvidence>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage_scope_commitment: Option<StorageScopeCommitment>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        storage: Option<StorageEvidence>,
    },
}

pub trait AuditLog: Send + Sync + 'static {
    fn append(&self, event: AuditEvent) -> impl Future<Output = Result<(), AuditError>> + Send;
}

#[derive(Debug)]
pub struct InMemoryAuditLog {
    maximum: usize,
    state: parking_lot::Mutex<Vec<AuditEvent>>,
}

impl InMemoryAuditLog {
    pub fn new(maximum: usize) -> Result<Self, AuditConfigurationError> {
        if maximum == 0 {
            return Err(AuditConfigurationError::ZeroMaximum);
        }
        Ok(Self {
            maximum,
            state: parking_lot::Mutex::new(Vec::new()),
        })
    }

    pub fn records(&self) -> Vec<AuditEvent> {
        self.state.lock().clone()
    }
}

impl AuditLog for InMemoryAuditLog {
    async fn append(&self, event: AuditEvent) -> Result<(), AuditError> {
        let mut records = self.state.lock();
        if records.len() >= self.maximum {
            return Err(AuditError::Full {
                maximum: self.maximum,
            });
        }
        records.push(event);
        Ok(())
    }
}

/// Every decision is already logged in the live trace before any sink runs; this sink keeps nothing
/// else, so losing the log exporter loses the audit trail entirely, by design.
#[derive(Clone, Copy, Debug, Default)]
pub struct TraceOnlyAuditLog;

impl AuditLog for TraceOnlyAuditLog {
    async fn append(&self, _event: AuditEvent) -> Result<(), AuditError> {
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AuditConfigurationError {
    #[error("audit record maximum must be greater than zero")]
    ZeroMaximum,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("audit log reached its {maximum}-record bound")]
    Full { maximum: usize },
}

impl AuditError {
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Full { .. } => "full",
        }
    }
}

#[derive(Debug)]
pub struct Broker<A> {
    registry: BrokerProviderRegistry,
    policy: Arc<PolicyEngine>,
    policy_digest: String,
    constraints: ConstraintCatalog,
    credentials: CredentialStore,
    secrets: SecretCatalog,
    identities: IdentityDirectory,
    broker_principal: PrincipalId,
    gate: AuthorizationGate,
    audit: Arc<A>,
    chat_memory: Option<ChatMemoryConfig>,
    console_smoke_nonce: [u8; 16],
}

impl<A> Broker<A>
where
    A: AuditLog,
{
    #[allow(
        clippy::too_many_arguments,
        reason = "each trusted input is a separate owner-controlled store; bundling them into one \
                  struct would let a caller assemble policy, constraints, credentials, and identity \
                  mapping from mismatched sources without the type system noticing"
    )]
    pub fn new(
        registry: BrokerProviderRegistry,
        broker_principal: PrincipalId,
        policy: PolicyEngine,
        constraints: ConstraintCatalog,
        credentials: CredentialStore,
        identities: IdentityDirectory,
        audit: Arc<A>,
        limits: BrokerLimits,
    ) -> Result<Self, BrokerBuildError> {
        Self::start(
            registry,
            broker_principal,
            policy,
            constraints,
            credentials,
            identities,
            audit,
            limits,
            Leniency::Strict,
        )
        .map(|(broker, _)| broker)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "each trusted input is a separate owner-controlled store; bundling them into one \
                  struct would let a caller assemble policy, constraints, credentials, and identity \
                  mapping from mismatched sources without the type system noticing"
    )]
    pub fn start(
        registry: BrokerProviderRegistry,
        broker_principal: PrincipalId,
        policy: PolicyEngine,
        constraints: ConstraintCatalog,
        credentials: CredentialStore,
        identities: IdentityDirectory,
        audit: Arc<A>,
        limits: BrokerLimits,
        leniency: Leniency,
    ) -> Result<(Self, Vec<StartupWarning>), BrokerBuildError> {
        let mut constraints = constraints;
        let mut warnings = Vec::new();
        if limits.max_constraint_sets == 0 {
            return Err(BrokerBuildError::ZeroLimit {
                field: "max_constraint_sets",
            });
        }
        if leniency == Leniency::Tolerant {
            // Unrouted constraint sets must be dropped before validate runs, since validate cannot
            // prove a set with no route either way.
            for capability in constraints.retain_routed(&registry) {
                warnings.push(StartupWarning::UnroutedConstraintSet { capability });
            }
        }
        constraints.validate(&registry, &credentials, limits.max_constraint_sets)?;
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
        let mut console_smoke_nonce = [0; 16];
        getrandom::fill(&mut console_smoke_nonce).map_err(|source| {
            BrokerBuildError::BootEntropy {
                source: std::io::Error::other(source),
            }
        })?;
        Ok((
            Self {
                registry,
                policy_digest: policy.digest().to_owned(),
                policy: Arc::new(policy),
                constraints,
                credentials,
                secrets: SecretCatalog::empty(),
                identities,
                broker_principal,
                gate: AuthorizationGate::new(),
                audit,
                chat_memory: None,
                console_smoke_nonce,
            },
            warnings,
        ))
    }

    pub fn with_secret_catalog(mut self, secrets: SecretCatalog) -> Result<Self, BrokerBuildError> {
        secrets.validate(&self.constraints)?;
        self.secrets = secrets;
        Ok(self)
    }

    #[must_use]
    pub fn policy_digest(&self) -> &str {
        &self.policy_digest
    }

    #[must_use]
    pub fn storage_retention_policies(&self) -> RetentionPolicies {
        self.constraints
            .retention_policies()
            .expect("validated storage retention policy")
    }

    pub fn with_chat_memory(mut self, config: ChatMemoryConfig) -> Result<Self, BrokerBuildError> {
        let storage_host = self
            .registry
            .storage_host()
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        config.validate(storage_host.limits())?;
        config.validate_host_limits(self.registry.host_limits())?;
        let expected = [
            (
                CapabilityRoute::ChatMemoryRecord,
                EffectKind::LocalWrite,
                RiskLevel::Medium,
            ),
            (
                CapabilityRoute::ChatMemoryRecent,
                EffectKind::ReadOnly,
                RiskLevel::High,
            ),
            (
                CapabilityRoute::ChatMemorySearch,
                EffectKind::ReadOnly,
                RiskLevel::High,
            ),
        ];
        // This unrouted-role check must run before the shape-check loop below, whose expect call
        // would panic if a role stayed unrouted.
        let unrouted = expected
            .iter()
            .map(|(route, ..)| *route)
            .filter(|route| self.constraints.routed(*route).is_none())
            .collect::<Vec<_>>();
        if !unrouted.is_empty() {
            return Err(BrokerBuildError::UnroutedChatMemory { roles: unrouted });
        }
        let mut routed = BTreeSet::new();
        for (route, effect, risk) in expected {
            let (capability, set) = self
                .constraints
                .routed(route)
                .expect("every chat-memory role was proved routed above");
            routed.insert(capability.as_str());
            if set.effect != effect
                || set.risk != risk
                || set.credential.is_some()
                || set.constraints.http.is_some()
            {
                return Err(BrokerBuildError::InvalidChatMemory);
            }
        }
        // The routed provider must own exactly these three capabilities and no more, since a fourth
        // route on that same component would be reachable under the storage authority the memory
        // surface grants it.
        let provider = self
            .constraints
            .chat_memory_provider()
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let memory_provider = self
            .registry
            .manifests()
            .find(|manifest| &manifest.id == provider)
            .ok_or(BrokerBuildError::InvalidChatMemory)?;
        let declared = memory_provider
            .capabilities
            .iter()
            .map(|capability| capability.id.as_str())
            .collect::<BTreeSet<_>>();
        if declared != routed {
            return Err(BrokerBuildError::InvalidChatMemory);
        }
        self.chat_memory = Some(config);
        Ok(self)
    }

    fn capability_uses_storage(&self, capability: &CapabilityId) -> bool {
        self.constraints
            .get(capability)
            .is_some_and(|set| set.constraints.storage.is_some() || set.route.is_chat_memory())
    }

    /// The serving decision before any secret use is bound; `dekopon-brokerd check --workflows`
    /// relies on it being the decision `invoke` makes.
    pub fn authorize_invocation(
        &self,
        context: &AuthenticatedContext,
        capability: &CapabilityId,
    ) -> Result<CapabilityDecision, Refusal> {
        // This check is the actual enforcement under Leniency::Tolerant, not just defense in
        // depth; never weaken it into an assertion or fold it into the policy decision, or an
        // unrouted but policy-anticipated capability would stop being denied.
        let Some(set) = self.constraints.get(capability).cloned() else {
            return Err(unevaluated_refusal("unconstrained-capability"));
        };
        if !probe_permits(context, set.effect) {
            return Err(unevaluated_refusal("probe-write"));
        }
        if set.constraints.storage.is_some() && context.chat_scope().is_none() {
            return Err(unevaluated_refusal("chat-scope-required"));
        }
        if !storage_scope_permits(context, &set) {
            return Err(unevaluated_refusal("policy-denied"));
        }
        let slots = render_chat_slots(set.constraints.http.as_ref(), context.chat_scope())
            .map_err(unevaluated_refusal)?;
        let decision = self.authorize_capability(context, capability, &set, None);
        if !decision.allowed {
            let reason = denial_reason(&decision, "policy-denied");
            return Err(Refusal {
                evaluation: Some(Box::new(PolicyEvaluation {
                    errors_present: decision.policy.errors_present,
                    cause: decision.policy.refusal,
                    space: decision.space,
                    github_denial: decision.github_denial,
                })),
                ..determined_refusal(reason, decision.policy.determining_policy_ids)
            });
        }
        Ok(CapabilityDecision {
            set,
            policy_ids: decision.policy.determining_policy_ids,
            slots,
            space: decision.space,
            errors_present: decision.policy.errors_present,
        })
    }

    #[must_use]
    pub fn secret_use_or_default(
        &self,
        capability: &CapabilityId,
        proposed: Option<SecretUseProposal>,
    ) -> Option<SecretUseProposal> {
        proposed.or_else(|| {
            let default = self
                .constraints
                .get(capability)?
                .default_secret_use
                .as_ref()?;
            Some(match default.sink {
                DefaultSecretSink::HttpBearer => SecretUseProposal::HttpBearer {
                    secret: default.drn.clone(),
                },
            })
        })
    }

    pub fn bind_secret_use(
        &self,
        context: &AuthenticatedContext,
        capability: &CapabilityId,
        decision: &mut CapabilityDecision,
        secret_use: Option<&SecretUseProposal>,
    ) -> Result<(), Refusal> {
        let Some(secret_use) = secret_use else {
            decision.set.constraints.secret_use = None;
            return Ok(());
        };
        let Some(binding) = self.secrets.binding(capability, secret_use) else {
            return Err(determined_refusal(
                "secret-denied",
                decision.policy_ids.clone(),
            ));
        };
        let secret_decision =
            self.authorize_secret_use(context, capability, &decision.set, secret_use);
        let secret_allowed = secret_decision.allowed;
        let secret_errors = secret_decision.errors_present;
        decision.errors_present |= secret_errors;
        merge_space_decision(&mut decision.space, secret_decision.space);
        decision
            .policy_ids
            .extend(secret_decision.policy.determining_policy_ids);
        decision.policy_ids.sort();
        decision.policy_ids.dedup();
        if !secret_allowed {
            return Err(Refusal {
                evaluation: Some(Box::new(PolicyEvaluation {
                    errors_present: secret_errors,
                    cause: None,
                    space: decision.space.clone(),
                    github_denial: secret_decision.github_denial,
                })),
                ..determined_refusal("secret-denied", decision.policy_ids.clone())
            });
        }
        decision.set.constraints.secret_use = Some(self.secrets.grant(binding));
        Ok(())
    }

    fn authorize_capability(
        &self,
        context: &AuthenticatedContext,
        capability: &CapabilityId,
        set: &ConstraintSet,
        facts: Option<dekopon_policy::HttpRequestFacts>,
    ) -> EvaluatedDecision {
        let request = PolicyRequest {
            principal: context.principal().clone(),
            target: PolicyTarget::Capability {
                capability: capability.clone(),
                provider: set.provider.clone(),
                effect: set.effect,
                risk: set.risk,
            },
            context: policy_context(context),
        };
        Self::evaluate(
            &self.policy,
            request,
            facts,
            context.space.as_ref(),
            context.chat_scope().and_then(|scope| scope.github.as_ref()),
        )
    }

    fn evaluate(
        policy: &PolicyEngine,
        request: PolicyRequest,
        facts: Option<dekopon_policy::HttpRequestFacts>,
        space: Option<&PrincipalId>,
        github: Option<&dekopon_core::github::GithubBatch>,
    ) -> EvaluatedDecision {
        let evaluate = |request, facts| match facts {
            Some(facts) => policy.authorize_http(request, facts),
            None => policy.authorize(request),
        };
        let mut contexts = Vec::new();
        if let Some(batch) = github {
            for event in batch.events() {
                let full = match serde_json::to_value(event) {
                    Ok(value) => value,
                    Err(source) => {
                        tracing::warn!(event="broker_policy_context_failed",error=%source,"broker.policy.context.failed");
                        return EvaluatedDecision {
                            policy: PolicyDecision {
                                errors_present: true,
                                refusal: Some("github-context".to_owned()),
                                ..PolicyDecision::default()
                            },
                            space: None,
                            github_denial: Some(GithubDenial {
                                phase: GithubDenier::Facts,
                                delivery: event.delivery.clone(),
                            }),
                        };
                    }
                };
                let mut stripped = full.clone();
                if let Some(record) = stripped.as_object_mut() {
                    record.remove("authorSet");
                }
                for (value, phase) in [
                    (stripped, GithubDenier::Facts),
                    (full, GithubDenier::AuthorSet),
                ] {
                    let mut context = request.context.clone();
                    context.github = Some(value);
                    contexts.push((
                        context,
                        Some(GithubDenial {
                            phase,
                            delivery: event.delivery.clone(),
                        }),
                    ));
                }
            }
        } else {
            contexts.push((request.context.clone(), None));
        }
        let mut result = EvaluatedDecision {
            policy: PolicyDecision {
                allowed: true,
                ..PolicyDecision::default()
            },
            space: None,
            github_denial: None,
        };
        for (context, coordinate) in contexts {
            let request = PolicyRequest {
                context,
                ..request.clone()
            };
            let asker = evaluate(request.clone(), facts.clone());
            let mut pass = EvaluatedDecision {
                policy: asker,
                space: None,
                github_denial: None,
            };
            if let Some(id) = space {
                let verdict = evaluate(
                    PolicyRequest {
                        principal: id.clone(),
                        ..request
                    },
                    facts.clone(),
                );
                pass.space = Some(SpaceDecision {
                    id: id.clone(),
                    asker_allowed: Some(pass.policy.allowed),
                    allowed: Some(verdict.allowed),
                    policy_ids: verdict.determining_policy_ids.clone(),
                    denied_by: denying_principal(pass.policy.allowed, verdict.allowed),
                });
                merge_policy_decision(&mut pass.policy, verdict);
            }
            if !pass.policy.allowed && result.github_denial.is_none() {
                result.github_denial = coordinate;
            }
            merge_policy_decision(&mut result.policy, pass.policy);
            merge_space_decision(&mut result.space, pass.space);
        }
        result
    }

    fn authorize_secret_use(
        &self,
        context: &AuthenticatedContext,
        capability: &CapabilityId,
        set: &ConstraintSet,
        proposal: &SecretUseProposal,
    ) -> EvaluatedDecision {
        Self::evaluate(
            &self.policy,
            PolicyRequest {
                principal: context.principal().clone(),
                target: PolicyTarget::SecretUse {
                    secret: proposal.secret().clone(),
                    capability: capability.clone(),
                    provider: set.provider.clone(),
                    sink: proposal.sink(),
                },
                context: policy_context(context),
            },
            None,
            context.space.as_ref(),
            context.chat_scope().and_then(|scope| scope.github.as_ref()),
        )
    }

    /// This is the session gate: permitting a principal to talk to an agent is its own explicit
    /// policy statement, not a side effect of holding any capability.
    fn authorize_agent_prompt(
        &self,
        context: &AuthenticatedContext,
        agent: &AgentId,
    ) -> EvaluatedDecision {
        Self::evaluate(
            &self.policy,
            PolicyRequest {
                principal: context.principal().clone(),
                target: PolicyTarget::AgentPrompt {
                    agent: agent.clone(),
                },
                context: policy_context(context),
            },
            None,
            context.space.as_ref(),
            context.chat_scope().and_then(|scope| scope.github.as_ref()),
        )
    }

    /// Every capability listing filters this same set of Cedar evaluations, so what is listed can
    /// never diverge from what an invocation would actually be authorized to do.
    fn authorized_sets(
        &self,
        context: &AuthenticatedContext,
    ) -> Vec<(&CapabilityId, &ConstraintSet)> {
        self.constraints
            .iter()
            .filter(|(capability, set)| {
                set.route.is_generic()
                    && storage_scope_permits(context, set)
                    && (set.constraints.storage.is_none() || context.chat_scope().is_some())
                    && render_chat_slots(set.constraints.http.as_ref(), context.chat_scope())
                        .is_ok()
                    && probe_permits(context, set.effect)
                    && self
                        .authorize_capability(context, capability, set, None)
                        .allowed
            })
            .collect()
    }

    #[must_use]
    pub fn command_words(&self, context: &AuthenticatedContext) -> Vec<String> {
        self.reachable_command_words(context, &self.authorized_sets(context))
    }

    /// Same reachability as `command_words`: a word's `--help` page is visible only where the
    /// word itself is, since a page naming a provider this sender cannot reach would describe a
    /// surface that was never offered.
    #[must_use]
    pub fn command_word_help(&self, context: &AuthenticatedContext) -> BTreeMap<String, String> {
        let words = self.command_words(context);
        self.command_word_help_for(&words)
    }

    fn command_word_help_for(&self, words: &[String]) -> BTreeMap<String, String> {
        let all = self.registry.command_word_help();
        words
            .iter()
            .filter_map(|word| all.get(word).map(|page| (word.clone(), page.clone())))
            .collect()
    }

    fn reachable_providers(
        &self,
        context: &AuthenticatedContext,
        authorized: &[(&CapabilityId, &ConstraintSet)],
    ) -> BTreeSet<ProviderId> {
        let reachable = authorized
            .iter()
            .map(|(_, set)| set.provider.clone())
            .collect::<BTreeSet<_>>();
        let storage_providers = self
            .constraints
            .iter()
            .map(|(_, set)| set)
            .filter(|set| set.constraints.storage.is_some())
            .map(|set| set.provider.clone())
            .collect::<BTreeSet<_>>();
        let reserved = self.constraints.chat_memory_provider().cloned();
        reachable
            .into_iter()
            .filter(|provider| {
                Some(provider) != reserved.as_ref()
                    && (context.chat_scope().is_some() || !storage_providers.contains(provider))
            })
            .collect()
    }

    fn reachable_command_words(
        &self,
        context: &AuthenticatedContext,
        authorized: &[(&CapabilityId, &ConstraintSet)],
    ) -> Vec<String> {
        let reachable = self.reachable_providers(context, authorized);
        let mut words = self
            .registry
            .command_words_by_provider()
            .into_iter()
            .filter(|(provider, _)| reachable.contains(*provider))
            .flat_map(|(_, words)| words.iter().cloned())
            .collect::<Vec<_>>();
        words.sort();
        words.dedup();
        words
    }

    /// Ungated by design: running a word only produces a proposal or rendered text and authorizes
    /// nothing; real authorization happens later, at the invocation that follows.
    #[expect(
        clippy::too_many_arguments,
        reason = "reshaped by the unit that next rewrites this"
    )]
    pub async fn run_command(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: Option<&Attestation>,
        word: &str,
        argv: &[String],
        stdin_piped: bool,
    ) -> Result<CommandRunOutcome, BrokerHostError> {
        let attested = match attestation {
            Some(claim) => {
                let (context, refusal) = self.resolve_context(peer, grant, claim);
                if let Some(refusal) = refusal {
                    report_inspection_refusal(&refusal, peer, &claim.subject, &claim.agent);
                    if refusal.reason == CONSOLE_REAL_SCOPE_REASON {
                        return Ok(CommandRunOutcome::Rendered {
                            stdout: String::new(),
                            stderr: format!("{CONSOLE_REAL_SCOPE_REFUSAL}\n"),
                            status: 2,
                        });
                    }
                    return Err(BrokerHostError::UnknownCommandWord {
                        word: word.to_owned(),
                    });
                }
                Some((context, claim))
            }
            None => None,
        };
        let memory_word = self.is_chat_memory_word(word);
        if memory_word
            && !attested.is_some_and(|(context, claim)| {
                self.memory_surface(&context, &claim.agent).is_some()
            })
        {
            return Err(BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            });
        }
        let outcome = self.registry.run_command(word, argv, stdin_piped).await?;
        if matches!(
            &outcome,
            CommandRunOutcome::Proposed { capability, .. } if {
                let route = self.route(capability);
                if memory_word { !route.is_chat_memory_retrieval() } else { route.is_chat_memory() }
            }
        ) {
            return Err(BrokerHostError::UnknownCommandWord {
                word: word.to_owned(),
            });
        }
        Ok(outcome)
    }

    #[must_use]
    pub fn capabilities(&self, context: &AuthenticatedContext) -> Vec<AvailableCapability> {
        self.available_capabilities(&self.authorized_sets(context))
    }

    #[must_use]
    #[allow(
        clippy::type_complexity,
        reason = "a session's reachable capabilities, command words, and each reachable word's \
                  --help page, in that order; a named struct would only rename what the three \
                  call sites already destructure by position"
    )]
    pub fn capability_view(
        &self,
        context: &AuthenticatedContext,
    ) -> (
        Vec<AvailableCapability>,
        Vec<String>,
        BTreeMap<String, String>,
    ) {
        let authorized = self.authorized_sets(context);
        let words = self.reachable_command_words(context, &authorized);
        let help = self.command_word_help_for(&words);
        (self.available_capabilities(&authorized), words, help)
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
                AvailableCapability {
                    provider: set.provider.clone(),
                    capability,
                }
            })
            .collect::<Vec<_>>();
        capabilities.sort_by(|left, right| left.capability.id.cmp(&right.capability.id));
        capabilities
    }

    #[must_use]
    #[allow(clippy::type_complexity, reason = "see capability_view")]
    pub fn capability_ceiling(
        &self,
    ) -> (
        Vec<AvailableCapability>,
        Vec<String>,
        BTreeMap<String, String>,
    ) {
        let mut capabilities = self
            .constraints
            .iter()
            .filter_map(|(capability, _)| self.available_capability(capability))
            .collect::<Vec<_>>();
        capabilities.sort_by(|left, right| left.capability.id.cmp(&right.capability.id));
        let words = self.registry.command_words();
        let help = self.registry.command_word_help();
        (capabilities, words, help)
    }

    /// Display only: the gateway labels a shared-scope turn with it and decides nothing from it.
    #[must_use]
    pub fn display_principal(&self, subject: &ExternalSubject) -> Option<PrincipalId> {
        self.identities.resolve(subject).cloned()
    }

    #[must_use]
    pub fn principal_ceiling(&self) -> Option<PrincipalId> {
        self.identities
            .principals()
            .max_by_key(|principal| principal.as_str().len())
            .cloned()
    }

    #[must_use]
    pub fn chat_memory_ceiling(&self) -> Option<ChatMemorySurface> {
        let config = self.chat_memory.as_ref()?;
        Some(ChatMemorySurface {
            max_lookback_turns: config.max_lookback_turns,
            prompt_note: MEMORY_PROMPT_NOTE.to_owned(),
        })
    }

    #[must_use]
    #[allow(clippy::type_complexity, reason = "see capability_view")]
    pub fn capability_surface(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: Option<&Attestation>,
    ) -> Option<(
        Vec<AvailableCapability>,
        Vec<String>,
        BTreeMap<String, String>,
        Option<ChatMemorySurface>,
    )> {
        self.session_surface(peer, grant, attestation).ok()
    }

    #[allow(clippy::type_complexity, reason = "see capability_view")]
    pub fn session_surface(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: Option<&Attestation>,
    ) -> Result<
        (
            Vec<AvailableCapability>,
            Vec<String>,
            BTreeMap<String, String>,
            Option<ChatMemorySurface>,
        ),
        Refusal,
    > {
        let Some(claim) = attestation else {
            let (capabilities, words, help) = self.capability_view(peer);
            return Ok((capabilities, words, help, None));
        };
        let (context, refusal) = self.resolve_context(peer, grant, claim);
        if let Some(refusal) = refusal {
            report_inspection_refusal(&refusal, peer, &claim.subject, &claim.agent);
            return Err(refusal);
        }
        let (mut capabilities, mut words, mut help) = self.capability_view(&context);
        let memory = self.memory_surface(&context, &claim.agent);
        if memory.is_some() {
            for route in [
                CapabilityRoute::ChatMemoryRecent,
                CapabilityRoute::ChatMemorySearch,
            ] {
                let (capability, _) = self
                    .constraints
                    .routed(route)
                    .ok_or_else(|| unevaluated_refusal("memory-route-unavailable"))?;
                capabilities.push(
                    self.available_capability(capability)
                        .ok_or_else(|| unevaluated_refusal("memory-route-unavailable"))?,
                );
            }
            capabilities.sort_by(|left, right| left.capability.id.cmp(&right.capability.id));
            let chat_memory_words = self.chat_memory_words();
            help.extend(self.command_word_help_for(&chat_memory_words));
            words.extend(chat_memory_words);
            words.sort();
            words.dedup();
        }
        Ok((capabilities, words, help, memory))
    }

    pub fn space_grants(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: Option<&Attestation>,
    ) -> Option<Vec<CapabilityId>> {
        let claim = attestation?;
        let (mut context, refusal) = self.resolve_context(peer, grant, claim);
        if refusal.is_some() {
            return None;
        }
        context.principal = context.space.as_ref()?.clone();
        let mut capabilities = self.capabilities(&context);
        if self.memory_surface(&context, &claim.agent).is_some() {
            for route in [
                CapabilityRoute::ChatMemoryRecent,
                CapabilityRoute::ChatMemorySearch,
            ] {
                let (capability, _) = self.constraints.routed(route)?;
                capabilities.push(self.available_capability(capability)?);
            }
        }
        capabilities.sort_by(|left, right| left.capability.id.cmp(&right.capability.id));
        Some(
            capabilities
                .into_iter()
                .map(|available| available.capability.id)
                .collect(),
        )
    }

    pub async fn invoke(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: Option<&Attestation>,
        mut request: InvocationRequest,
        assets: dekopon_broker_host::asset::AssetInputs,
    ) -> Result<AssetInvocationResult, BrokerError> {
        let (context, mut refusal) = match attestation {
            Some(claim) => self.resolve_context(peer, grant, claim),
            None => (peer.clone(), None),
        };
        let chat = attestation.filter(|claim| claim.scope.is_some());
        if chat.is_some() {
            refusal = refusal.map(|refusal| {
                if refusal.reason == CONSOLE_REAL_SCOPE_REASON {
                    refusal
                } else {
                    refusal.opaque()
                }
            });
        }
        if let Some(claim) = chat
            && refusal.is_none()
            && !claim.binds(&request.id)
        {
            refusal = Some(unevaluated_refusal(CHAT_REFUSAL));
        }
        let route = self.route(&request.capability);
        if refusal.is_none() {
            if let Some(claim) = chat {
                match route {
                    CapabilityRoute::Generic => {}
                    // Recording is reachable only through `record_delivered_turn`, whatever a
                    // proposal names and whatever attestation carries it.
                    CapabilityRoute::ChatMemoryRecord => {
                        refusal = Some(unevaluated_refusal("record-operation-required"));
                    }
                    CapabilityRoute::ChatMemoryRecent | CapabilityRoute::ChatMemorySearch => {
                        if self.memory_surface(&context, &claim.agent).is_none() {
                            refusal = Some(unevaluated_refusal("memory-unavailable"));
                        } else if let Err(reason) = self.curate_memory_input(route, &mut request) {
                            refusal = Some(unevaluated_refusal(reason));
                        }
                    }
                }
            } else if route.is_chat_memory() {
                refusal = Some(unevaluated_refusal("chat-scope-required"));
            }
        }
        let mut outputs = dekopon_broker_host::asset::AssetOutputs::default();
        let result = self
            .invoke_inner(&context, request, refusal, assets, &mut outputs)
            .await?;
        Ok(AssetInvocationResult {
            result,
            assets: outputs,
        })
    }

    pub async fn record_delivered_turn(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: &Attestation,
        mut turn: DeliveredTurnRequest,
    ) -> Result<InvocationResult, BrokerError> {
        let (context, claim_refusal) = self.resolve_context(peer, grant, attestation);
        if let Some(scope) = context.chat_scope()
            && scope.transport.as_str() == CONSOLE_SMOKE_TRANSPORT
        {
            turn.delivery = console_smoke_delivery(scope, turn.delivery);
        }
        let refusal = claim_refusal
            .map(|refusal| {
                if refusal.reason == CONSOLE_REAL_SCOPE_REASON {
                    refusal
                } else {
                    refusal.opaque()
                }
            })
            .or_else(|| {
                if !attestation.binds(&turn.id) {
                    Some(unevaluated_refusal(CHAT_REFUSAL))
                } else if self.memory_surface(&context, &attestation.agent).is_none() {
                    Some(unevaluated_refusal("memory-unavailable"))
                } else if !turn.is_bounded()
                    || !context
                        .chat_scope()
                        .is_some_and(|scope| turn.delivery.is_canonical_for(scope))
                {
                    Some(unevaluated_refusal("invalid-turn"))
                } else {
                    None
                }
            });
        let capability = self
            .constraints
            .routed(CapabilityRoute::ChatMemoryRecord)
            .map_or_else(
                || {
                    UNROUTED_RECORD_CAPABILITY
                        .parse()
                        .expect("reserved record label is a valid capability identifier")
                },
                |(capability, _)| capability.clone(),
            );
        let input = serde_json::json!({
            "delivery": turn.delivery,
            "user": turn.user(),
            "assistant": turn.assistant(),
        });
        let request = InvocationRequest {
            id: turn.id,
            capability,
            trace_parent: turn.trace_parent,
            secret_use: None,
            input,
        };
        self.invoke_inner(
            &context,
            request,
            refusal,
            Default::default(),
            &mut Default::default(),
        )
        .await
    }

    pub fn resolve_context(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        claim: &Attestation,
    ) -> (AuthenticatedContext, Option<Refusal>) {
        let refused = || {
            let mut context = peer.with_refused_subject(claim.subject.clone());
            context.chat_scope = claim.scope.clone();
            context
        };
        let Some(grant) = grant else {
            return (refused(), Some(unevaluated_refusal("attestation-denied")));
        };
        if self.console_real_scope_refused(peer, claim) {
            return (refused(), Some(console_real_scope_refusal()));
        }
        let Some(principal) = self.identities.resolve(&claim.subject) else {
            let mut refusal = unevaluated_refusal("unmapped-subject");
            if grant.permits(&claim.subject) {
                refusal.session = dekopon_broker_protocol::SessionRefusal::UnmappedSubject;
            }
            return (refused(), Some(refusal));
        };
        if self.identities.principal_kind(principal) == PrincipalKind::Space {
            return (refused(), Some(unevaluated_refusal("attestation-denied")));
        }
        let actor = Actor::Agent {
            agent: claim.agent.clone(),
        };
        if !grant.permits(&claim.subject) {
            return (refused(), Some(unevaluated_refusal("attestation-denied")));
        }
        let scope = match &claim.scope {
            Some(scope) if scope.transport.as_str() == CONSOLE_SMOKE_TRANSPORT => {
                if !is_console_smoke_literal(scope) {
                    return (refused(), Some(unevaluated_refusal(CONSOLE_SMOKE_REFUSAL)));
                }
                let mut derived = scope.clone();
                derived.conversation.id =
                    self.console_smoke_conversation(peer.principal(), &claim.subject, &claim.agent);
                Some(derived)
            }
            scope => scope.clone(),
        };
        let derived = match scope {
            Some(scope) => {
                if !scope
                    .conversation
                    .is_canonical_for(scope.kind, &claim.subject)
                {
                    return (refused(), Some(unevaluated_refusal("attestation-denied")));
                }
                if scope
                    .delivery
                    .as_ref()
                    .is_some_and(|delivery| !delivery.is_canonical_for(&scope))
                {
                    return (refused(), Some(unevaluated_refusal("attestation-denied")));
                }
                if let Some(batch) = &scope.github {
                    let subject = match (claim.subject.tenant(), claim.subject.subject()) {
                        (Some(owner), repo) => format!("{owner}/{repo}"),
                        _ => String::new(),
                    };
                    if scope.kind != ChatTransportKind::Github
                        || scope.trigger != Trigger::Message
                        || !matches!(&scope.delivery,Some(DeliveryIdentity::Github{delivery}) if delivery==batch.lead().delivery.as_str())
                        || batch.events().iter().any(|event| {
                            event.repository.full_name.as_str() != subject
                                || scope.conversation.thread.as_deref()
                                    != Some(event.thread().to_string().as_str())
                        })
                    {
                        return (refused(), Some(unevaluated_refusal("attestation-denied")));
                    }
                }
                AuthenticatedContext::attested_chat(
                    principal.clone(),
                    actor,
                    peer.principal().clone(),
                    claim.subject.clone(),
                    scope,
                )
            }
            None => AuthenticatedContext::attested(
                principal.clone(),
                actor,
                peer.principal().clone(),
                claim.subject.clone(),
            ),
        };
        let Ok(mut context) = derived else {
            return (refused(), Some(unevaluated_refusal("attestation-denied")));
        };
        context.space = context.chat_scope().and_then(|scope| {
            let in_space = match scope.conversation.kind {
                ConversationKind::Channel | ConversationKind::Thread => true,
                ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => false,
            };
            if !in_space {
                return None;
            }
            let service = match scope.kind {
                ChatTransportKind::Fauxlack => "fauxlack",
                ChatTransportKind::Slack => "slack",
                ChatTransportKind::Discord => "discord",
                ChatTransportKind::Telegram
                | ChatTransportKind::Whatsapp
                | ChatTransportKind::Github
                | ChatTransportKind::Local => return None,
            };
            let container = scope.conversation.container.as_deref()?;
            let subject = format!("{service}.{container}.{}", scope.conversation.id)
                .parse()
                .ok()?;
            self.identities
                .resolve(&subject)
                .filter(|mapped| self.identities.principal_kind(mapped) == PrincipalKind::Space)
                .cloned()
        });
        let decision = self.authorize_agent_prompt(&context, &claim.agent);
        if decision.allowed {
            (context, None)
        } else {
            let named_denial = !decision.errors_present
                && match decision.space.as_ref().and_then(|space| space.denied_by) {
                    Some(DenyingPrincipal::Space) => false,
                    Some(DenyingPrincipal::Asker | DenyingPrincipal::Both) | None => true,
                };
            let mut refusal = decided_refusal(decision, "agent-denied");
            if named_denial {
                refusal.session = dekopon_broker_protocol::SessionRefusal::AgentDenied {
                    principal: principal.clone(),
                };
            }
            (context, Some(refusal))
        }
    }

    #[must_use]
    pub fn console_real_scope_refused(
        &self,
        peer: &AuthenticatedContext,
        claim: &Attestation,
    ) -> bool {
        peer.principal().as_str() == "dekopon-console"
            && claim
                .scope
                .as_ref()
                .is_some_and(|scope| scope.transport.as_str() != CONSOLE_SMOKE_TRANSPORT)
    }

    fn console_smoke_conversation(
        &self,
        via: &PrincipalId,
        subject: &ExternalSubject,
        agent: &AgentId,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(CONSOLE_SMOKE_DOMAIN);
        hasher.update(self.console_smoke_nonce);
        for part in [via.as_str(), subject.canonical().as_str(), agent.as_str()] {
            hasher.update(part);
            hasher.update([0]);
        }
        // The hyphen keeps the id outside the Slack, Discord, Telegram and WhatsApp id grammars.
        format!("smoke-{}", hex_bytes(&hasher.finalize()))
    }

    fn route(&self, capability: &CapabilityId) -> CapabilityRoute {
        self.constraints
            .get(capability)
            .map_or(CapabilityRoute::Generic, |set| set.route)
    }

    fn chat_memory_words(&self) -> Vec<String> {
        let Some(provider) = self.constraints.chat_memory_provider() else {
            return Vec::new();
        };
        self.registry
            .command_words_by_provider()
            .into_iter()
            .filter(|(candidate, _)| *candidate == provider)
            .flat_map(|(_, words)| words.iter().cloned())
            .collect()
    }

    fn is_chat_memory_word(&self, word: &str) -> bool {
        self.chat_memory_words().iter().any(|value| value == word)
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
        for route in CapabilityRoute::CHAT_MEMORY {
            let (capability, set) = self.constraints.routed(route)?;
            if !self
                .authorize_capability(context, capability, set, None)
                .allowed
            {
                return None;
            }
        }
        Some(ChatMemorySurface {
            max_lookback_turns: config.max_lookback_turns,
            prompt_note: MEMORY_PROMPT_NOTE.to_owned(),
        })
    }

    fn available_capability(&self, capability: &CapabilityId) -> Option<AvailableCapability> {
        let set = self.constraints.get(capability)?;
        let (_, manifest) = self.registry.capability(capability)?;
        let mut capability = manifest.clone();
        capability.effect = set.effect;
        capability.risk = set.risk;
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
        // This authority encoding excludes principal mapping deliberately; remapping the same
        // subject without changing effective authority must never rotate the generation.

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
                (set.route.is_generic() || effective_memory_surface)
                    && storage_scope_permits(context, set)
                    && self
                        .authorize_capability(context, capability, set, None)
                        .allowed
            })
            .collect::<Vec<_>>();
        let effective_capabilities = effective
            .iter()
            .map(|(capability, _)| (*capability).clone())
            .collect::<BTreeSet<_>>();
        encoded.number("capabilityCount", effective.len() as u128);
        for (capability, set) in effective {
            let digest = artifacts
                .get(&set.provider)
                .ok_or(BrokerError::MemoryUnavailable)?;
            encode_capability_authority(
                &mut encoded,
                capability,
                set,
                self.constraints.credential_for(set, context.actor()),
                digest,
            );
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
        let memory_route = set.route.is_chat_memory();
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
            storage.scope,
            agent,
            subject.clone(),
            scope.kind.to_string(),
            scope.transport.to_string(),
            scope.conversation.id.clone(),
            scope.conversation.key(),
            if memory_route {
                self.chat_memory
                    .as_ref()
                    .map_or(ContinuityPolicy::AuthorityBound, |config| {
                        config.continuity_policy
                    })
            } else if storage.scope == StorageScope::PrivateConversation {
                ContinuityPolicy::AuthorityBound
            } else {
                ContinuityPolicy::Stable
            },
            authority,
        )
        .with_shared_namespace(storage.namespace.clone())
        .with_space_key(context.space_binding().map(|binding| binding.key));
        host.prepare_grant(grant_request)
            .map(Some)
            .map_err(|source| BrokerError::Storage { source })
    }

    fn curate_memory_input(
        &self,
        route: CapabilityRoute,
        request: &mut InvocationRequest,
    ) -> Result<(), &'static str> {
        let config = self.chat_memory.as_ref().ok_or("memory-unavailable")?;
        request.input = match route {
            CapabilityRoute::ChatMemoryRecent => {
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
            CapabilityRoute::ChatMemorySearch => {
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
            CapabilityRoute::Generic | CapabilityRoute::ChatMemoryRecord => {
                return Err("invalid-memory-input");
            }
        };
        Ok(())
    }

    async fn invoke_inner(
        &self,
        context: &AuthenticatedContext,
        mut request: InvocationRequest,
        refusal: Option<Refusal>,
        assets: dekopon_broker_host::asset::AssetInputs,
        outputs: &mut dekopon_broker_host::asset::AssetOutputs,
    ) -> Result<InvocationResult, BrokerError> {
        let authorize = tracing::info_span!(
            "broker.authorize",
            invocation = %request.id,
            capability = %request.capability,
            subject = tracing::field::Empty,
            via = tracing::field::Empty,
            trigger = tracing::field::Empty,
            outcome = tracing::field::Empty,
            policy.errors_present = tracing::field::Empty,
            input = tracing::field::Empty,
            input.bytes = tracing::field::Empty,
        );
        if let Some(subject) = context.attested_subject() {
            authorize.record("subject", tracing::field::display(subject));
        }
        if let Some(via) = context.via() {
            authorize.record("via", tracing::field::display(via));
        }
        if let Some(scope) = context.chat_scope() {
            authorize.record("trigger", scope.trigger.as_str());
        }
        let input = dekopon_core::bounded_display(&request.input);
        authorize.record("input", tracing::field::display(input.text()));
        authorize.record("input.bytes", input.bytes());
        // Instrumented rather than entered with a guard, since a guard held across the audit-append
        // await would stay entered on the worker thread while suspended, misattributing another
        // connection's spans to this request's authorization.
        let authorized = async {
            if let Some(refusal) = refusal {
                authorize.record("outcome", refusal.reason);
                return self
                    .deny(context, &request, refusal)
                    .await
                    .map(ControlFlow::Break);
            }
            // Before the decision: a denial still records the defaulted secret it refused.
            request.secret_use =
                self.secret_use_or_default(&request.capability, request.secret_use.take());
            let mut decision = match self.authorize_invocation(context, &request.capability) {
                Ok(authorized) => authorized,
                Err(refusal) => {
                    // A policy error denies exactly like a non-match; this is recorded as a flag
                    // rather than the error text so denial explanations can't become a
                    // per-request channel leaking policy source or entity data.
                    if let Some(evaluation) = &refusal.evaluation {
                        authorize.record("policy.errors_present", evaluation.errors_present);
                        if evaluation.errors_present {
                            tracing::warn!(
                                event = "broker_policy_evaluation_error",
                                invocation = %request.id,
                                policy.target = "capability",
                                "broker.policy.evaluation.error"
                            );
                        }
                        if let Some(cause) = &evaluation.cause {
                            tracing::warn!(
                                target: "dekopon_broker::audit",
                                {
                                    audit.event = "policy.request.refused",
                                    capability.id = %request.capability,
                                    error.reason = %cause,
                                },
                                "policy.request.refused"
                            );
                        }
                    }
                    authorize.record("outcome", refusal.reason);
                    return self
                        .deny(context, &request, refusal)
                        .await
                        .map(ControlFlow::Break);
                }
            };
            let bound = self.bind_secret_use(
                context,
                &request.capability,
                &mut decision,
                request.secret_use.as_ref(),
            );
            authorize.record("policy.errors_present", decision.errors_present);
            if let Err(refusal) = bound {
                authorize.record("outcome", refusal.reason);
                if refusal
                    .evaluation
                    .as_ref()
                    .is_some_and(|evaluation| evaluation.errors_present)
                {
                    tracing::warn!(
                        event = "broker_policy_evaluation_error",
                        invocation = %request.id,
                        policy.target = "secret",
                        "broker.policy.evaluation.error"
                    );
                }
                return self
                    .deny(context, &request, refusal)
                    .await
                    .map(ControlFlow::Break);
            }
            authorize.record("outcome", "allowed");
            Ok(ControlFlow::Continue(decision))
        }
        .instrument(authorize.clone())
        .await?;
        let decision = match authorized {
            ControlFlow::Break(denied) => return Ok(denied),
            ControlFlow::Continue(allowed) => allowed,
        };
        let CapabilityDecision {
            set,
            policy_ids,
            slots,
            space,
            ..
        } = decision;
        let execute = tracing::info_span!(
            "broker.execute",
            provider = %set.provider,
            credential = tracing::field::Empty,
            storage = tracing::field::Empty,
            storage.namespace = tracing::field::Empty,
            storage.reset = tracing::field::Empty,
            outcome = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_description = tracing::field::Empty,
            error = tracing::field::Empty,
            error.code = tracing::field::Empty,
            error.message = tracing::field::Empty,
        );
        if set.constraints.storage.is_some() {
            execute.record("storage", true);
        }
        if let Some(secret) = request.secret_use.as_ref() {
            execute.record("credential", secret.secret().as_str());
        } else if let Some(credential) = self.constraints.credential_for(&set, context.actor()) {
            execute.record("credential", credential);
        }
        self.execute(
            context, request, set, policy_ids, slots, space, assets, outputs,
        )
        .instrument(execute)
        .await
    }

    async fn deny(
        &self,
        context: &AuthenticatedContext,
        request: &InvocationRequest,
        refusal: Refusal,
    ) -> Result<InvocationResult, BrokerError> {
        let Refusal {
            reason,
            wire,
            policy_ids,
            evaluation,
            ..
        } = refusal;
        let mut authority = decision_authority(
            context,
            self.identities.principal_kind(context.principal()),
            evaluation
                .as_ref()
                .and_then(|evaluation| evaluation.space.clone()),
        );
        authority.github_denial = evaluation
            .as_ref()
            .and_then(|evaluation| evaluation.github_denial.clone());
        let decision_id = format!("deny-{}", request.id);
        let decision = self.decision_reference(&decision_id);
        let trace = request.trace_parent.trace();
        let material = DecisionMaterial {
            invocation: &request.id,
            trace,
            principal: context.principal(),
            actor: context.actor(),
            via: context.via(),
            attested_subject: context.attested_subject(),
            capability: &request.capability,
            secret_use: request.secret_use.as_ref(),
            provider: None,
            authorized_by: &self.broker_principal,
            policy_ids: &policy_ids,
            policy_digest: &self.policy_digest,
            constraints: None,
            allowed: false,
            reason: Some(reason),
            space: authority.space.as_ref(),
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
        self.record_audit(AuditEvent::Decision {
            authority,
            invocation: request.id.clone(),
            trace,
            principal: Some(context.principal().clone()),
            actor: Some(context.actor().clone()),
            via: context.via().cloned(),
            attested_subject: context.attested_subject().cloned(),
            capability: request.capability.clone(),
            secret: request
                .secret_use
                .as_ref()
                .map(|secret| secret.secret().clone()),
            secret_sink: request.secret_use.as_ref().map(SecretUseProposal::sink),
            provider: None,
            authorized_by: Some(self.broker_principal.clone()),
            decision_id: decision_id.clone(),
            policy_ids,
            policy_error_ids: Vec::new(),
            policy_errors_present: evaluation
                .as_ref()
                .and_then(|evaluation| context.space.as_ref().map(|_| evaluation.errors_present)),
            decision_point: "invocation".to_owned(),
            policy_digest: Some(self.policy_digest.clone()),
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
            exit_status: None,
            error: Some(wire.to_owned()),
            detail: None,
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
        reason = "the terminal record takes the exact authorization identity and policy material \
                  already in scope; bundling it would create a second partial invocation type"
    )]
    async fn fail_authorized_before_provider(
        &self,
        context: &AuthenticatedContext,
        invocation: &InvocationId,
        trace: TraceId,
        capability: &CapabilityId,
        decision_id: &str,
        decision: DecisionReference,
        set: &ConstraintSet,
        credential: Option<&str>,
        policy_ids: &[String],
        policy_evidence: Evidence,
        reason: &'static str,
    ) -> Result<InvocationResult, BrokerError> {
        let artifact = self
            .registry
            .loaded_provider_metadata()
            .find(|metadata| metadata.manifest.id == set.provider)
            .map(|metadata| metadata.artifact_sha256);
        let event = execution_event(
            context,
            invocation,
            trace,
            capability,
            decision_id,
            policy_ids,
            &self.policy_digest,
            artifact.as_deref(),
            &self.broker_principal,
            set,
            credential,
            InvocationOutcome::Failed,
            None,
            0,
            Some(reason.to_owned()),
            None,
            Vec::new(),
            None,
            None,
        );
        let execution = tracing::Span::current();
        execution.record("outcome", "failed");
        execution.record("otel.status_description", reason);
        execution.record("error", reason);
        self.record_audit(event).await.map_err(|source| {
            execution.record("outcome", "authorized-failure-unaudited");
            execution.record("error", source.category());
            report_audit_failure("authorized-failure", invocation, &source);
            BrokerError::AuthorizedFailureAudit { source }
        })?;
        Ok(InvocationResult {
            invocation: invocation.clone(),
            decision,
            outcome: InvocationOutcome::Failed,
            exit_status: None,
            error: Some(reason.to_owned()),
            detail: None,
            evidence: vec![policy_evidence],
        })
    }

    #[expect(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "reshaped by the unit that next rewrites this"
    )]
    async fn execute(
        &self,
        context: &AuthenticatedContext,
        mut request: InvocationRequest,
        set: ConstraintSet,
        policy_ids: Vec<String>,
        slots: ChatSlotValues,
        space: Option<SpaceDecision>,
        assets: dekopon_broker_host::asset::AssetInputs,
        outputs: &mut dekopon_broker_host::asset::AssetOutputs,
    ) -> Result<InvocationResult, BrokerError> {
        // Deliberately non-mutating here: the authorization decision must be recorded before
        // materialize can create a namespace, rotate a generation pointer, or update lifecycle
        // state.
        let authority = decision_authority(
            context,
            self.identities.principal_kind(context.principal()),
            space,
        );
        let mut storage_preparation = self.prepare_storage_grant(context, &request, &set)?;
        if let Some(preparation) = &storage_preparation {
            tracing::Span::current().record("storage.namespace", preparation.namespace());
        }
        let storage_scope_commitment = storage_preparation
            .as_ref()
            .map(StorageGrantPreparation::scope_commitment);
        if set.route == CapabilityRoute::ChatMemoryRecord {
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
                          the record route, so the only proposal reaching here is the one \
                          `record_delivered_turn` builds from a typed DeliveryIdentity \
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
                "compactionTargetBytes": config.compaction_target_bytes,
                "compactionThresholdBytes": config.compaction_threshold_bytes,
            });
        }
        let decision_id = format!("allow-{}", request.id);
        let decision = self.decision_reference(&decision_id);
        let invocation_id = request.id.clone();
        let trace = request.trace_parent.trace();
        let capability = request.capability.clone();
        let secret_use = request.secret_use.take();
        let proposal = ProposedInvocation::new(
            request.id,
            request.capability,
            context.actor().clone(),
            trace,
            request.input,
        )
        .with_secret_use(secret_use);
        let request_authorizer = set.constraints.http.as_ref().map(|_| {
            let policy = Arc::clone(&self.policy);
            let audit = Arc::clone(&self.audit);
            let principal = context.principal().clone();
            let actor = context.actor().clone();
            let via = context.via().cloned();
            let subject = context.attested_subject().cloned();
            let provider = set.provider.clone();
            let broker_principal = self.broker_principal.clone();
            let policy_digest = self.policy_digest.clone();
            let effect = set.effect;
            let risk = set.risk;
            let invocation = invocation_id.clone();
            let capability = capability.clone();
            let authority = authority.clone();
            let space = context.space.clone();
            let github = context.chat_scope().and_then(|scope| scope.github.clone());
            let context = policy_context(context);
            let sequence = std::sync::atomic::AtomicUsize::new(0);
            Arc::new(move |facts: dekopon_broker_host::HttpRequestFacts| {
                let request = PolicyRequest {
                    principal: principal.clone(),
                    target: PolicyTarget::Capability {
                        capability: capability.clone(),
                        provider: provider.clone(),
                        effect,
                        risk,
                    },
                    context: context.clone(),
                };
                let facts = dekopon_policy::HttpRequestFacts {
                    method: facts.method,
                    host: facts.host,
                    path: facts.path,
                    query: facts.query,
                    headers: facts.headers,
                    body: facts.body.map_or(
                        dekopon_policy::RequestBody::Streamed,
                        dekopon_policy::RequestBody::Buffered,
                    ),
                    content_type: facts.content_type,
                };
                // Cedar measures its recursion reserve against stacker's thread bounds, which a
                // Wasmtime fiber stack falls outside, so it evaluates on a segment stacker tracks.
                let result = stacker::grow(POLICY_STACK_BYTES, || {
                    Self::evaluate(
                        &policy,
                        request,
                        Some(facts),
                        space.as_ref(),
                        github.as_ref(),
                    )
                });
                let mut authority = authority.clone();
                authority.space = result.space.clone();
                authority.github_denial = result.github_denial.clone();
                let material = DecisionMaterial {
                    invocation: &invocation,
                    trace,
                    principal: &principal,
                    actor: &actor,
                    via: via.as_ref(),
                    attested_subject: subject.as_ref(),
                    capability: &capability,
                    secret_use: None,
                    provider: Some(&provider),
                    authorized_by: &broker_principal,
                    policy_ids: &result.policy.determining_policy_ids,
                    policy_digest: &policy_digest,
                    constraints: None,
                    allowed: result.allowed,
                    reason: (!result.allowed).then(|| denial_reason(&result, "policy-denied")),
                    space: authority.space.as_ref(),
                };
                let digest = decision_evidence_digest("policy-decision", &material);
                let ordinal = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let decision_id = format!("http-{invocation}-{ordinal}");
                if result.errors_present
                    && let Some(opaque) = result.opaque_body
                {
                    tracing::warn!(
                        target: "dekopon_broker::audit",
                        {
                            audit.event = "policy.request.body_opaque",
                            invocation.id = %invocation,
                            capability.id = %capability,
                            decision.id = decision_id.as_str(),
                            request.body.reason = opaque.label(),
                        },
                        "policy.request.body_opaque"
                    );
                }
                let reason =
                    (!result.allowed).then(|| denial_reason(&result, "policy-denied").to_owned());
                let decision_digest = match digest {
                    Ok(digest) => digest,
                    Err(source) => {
                        tracing::error!(
                            event = "broker_decision_evidence_failed",
                            invocation = %invocation,
                            point = "http.request",
                            cause_type = std::any::type_name_of_val(&source),
                            "broker.decision.evidence.failed"
                        );
                        return Box::pin(async { false })
                            as std::pin::Pin<Box<dyn Future<Output = bool> + Send>>;
                    }
                };
                let event = AuditEvent::Decision {
                    authority,
                    invocation: invocation.clone(),
                    trace,
                    principal: Some(principal.clone()),
                    actor: Some(actor.clone()),
                    via: via.clone(),
                    attested_subject: subject.clone(),
                    capability: capability.clone(),
                    secret: None,
                    secret_sink: None,
                    provider: Some(provider.clone()),
                    authorized_by: Some(broker_principal.clone()),
                    decision_id,
                    policy_ids: result.policy.determining_policy_ids,
                    policy_error_ids: result.policy.erroring_policy_ids,
                    policy_errors_present: Some(result.policy.errors_present),
                    decision_point: "http.request".to_owned(),
                    policy_digest: Some(policy_digest.clone()),
                    allowed: result.policy.allowed,
                    reason,
                    decision_digest,
                    storage_scope_commitment: None,
                    storage: None,
                };
                let audit = Arc::clone(&audit);
                let invocation = invocation.clone();
                Box::pin(async move {
                    emit_audit_event(&event);
                    match audit.append(event).await {
                        Ok(()) => result.policy.allowed,
                        Err(source) => {
                            report_audit_failure("decision", &invocation, &source);
                            false
                        }
                    }
                }) as std::pin::Pin<Box<dyn Future<Output = bool> + Send>>
            }) as dekopon_broker_host::RequestAuthorizer
        });
        let authorized = self
            .gate
            .authorize(
                proposal,
                set.provider.clone(),
                decision_id.clone(),
                self.broker_principal.clone(),
                self.policy_hex_digest().to_owned(),
                set.constraints.clone(),
            )
            .map_err(|source| BrokerError::Authorization { source })?;
        let decision_digest = if authority.space.is_some() {
            let material = DecisionMaterial {
                invocation: &invocation_id,
                trace,
                principal: context.principal(),
                actor: context.actor(),
                via: context.via(),
                attested_subject: context.attested_subject(),
                capability: &capability,
                secret_use: authorized.proposal().secret_use.as_ref(),
                provider: Some(&set.provider),
                authorized_by: &self.broker_principal,
                policy_ids: &policy_ids,
                policy_digest: &self.policy_digest,
                constraints: Some(&set.constraints),
                allowed: true,
                reason: None,
                space: authority.space.as_ref(),
            };
            let material = (&authorized, material);
            if let Some(preparation) = storage_preparation.as_ref() {
                let bytes = serde_json::to_vec(&material)
                    .map_err(|source| BrokerError::DecisionEvidence { source })?;
                preparation.evidence_commitment("policy-decision", &bytes)
            } else {
                decision_evidence_digest("policy-decision", &material)?
            }
        } else if let Some(preparation) = storage_preparation.as_ref() {
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

        self.record_audit(AuditEvent::Decision {
            authority,
            invocation: invocation_id.clone(),
            trace,
            principal: Some(context.principal().clone()),
            actor: Some(context.actor().clone()),
            via: context.via().cloned(),
            attested_subject: context.attested_subject().cloned(),
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
            provider: Some(set.provider.clone()),
            authorized_by: Some(self.broker_principal.clone()),
            decision_id: decision_id.clone(),
            policy_ids: policy_ids.clone(),
            policy_error_ids: Vec::new(),
            policy_errors_present: None,
            decision_point: "invocation".to_owned(),
            policy_digest: Some(self.policy_digest.clone()),
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
            Some(preparation) => {
                let span = tracing::Span::current();
                let materialized = tokio::task::spawn_blocking(move || {
                    span.in_scope(|| preparation.materialize())
                })
                .await
                .map_err(|source| BrokerError::StorageTask { source })?;
                Some(materialized.map_err(|source| {
                    if source.namespace_reset() {
                        tracing::Span::current().record("storage.reset", true);
                    }
                    BrokerError::Storage { source }
                })?)
            }
        };

        let proposed_secret = authorized.proposal().secret_use.clone();
        let legacy_credential_name = self
            .constraints
            .credential_for(&set, context.actor())
            .map(str::to_owned);
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
                        "broker.secret.resolution.failed"
                    );
                    return self
                        .fail_authorized_before_provider(
                            context,
                            &invocation_id,
                            trace,
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
                        trace,
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
                        "broker.secret.credential.failed"
                    );
                    return self
                        .fail_authorized_before_provider(
                            context,
                            &invocation_id,
                            trace,
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
            // Refreshing credentials must resolve here, never inside the execution context, or the
            // renewal call would wrongly consume the guest's request budget and evidence.
            match legacy_credential_name
                .as_deref()
                .and_then(|name| self.credentials.get(name))
            {
                None => None,
                Some(StoredCredential::Fixed(credential)) => Some((**credential).clone()),
                Some(StoredCredential::Refreshing(source)) => match source.resolve().await {
                    Ok(credential) => Some(credential),
                    Err(failure) => {
                        if failure.permanent() {
                            tracing::error!(
                                event = "broker_credential_refresh_failed",
                                invocation = %invocation_id,
                                credential = legacy_credential_name.as_deref(),
                                category = failure.category(),
                                retryable = false,
                                "broker.credential.refresh.failed"
                            );
                        } else {
                            tracing::warn!(
                                event = "broker_credential_refresh_failed",
                                invocation = %invocation_id,
                                credential = legacy_credential_name.as_deref(),
                                category = failure.category(),
                                retryable = true,
                                "broker.credential.refresh.failed"
                            );
                        }
                        return self
                            .fail_authorized_before_provider(
                                context,
                                &invocation_id,
                                trace,
                                &capability,
                                &decision_id,
                                decision,
                                &set,
                                audit_credential,
                                &policy_ids,
                                policy_evidence,
                                failure.reason(),
                            )
                            .await;
                    }
                },
            }
        };
        let provider_artifact_sha256 = self
            .registry
            .loaded_provider_metadata()
            .find(|metadata| metadata.manifest.id == set.provider)
            .map(|metadata| metadata.artifact_sha256);
        let started = Instant::now();
        let execution = self
            .registry
            .invoke_with_storage(
                authorized,
                credential,
                storage_grant,
                dekopon_broker_host::HttpInvocation {
                    slots,
                    authorizer: request_authorizer,
                },
                assets,
            )
            .await;
        let duration_ms = duration_millis(started.elapsed());
        let (result, audit_event) = match execution {
            Ok(output) => {
                *outputs = output.assets;
                let mut evidence = vec![policy_evidence];
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
                    trace,
                    &capability,
                    &decision_id,
                    &policy_ids,
                    &self.policy_digest,
                    provider_artifact_sha256.as_deref(),
                    &self.broker_principal,
                    &set,
                    audit_credential,
                    InvocationOutcome::Succeeded,
                    None,
                    duration_ms,
                    None,
                    None,
                    output.http_calls,
                    storage_scope_commitment.clone(),
                    output.storage,
                );
                (
                    InvocationResult {
                        invocation: invocation_id.clone(),
                        decision: decision.clone(),
                        outcome: InvocationOutcome::Succeeded,
                        exit_status: None,
                        error: None,
                        detail: (!output.stderr.is_empty())
                            .then(|| ProviderFailureDetail::new("provider-stderr", &output.stderr)),
                        evidence,
                    },
                    event,
                )
            }
            Err(failure) => {
                let error = public_host_error(&failure.error).to_owned();
                let detail = provider_failure_detail(&failure.error);
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
                    trace,
                    &capability,
                    &decision_id,
                    &policy_ids,
                    &self.policy_digest,
                    provider_artifact_sha256.as_deref(),
                    &self.broker_principal,
                    &set,
                    audit_credential,
                    InvocationOutcome::Failed,
                    Some(&failure.error),
                    duration_ms,
                    Some(error.clone()),
                    detail.clone(),
                    failure.http_calls,
                    storage_scope_commitment.clone(),
                    failure.storage,
                );
                (
                    InvocationResult {
                        invocation: invocation_id.clone(),
                        decision,
                        outcome: InvocationOutcome::Failed,
                        exit_status: provider_exit_status(&failure.error),
                        error: Some(error),
                        detail,
                        evidence,
                    },
                    event,
                )
            }
        };

        let execution = tracing::Span::current();
        match result.outcome {
            InvocationOutcome::Succeeded => {
                execution.record("outcome", "succeeded");
            }
            InvocationOutcome::Denied | InvocationOutcome::Failed => {
                execution.record("outcome", "failed");
                execution.record("otel.status_code", "error");
            }
        }
        if let Some(error) = result.error.as_deref() {
            execution.record("otel.status_description", error);
            execution.record("error", error);
        }
        if let Some(detail) = result.detail.as_ref() {
            execution.record("error.code", detail.code.as_str());
            execution.record("error.message", detail.message.as_str());
        }

        self.record_audit(audit_event).await.map_err(|source| {
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

    async fn record_audit(&self, event: AuditEvent) -> Result<(), AuditError> {
        emit_audit_event(&event);
        self.audit.append(event).await
    }

    fn policy_hex_digest(&self) -> &str {
        self.policy_digest
            .strip_prefix("sha256:")
            .unwrap_or(&self.policy_digest)
    }

    fn decision_reference(&self, decision_id: &str) -> DecisionReference {
        DecisionReference {
            decision_id: decision_id.to_owned(),
            authorized_by: self.broker_principal.clone(),
            policy_digest: self.policy_hex_digest().to_owned(),
        }
    }
}

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

/// Every field here is a semantic authority input: changing any one re-keys retained storage under
/// a fresh generation, and the set is deliberately complete across capability, provider bytes,
/// effect, risk, credential, and constraints.
fn encode_capability_authority(
    encoded: &mut AuthorityEncoder,
    capability: &CapabilityId,
    set: &ConstraintSet,
    credential: Option<&str>,
    provider_artifact_sha256: &str,
) {
    encoded.text("capability", capability.as_str());
    encoded.text("provider", set.provider.as_str());
    encoded.byte("effect", effect_tag(set.effect));
    encoded.byte("risk", risk_tag(set.risk));
    encoded.optional_text("credential", credential);
    encode_execution_constraints(encoded, &set.constraints);
    encoded.text("providerArtifactSha256", provider_artifact_sha256);
}

fn encode_execution_constraints(
    encoded: &mut AuthorityEncoder,
    constraints: &ExecutionConstraints,
) {
    encoded.number("execution.timeoutMs", u128::from(constraints.timeout_ms));
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
        if http.propagate_trace {
            encoded.boolean("execution.http.propagateTrace", true);
        }
        if http.signed_download == Some(dekopon_capability::SignedDownload::GithubActionsLogsV1) {
            encoded.text("execution.http.signedDownload", "github-actions-logs-v1");
        }
        if !http.request_templates.is_empty() {
            let templates = http.request_templates.iter().collect::<BTreeSet<_>>();
            encoded.number(
                "execution.http.requestTemplateCount",
                templates.len() as u128,
            );
            for template in templates {
                encoded.text("execution.http.requestTemplate.method", &template.method);
                encoded.text(
                    "execution.http.requestTemplate.path",
                    &template.path.to_string(),
                );
                encoded.number(
                    "execution.http.requestTemplate.pinnedCount",
                    template.query.pinned.len() as u128,
                );
                for (key, slot) in &template.query.pinned {
                    encoded.text("execution.http.requestTemplate.pinnedKey", key);
                    encoded.text("execution.http.requestTemplate.pinnedSlot", slot.as_str());
                }
                encoded.number(
                    "execution.http.requestTemplate.allowedCount",
                    template.query.allowed.len() as u128,
                );
                for key in &template.query.allowed {
                    encoded.text("execution.http.requestTemplate.allowedKey", key);
                }
            }
        }
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
            match storage.scope {
                StorageScope::PrivateConversation => 0,
                StorageScope::SharedConversation => 1,
                StorageScope::Agent => 2,
                StorageScope::Space => 3,
            },
        );
        if let Some(namespace) = &storage.namespace {
            encoded.text("execution.storage.sharedNamespace", namespace.as_str());
        }
    } else {
        encoded.byte("execution.storage.present", 0);
    }
    if let Some(asset) = &constraints.asset {
        encoded.byte("execution.asset.present", 1);
        encoded.boolean("execution.asset.attach", asset.attach);
        encoded.boolean("execution.asset.remove", asset.remove);
        encoded.boolean("execution.asset.send", asset.send);
    } else {
        encoded.byte("execution.asset.present", 0);
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
        // The label keeps the key's old name: renaming it would rotate every authority-bound generation.
        (
            "storage.maxPendingTransactions",
            limits.max_active_invocations,
        ),
        ("storage.startupMaxEntries", limits.startup_max_entries),
    ] {
        encoded.number(label, u128::from(value));
    }
}

const CHAT_REFUSAL: &str = "chat-attestation-denied";

const CONSOLE_SMOKE_TRANSPORT: &str = "console-smoke";
const CONSOLE_SMOKE_REFUSAL: &str = "console-smoke-claim-denied";
const CONSOLE_REAL_SCOPE_REASON: &str = "console-real-scope-denied";
pub const CONSOLE_REAL_SCOPE_REFUSAL: &str = "dekopon sandbox: console sessions cannot attest a real conversation; use --smoke-conversation or the authenticated gateway";
const CONSOLE_SMOKE_DOMAIN: &[u8] = b"dekopon-console-smoke-v1\0";

/// Slack ids render upper-cased because the attestation carries them lower-cased and the Web API
/// takes them as Slack issued them.
fn render_chat_slots(
    http: Option<&HttpConstraints>,
    scope: Option<&ChatScopeClaim>,
) -> Result<ChatSlotValues, &'static str> {
    let mut values = ChatSlotValues::default();
    let Some(http) = http.filter(|http| !http.request_templates.is_empty()) else {
        return Ok(values);
    };
    let scope = scope.ok_or("chat-scope-required")?;
    let api_form = |id: &str| match scope.kind {
        ChatTransportKind::Slack => id.to_ascii_uppercase(),
        ChatTransportKind::Fauxlack
        | ChatTransportKind::Discord
        | ChatTransportKind::Telegram
        | ChatTransportKind::Whatsapp
        | ChatTransportKind::Github
        | ChatTransportKind::Local => id.to_owned(),
    };
    for slot in http.template_slots() {
        let value = match slot {
            ChatSlot::ConversationId => api_form(&scope.conversation.id),
            ChatSlot::ConversationThread => scope
                .conversation
                .thread
                .clone()
                .ok_or("request-template-slot-absent")?,
            ChatSlot::ConversationApiChannel => {
                api_form(scope.conversation.api_channel(scope.kind))
            }
            ChatSlot::Transport => scope.transport.as_str().to_owned(),
        };
        values.insert(slot, value);
    }
    Ok(values)
}

fn is_console_smoke_literal(scope: &ChatScopeClaim) -> bool {
    let ChatScopeClaim {
        transport: _,
        kind,
        conversation:
            Conversation {
                kind: conversation_kind,
                container,
                id,
                thread,
            },
        delivery,
        participant: _,
        github,
        trigger,
    } = scope;
    *kind == ChatTransportKind::Local
        && *conversation_kind == ConversationKind::DirectMessage
        && container.is_none()
        && id == CONSOLE_SMOKE_TRANSPORT
        && thread.is_none()
        && *trigger == Trigger::Message
        && delivery.is_none()
        && github.is_none()
}

fn console_smoke_delivery(
    scope: &ChatScopeClaim,
    placeholder: DeliveryIdentity,
) -> DeliveryIdentity {
    match placeholder {
        DeliveryIdentity::Local {
            boot_nonce,
            connection,
            sequence,
            ..
        } => DeliveryIdentity::Local {
            transport: scope.transport.clone(),
            conversation: scope.conversation.key(),
            boot_nonce,
            connection,
            sequence,
        },
        placeholder @ (DeliveryIdentity::Fauxlack { .. }
        | DeliveryIdentity::Github { .. }
        | DeliveryIdentity::Slack { .. }
        | DeliveryIdentity::Discord { .. }
        | DeliveryIdentity::Telegram { .. }
        | DeliveryIdentity::Whatsapp { .. }) => placeholder,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DenyingPrincipal {
    Asker,
    Space,
    Both,
}

impl DenyingPrincipal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Asker => "asker",
            Self::Space => "space",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceDecision {
    id: PrincipalId,
    #[serde(skip_serializing_if = "Option::is_none")]
    asker_allowed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    policy_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    denied_by: Option<DenyingPrincipal>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum GithubDenier {
    Facts,
    AuthorSet,
}
impl GithubDenier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::AuthorSet => "author-set",
        }
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GithubDenial {
    phase: GithubDenier,
    delivery: dekopon_core::github::GithubDeliveryId,
}

fn denying_principal(asker: bool, space: bool) -> Option<DenyingPrincipal> {
    match (asker, space) {
        (true, true) => None,
        (false, true) => Some(DenyingPrincipal::Asker),
        (true, false) => Some(DenyingPrincipal::Space),
        (false, false) => Some(DenyingPrincipal::Both),
    }
}
fn merge_policy_decision(previous: &mut PolicyDecision, next: PolicyDecision) {
    previous.allowed &= next.allowed;
    previous.errors_present |= next.errors_present;
    previous
        .determining_policy_ids
        .extend(next.determining_policy_ids);
    previous.determining_policy_ids.sort();
    previous.determining_policy_ids.dedup();
    previous
        .erroring_policy_ids
        .extend(next.erroring_policy_ids);
    previous.erroring_policy_ids.sort();
    previous.erroring_policy_ids.dedup();
    previous.refusal = previous.refusal.take().or(next.refusal);
    previous.opaque_body = previous.opaque_body.take().or(next.opaque_body);
}

struct EvaluatedDecision {
    github_denial: Option<GithubDenial>,
    policy: PolicyDecision,
    space: Option<SpaceDecision>,
}
impl std::ops::Deref for EvaluatedDecision {
    type Target = PolicyDecision;
    fn deref(&self) -> &Self::Target {
        &self.policy
    }
}

fn merge_space_decision(previous: &mut Option<SpaceDecision>, next: Option<SpaceDecision>) {
    if let Some(mut next) = next {
        if let Some(previous) = previous.as_ref() {
            next.asker_allowed = next
                .asker_allowed
                .zip(previous.asker_allowed)
                .map(|(a, b)| a && b);
            next.allowed = next.allowed.zip(previous.allowed).map(|(a, b)| a && b);
            next.policy_ids.extend(previous.policy_ids.iter().cloned());
            next.policy_ids.sort();
            next.policy_ids.dedup();
        }
        next.denied_by = next
            .asker_allowed
            .zip(next.allowed)
            .and_then(|(asker, space)| denying_principal(asker, space));
        *previous = Some(next);
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionAuthority {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_denial: Option<GithubDenial>,
    principal_kind: PrincipalKind,
    started_by: Option<TurnStartedBy>,
    chat: Option<ChatScopeClaim>,
    space: Option<SpaceDecision>,
}

fn decision_authority(
    context: &AuthenticatedContext,
    kind: PrincipalKind,
    space: Option<SpaceDecision>,
) -> Box<DecisionAuthority> {
    Box::new(DecisionAuthority {
        github_denial: None,
        principal_kind: kind,
        started_by: turn_started_by(context),
        chat: context.chat_scope.clone(),
        space: space.or_else(|| {
            context.space.as_ref().map(|id| SpaceDecision {
                id: id.clone(),
                asker_allowed: None,
                allowed: None,
                policy_ids: Vec::new(),
                denied_by: None,
            })
        }),
    })
}

pub struct Refusal {
    pub session: dekopon_broker_protocol::SessionRefusal,
    pub reason: &'static str,
    wire: &'static str,
    pub policy_ids: Vec<String>,
    evaluation: Option<Box<PolicyEvaluation>>,
}

struct PolicyEvaluation {
    github_denial: Option<GithubDenial>,
    errors_present: bool,
    cause: Option<String>,
    space: Option<SpaceDecision>,
}

pub struct CapabilityDecision {
    pub set: ConstraintSet,
    policy_ids: Vec<String>,
    slots: ChatSlotValues,
    space: Option<SpaceDecision>,
    errors_present: bool,
}

impl Refusal {
    fn opaque(mut self) -> Self {
        self.wire = CHAT_REFUSAL;
        self
    }
}

const fn console_real_scope_refusal() -> Refusal {
    Refusal {
        session: dekopon_broker_protocol::SessionRefusal::Other,
        reason: CONSOLE_REAL_SCOPE_REASON,
        wire: CONSOLE_REAL_SCOPE_REFUSAL,
        policy_ids: Vec::new(),
        evaluation: None,
    }
}

const fn unevaluated_refusal(reason: &'static str) -> Refusal {
    Refusal {
        session: dekopon_broker_protocol::SessionRefusal::Other,
        reason,
        wire: reason,
        policy_ids: Vec::new(),
        evaluation: None,
    }
}

fn determined_refusal(reason: &'static str, policy_ids: Vec<String>) -> Refusal {
    Refusal {
        session: dekopon_broker_protocol::SessionRefusal::Other,
        reason,
        wire: reason,
        policy_ids,
        evaluation: None,
    }
}

fn decided_refusal(decision: EvaluatedDecision, denied: &'static str) -> Refusal {
    let reason = denial_reason(&decision, denied);
    Refusal {
        evaluation: Some(Box::new(PolicyEvaluation {
            errors_present: decision.errors_present,
            cause: decision.policy.refusal,
            space: decision.space,
            github_denial: decision.github_denial,
        })),
        ..determined_refusal(reason, decision.policy.determining_policy_ids)
    }
}

fn denial_reason(decision: &EvaluatedDecision, denied: &'static str) -> &'static str {
    if decision.errors_present {
        "policy-error"
    } else {
        denied
    }
}

fn report_inspection_refusal(
    refusal: &Refusal,
    peer: &AuthenticatedContext,
    subject: &ExternalSubject,
    agent: &AgentId,
) {
    tracing::warn!(
        event = "broker_capabilities_refused",
        reason = refusal.reason,
        policy_ids = ?refusal.policy_ids,
        subject = %subject,
        agent = %agent,
        via = %peer.principal(),
        "broker.capabilities.refused"
    );
}

fn emit_audit_event(event: &AuditEvent) {
    if let AuditEvent::Decision { authority, .. } = event
        && let Some(batch) = authority
            .chat
            .as_ref()
            .and_then(|scope| scope.github.as_ref())
    {
        let deliveries = batch.deliveries();
        let labels = batch
            .events()
            .iter()
            .map(dekopon_core::github::GithubEvent::label)
            .collect::<Vec<_>>();
        let labels = labels.iter().map(String::as_str).collect::<Vec<_>>();
        let actors = batch
            .events()
            .iter()
            .map(|event| event.actor.id.get())
            .collect::<Vec<_>>();
        let actors = actors.iter().collect::<Vec<_>>();
        dekopon_telemetry::with_event_string_array("github.delivery", &deliveries, || {
            dekopon_telemetry::with_event_string_array("github.event", &labels, || {
                dekopon_telemetry::with_event_string_array("github.actor.id", &actors, || {
                    emit_audit_event_record(event)
                })
            })
        });
    } else {
        emit_audit_event_record(event);
    }
}

fn emit_audit_event_record(event: &AuditEvent) {
    match event {
        AuditEvent::Decision {
            authority,
            invocation,
            principal,
            actor,
            via,
            attested_subject,
            capability,
            secret,
            secret_sink,
            provider,
            authorized_by,
            decision_id,
            policy_ids,
            policy_error_ids,
            policy_errors_present,
            decision_point,
            policy_digest,
            allowed,
            reason,
            storage_scope_commitment,
            storage,
            decision_digest: _,
            trace: _,
        } => tracing::info!(
            target: "dekopon_broker::audit",
            {
                audit.event = "broker.decision",
                github.denier = authority.github_denial.as_ref().map(|denial|denial.phase.as_str()),
                github.denier.delivery = authority.github_denial.as_ref().map(|denial|denial.delivery.as_str()),
                principal.kind = authority.principal_kind.as_str(),
                turn.started_by = authority.started_by.map(TurnStartedBy::as_str),
                space.id = authority.space.as_ref().map(|space| space.id.as_str()),
                asker.allowed = authority.space.as_ref().and_then(|space| space.asker_allowed),
                space.allowed = authority.space.as_ref().and_then(|space| space.allowed),
                space.policy.ids = authority.space.as_ref().and_then(|space| joined(&space.policy_ids)),
                decision.denied_by = authority.space.as_ref().and_then(|space| space.denied_by.map(DenyingPrincipal::as_str)),
                transport.id = authority.chat.as_ref().map(|chat| chat.transport.as_str()),
                transport.kind = authority.chat.as_ref().map(|chat| chat.kind.to_string()),
                conversation.kind = authority.chat.as_ref().map(|chat| chat.conversation.kind.as_str()),
                conversation.container = authority.chat.as_ref().and_then(|chat| chat.conversation.container.as_deref()),
                conversation.id = authority.chat.as_ref().map(|chat| chat.conversation.id.as_str()),
                conversation.thread = authority.chat.as_ref().and_then(|chat| chat.conversation.thread.as_deref()),
                participant.handle = authority.chat.as_ref().and_then(|chat| chat.participant.as_deref()),
                delivery.kind = authority.chat.as_ref().and_then(|chat| chat.delivery.as_ref().map(delivery_kind)),
                delivery.id = authority.chat.as_ref().and_then(|chat| chat.delivery.as_ref().map(delivery_id)),
                invocation.id = %invocation,
                capability.id = %capability,
                decision.id = decision_id.as_str(),
                decision.allowed = allowed,
                decision.reason = reason.as_deref(),
                principal = principal.as_ref().map(ToString::to_string),
                actor.kind = actor.as_ref().map(actor_kind),
                actor.id = actor.as_ref().map(actor_id),
                via = via.as_ref().map(ToString::to_string),
                subject = attested_subject.as_ref().map(ToString::to_string),
                provider = provider.as_ref().map(ToString::to_string),
                authorized.by = authorized_by.as_ref().map(ToString::to_string),
                policy.ids = joined(policy_ids),
                policy.error_ids = joined(policy_error_ids),
                policy.errors_present = policy_errors_present,
                decision.point = decision_point.as_str(),
                policy.digest = policy_digest.as_deref(),
                secret = secret.as_ref().map(ToString::to_string),
                secret.sink = secret_sink.as_ref().map(ToString::to_string),
                storage.scope_commitment = storage_scope_commitment
                    .as_ref()
                    .map(StorageScopeCommitment::as_str),
                storage.evidence = storage.as_ref().and_then(rendered),
            },
            "broker.decision"
        ),
        AuditEvent::Execution {
            invocation,
            principal,
            actor,
            via,
            attested_subject,
            capability,
            secret,
            secret_sink,
            provider,
            authorized_by,
            decision_id,
            policy_ids,
            policy_digest,
            provider_artifact_sha256,
            effect,
            risk,
            credential,
            outcome,
            exit_status,
            duration_ms,
            error,
            error_detail,
            http_calls,
            storage_scope_commitment,
            storage,
            trace: _,
        } => tracing::info!(
            target: "dekopon_broker::audit",
            {
                audit.event = "broker.execution",
                invocation.id = %invocation,
                capability.id = %capability,
                decision.id = decision_id.as_str(),
                principal = principal.as_ref().map(ToString::to_string),
                actor.kind = actor.as_ref().map(actor_kind),
                actor.id = actor.as_ref().map(actor_id),
                via = via.as_ref().map(ToString::to_string),
                subject = attested_subject.as_ref().map(ToString::to_string),
                provider = provider.as_ref().map(ToString::to_string),
                authorized.by = authorized_by.as_ref().map(ToString::to_string),
                policy.ids = joined(policy_ids),
                policy.digest = policy_digest.as_deref(),
                provider.artifact_sha256 = provider_artifact_sha256.as_deref(),
                secret = secret.as_ref().map(ToString::to_string),
                secret.sink = secret_sink.as_ref().map(ToString::to_string),
                effect = ?effect,
                risk = ?risk,
                credential = credential.as_deref(),
                outcome = ?outcome,
                process.exit.code = exit_status.map(std::num::NonZeroU8::get),
                duration_ms = duration_ms,
                error = error.as_deref(),
                error.code = error_detail.as_ref().map(|detail| detail.code.as_str()),
                error.message = error_detail.as_ref().map(|detail| detail.message.as_str()),
                http.calls = rendered(http_calls),
                storage.scope_commitment = storage_scope_commitment
                    .as_ref()
                    .map(StorageScopeCommitment::as_str),
                storage.evidence = storage.as_ref().and_then(rendered),
            },
            "broker.execution"
        ),
    }
}

fn delivery_kind(delivery: &DeliveryIdentity) -> &'static str {
    match delivery {
        DeliveryIdentity::Fauxlack { .. } => "fauxlack",
        DeliveryIdentity::Slack { .. } => "slack",
        DeliveryIdentity::Discord { .. } => "discord",
        DeliveryIdentity::Telegram { .. } => "telegram",
        DeliveryIdentity::Whatsapp { .. } => "whatsapp",
        DeliveryIdentity::Local { .. } => "local",
        DeliveryIdentity::Github { .. } => "github",
    }
}

fn delivery_id(delivery: &DeliveryIdentity) -> String {
    match delivery {
        DeliveryIdentity::Slack { timestamp, .. } => timestamp.clone(),
        DeliveryIdentity::Fauxlack { message, .. }
        | DeliveryIdentity::Discord { message, .. }
        | DeliveryIdentity::Telegram { message, .. }
        | DeliveryIdentity::Whatsapp { message, .. } => message.clone(),
        DeliveryIdentity::Github { delivery } => delivery.clone(),
        DeliveryIdentity::Local {
            boot_nonce,
            connection,
            sequence,
            ..
        } => format!("{boot_nonce}-{connection}-{sequence}"),
    }
}

const fn actor_kind(actor: &Actor) -> &'static str {
    match actor {
        Actor::Human { .. } => "human",
        Actor::Agent { .. } => "agent",
        Actor::Service { .. } => "service",
    }
}

fn actor_id(actor: &Actor) -> String {
    match actor {
        Actor::Human { principal } | Actor::Service { principal } => principal.to_string(),
        Actor::Agent { agent } => agent.to_string(),
    }
}

fn rendered<T: Serialize>(value: &T) -> Option<String> {
    let json = serde_json::to_string(value).ok()?;
    (json != "[]" && json != "null").then_some(json)
}

fn joined(ids: &[String]) -> Option<String> {
    (!ids.is_empty()).then(|| ids.join(","))
}

fn report_audit_failure(stage: &'static str, invocation: &InvocationId, source: &AuditError) {
    tracing::error!(
        event = "broker_audit_append_failed",
        audit.stage = stage,
        category = source.category(),
        invocation = %invocation,
        error = %error_chain(source),
        "broker.audit.append.failed"
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "audit construction keeps every trusted correlation field explicit"
)]
fn execution_event(
    context: &AuthenticatedContext,
    invocation: &InvocationId,
    trace: TraceId,
    capability: &CapabilityId,
    decision_id: &str,
    policy_ids: &[String],
    policy_digest: &str,
    provider_artifact_sha256: Option<&str>,
    authorized_by: &PrincipalId,
    set: &ConstraintSet,
    credential: Option<&str>,
    outcome: InvocationOutcome,
    provider_failure: Option<&BrokerHostError>,
    duration_ms: u64,
    error: Option<String>,
    error_detail: Option<ProviderFailureDetail>,
    http_calls: Vec<HttpCallEvidence>,
    storage_scope_commitment: Option<StorageScopeCommitment>,
    storage: Option<StorageEvidence>,
) -> AuditEvent {
    AuditEvent::Execution {
        invocation: invocation.clone(),
        trace,
        principal: Some(context.principal().clone()),
        actor: Some(context.actor().clone()),
        via: context.via().cloned(),
        attested_subject: context.attested_subject().cloned(),
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
        provider: Some(set.provider.clone()),
        authorized_by: Some(authorized_by.clone()),
        decision_id: decision_id.to_owned(),
        policy_ids: policy_ids.to_vec(),
        policy_digest: Some(policy_digest.to_owned()),
        provider_artifact_sha256: provider_artifact_sha256.map(str::to_owned),
        effect: set.effect,
        risk: set.risk,
        credential: credential.map(str::to_owned),
        outcome,
        exit_status: provider_failure.and_then(provider_execution_exit_status),
        duration_ms,
        error,
        error_detail,
        http_calls,
        storage_scope_commitment,
        storage,
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn provider_failure_detail(error: &BrokerHostError) -> Option<ProviderFailureDetail> {
    match error {
        BrokerHostError::ProviderFailure { stderr, .. } => {
            Some(ProviderFailureDetail::new("provider-exit", stderr))
        }
        BrokerHostError::HostCallRejected {
            reason:
                dekopon_broker_host::HostCallRejection::Http(
                    dekopon_broker_host::PolicyViolation::RedirectRefused,
                ),
            ..
        } => Some(ProviderFailureDetail::new(
            "redirect-refused",
            "signed download redirect refused",
        )),
        _ => None,
    }
}

fn provider_execution_exit_status(error: &BrokerHostError) -> Option<std::num::NonZeroU8> {
    match error {
        BrokerHostError::ProviderFailure { .. } => provider_exit_status(error),
        BrokerHostError::PeerDisconnected
        | BrokerHostError::AssetOverBudget
        | BrokerHostError::AssetInput { .. }
        | BrokerHostError::NoProviders
        | BrokerHostError::InvalidLimit { .. }
        | BrokerHostError::AuthorizationExceedsHostLimit { .. }
        | BrokerHostError::InvalidHttpAuthorization
        | BrokerHostError::MixedHostAuthorization
        | BrokerHostError::InvalidSecretAuthorization { .. }
        | BrokerHostError::SecretAuthorizationExceedsHttp
        | BrokerHostError::SecretCredentialMismatch
        | BrokerHostError::StorageDisabled
        | BrokerHostError::MissingStorageGrant
        | BrokerHostError::UnexpectedStorageGrant
        | BrokerHostError::StorageGrantMismatch
        | BrokerHostError::Storage { .. }
        | BrokerHostError::HttpConfiguration { .. }
        | BrokerHostError::Engine { .. }
        | BrokerHostError::Store { .. }
        | BrokerHostError::Linker { .. }
        | BrokerHostError::InvalidArtifactSize { .. }
        | BrokerHostError::InvalidArtifactDigest
        | BrokerHostError::ArtifactTooLarge { .. }
        | BrokerHostError::ArtifactSizeMismatch { .. }
        | BrokerHostError::ArtifactDigestMismatch { .. }
        | BrokerHostError::ProviderIdentityMismatch { .. }
        | BrokerHostError::ArtifactMetadata { .. }
        | BrokerHostError::CompiledArtifactMissing { .. }
        | BrokerHostError::CompiledArtifact { .. }
        | BrokerHostError::CacheMaintenance { .. }
        | BrokerHostError::PrecompileWorker { .. }
        | BrokerHostError::MemoryBudgetExhausted { .. }
        | BrokerHostError::Compile { .. }
        | BrokerHostError::Instantiate { .. }
        | BrokerHostError::DescribeUsedHostImport { .. }
        | BrokerHostError::ConflictingProviders { .. }
        | BrokerHostError::InvalidCommandRun { .. }
        | BrokerHostError::UnknownCommandWord { .. }
        | BrokerHostError::MissingCommandExport { .. }
        | BrokerHostError::CommandExportSignature { .. }
        | BrokerHostError::CommandInputTooLarge { .. }
        | BrokerHostError::RunCommand { .. }
        | BrokerHostError::RunCommandUsedHostImport { .. }
        | BrokerHostError::Describe { .. }
        | BrokerHostError::InvalidManifest { .. }
        | BrokerHostError::Manifest { .. }
        | BrokerHostError::UnknownCapability { .. }
        | BrokerHostError::AuthorizedProviderMismatch { .. }
        | BrokerHostError::ProviderDoesNotImplement { .. }
        | BrokerHostError::InputNotObject { .. }
        | BrokerHostError::SerializeInput { .. }
        | BrokerHostError::InputTooLarge { .. }
        | BrokerHostError::OutputTooLarge { .. }
        | BrokerHostError::Timeout { .. }
        | BrokerHostError::HostCallRejected { .. }
        | BrokerHostError::StorageCallRejected { .. }
        | BrokerHostError::Invoke { .. }
        | BrokerHostError::StdioAdmission { .. } => None,
    }
}

fn provider_exit_status(error: &BrokerHostError) -> Option<std::num::NonZeroU8> {
    if let BrokerHostError::ProviderFailure { status, .. } = error {
        std::num::NonZeroU8::new(*status)
    } else if let BrokerHostError::HostCallRejected {
        reason:
            dekopon_broker_host::HostCallRejection::Http(
                dekopon_broker_host::PolicyViolation::RedirectRefused,
            ),
        ..
    } = error
    {
        std::num::NonZeroU8::new(3)
    } else {
        None
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecisionMaterial<'a> {
    invocation: &'a InvocationId,
    trace: TraceId,
    principal: &'a PrincipalId,
    actor: &'a Actor,
    via: Option<&'a PrincipalId>,
    attested_subject: Option<&'a ExternalSubject>,
    capability: &'a CapabilityId,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_use: Option<&'a SecretUseProposal>,
    provider: Option<&'a ProviderId>,
    authorized_by: &'a PrincipalId,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    policy_ids: &'a [String],
    policy_digest: &'a str,
    constraints: Option<&'a ExecutionConstraints>,
    allowed: bool,
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    space: Option<&'a SpaceDecision>,
}

fn decision_evidence_digest(label: &str, value: &impl Serialize) -> Result<String, BrokerError> {
    evidence_digest(label, value).map_err(|source| BrokerError::DecisionEvidence { source })
}

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
    Ok(digest_parts(
        EVIDENCE_HASH_DOMAIN,
        &[label.as_bytes(), &[0], &serde_json::to_vec(value)?],
    ))
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

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
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
        // These startup-only variants are named explicitly rather than wildcarded, so a future
        // variant can never be silently misclassified here.
        BrokerHostError::ConflictingProviders { .. }
        | BrokerHostError::MissingCommandExport { .. }
        | BrokerHostError::CommandExportSignature { .. }
        | BrokerHostError::InvalidArtifactSize { .. }
        | BrokerHostError::InvalidArtifactDigest
        | BrokerHostError::ArtifactTooLarge { .. }
        | BrokerHostError::ArtifactSizeMismatch { .. }
        | BrokerHostError::ArtifactDigestMismatch { .. }
        | BrokerHostError::ProviderIdentityMismatch { .. } => "provider-configuration",
        BrokerHostError::MemoryBudgetExhausted { .. } => "host-memory-budget",
        BrokerHostError::AssetOverBudget => "over-budget",
        BrokerHostError::AuthorizedProviderMismatch { .. } => "authorized-provider-mismatch",
        BrokerHostError::AssetInput { .. }
        | BrokerHostError::InputNotObject { .. }
        | BrokerHostError::SerializeInput { .. }
        | BrokerHostError::InputTooLarge { .. }
        | BrokerHostError::CommandInputTooLarge { .. } => "invalid-input",
        BrokerHostError::OutputTooLarge { .. } => "invalid-provider-output",
        BrokerHostError::RunCommand { .. }
        | BrokerHostError::RunCommandUsedHostImport { .. }
        | BrokerHostError::InvalidCommandRun { .. } => "command-rewrite-failed",
        BrokerHostError::Timeout { .. } => "provider-timeout",
        BrokerHostError::PeerDisconnected => "peer-disconnected",
        BrokerHostError::HostCallRejected { reason, .. } => match reason {
            dekopon_broker_host::HostCallRejection::Http(violation) => violation.label(),
            dekopon_broker_host::HostCallRejection::MonotonicOverflow
            | dekopon_broker_host::HostCallRejection::RandomTooLarge
            | dekopon_broker_host::HostCallRejection::RandomEntropy
            | dekopon_broker_host::HostCallRejection::AssetCallRejected => reason.label(),
        },
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
            | dekopon_storage_host::StorageHostError::CorruptLayout { .. } => "storage-corrupt",
            _ => "storage-io",
        },
        BrokerHostError::Invoke { .. } => "provider-trap",
        BrokerHostError::ProviderFailure { .. } => "provider-failure",
        BrokerHostError::StdioAdmission { .. } => "invalid-input",
        BrokerHostError::NoProviders
        | BrokerHostError::InvalidLimit { .. }
        | BrokerHostError::Engine { .. }
        | BrokerHostError::Store { .. }
        | BrokerHostError::Linker { .. }
        | BrokerHostError::ArtifactMetadata { .. }
        | BrokerHostError::CompiledArtifact { .. }
        | BrokerHostError::CompiledArtifactMissing { .. }
        | BrokerHostError::CacheMaintenance { .. }
        | BrokerHostError::PrecompileWorker { .. }
        | BrokerHostError::Compile { .. }
        | BrokerHostError::Instantiate { .. }
        | BrokerHostError::DescribeUsedHostImport { .. }
        | BrokerHostError::Describe { .. }
        | BrokerHostError::InvalidManifest { .. }
        | BrokerHostError::Manifest { .. } => "broker-host-failure",
    }
}

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("chat memory is unavailable")]
    MemoryUnavailable,
    #[error("chat memory input is invalid")]
    InvalidMemoryInput,
    #[error("broker storage authority failed")]
    Storage {
        #[source]
        source: dekopon_storage_host::StorageHostError,
    },
    #[error("broker storage materialization did not complete")]
    StorageTask {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("broker could not create constrained authorization")]
    Authorization {
        #[source]
        source: AuthorizationError,
    },
    #[error("broker could not serialize bounded decision evidence")]
    DecisionEvidence {
        #[source]
        source: serde_json::Error,
    },
    #[error("broker could not audit its authorization decision")]
    DecisionAudit {
        #[source]
        source: AuditError,
    },
    #[error("broker could not audit an authorized pre-provider failure")]
    AuthorizedFailureAudit {
        #[source]
        source: AuditError,
    },
    #[error("broker could not serialize terminal evidence for {invocation}")]
    OutcomeEvidence {
        invocation: InvocationId,
        #[source]
        source: serde_json::Error,
    },
    #[error("broker could not audit terminal execution for {invocation}")]
    OutcomeAudit {
        invocation: InvocationId,
        #[source]
        source: AuditError,
    },
}

impl BrokerError {
    #[must_use]
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    pub const fn storage_failure_code(&self) -> Option<&'static str> {
        let source = match self {
            Self::Storage { source } => source,
            Self::StorageTask { .. } => return Some("storage-io"),
            _ => return None,
        };
        Some(match source {
            dekopon_storage_host::StorageHostError::QuotaExceeded
            | dekopon_storage_host::StorageHostError::Arithmetic => "storage-quota",
            dekopon_storage_host::StorageHostError::Busy => "storage-busy",
            dekopon_storage_host::StorageHostError::Timeout => "storage-timeout",
            dekopon_storage_host::StorageHostError::Corrupt { .. }
            | dekopon_storage_host::StorageHostError::CorruptLayout { .. } => "storage-corrupt",
            _ => "storage-io",
        })
    }

    #[must_use]
    pub fn storage_namespace_reset(&self) -> bool {
        matches!(self, Self::Storage { source } if source.namespace_reset())
    }

    /// This class is for an exhaustion no resubmission can ever outlast, since the bounded audit
    /// log never evicts; OutcomeAudit is deliberately excluded here since its unaudited-outcome
    /// classification must not be weakened to this one.
    #[must_use]
    pub const fn capacity_failure_code(&self) -> Option<&'static str> {
        match self {
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
            | Self::OutcomeEvidence { .. }
            | Self::OutcomeAudit { .. } => None,
        }
    }

    /// Some means provider execution may already have run with no terminal audit record, so the
    /// request must never be resubmitted under any identifier since that could duplicate a
    /// non-idempotent effect; None means resubmission is safe.
    #[must_use]
    pub const fn unaudited_outcome(&self) -> Option<&InvocationId> {
        match self {
            Self::OutcomeEvidence { invocation, .. } | Self::OutcomeAudit { invocation, .. } => {
                Some(invocation)
            }
            Self::MemoryUnavailable
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

fn storage_scope_permits(context: &AuthenticatedContext, set: &ConstraintSet) -> bool {
    match set
        .constraints
        .storage
        .as_ref()
        .map(|storage| storage.scope)
    {
        Some(StorageScope::Space) => context.space.is_some(),
        Some(StorageScope::Agent) => context.space.is_none(),
        Some(StorageScope::PrivateConversation | StorageScope::SharedConversation) | None => true,
    }
}
