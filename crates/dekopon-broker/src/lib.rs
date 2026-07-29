//! Deny-by-default authorization, execution, evidence, and audit core for Dekopon.
//!
//! This crate is broker-owned machinery. A transport supplies an [`AuthenticatedContext`] derived
//! from trusted peer identity, never payload claims. [`Broker`] binds that context to an exact
//! policy rule, creates a single-use authorization, executes it through `dekopon-broker-host`, and
//! records metadata-only hash-linked audit events.
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

use std::{collections::BTreeSet, future::Future, sync::Arc, time::Instant};

use dekopon_broker_host::{
    BrokerHostError, BrokerProviderRegistry, HttpCallEvidence, ProviderCapability,
};
use dekopon_capability::{
    AuthorizationError, DecisionReference, EffectKind, Evidence, ExecutionConstraints, Idempotency,
    InvocationOutcome, InvocationResult, ProposedInvocation, broker::AuthorizationGate,
};
use dekopon_core::{
    Actor, CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel, TraceId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;

const MAX_POLICY_REVISION_BYTES: usize = 256;
const MAX_POLICY_SCOPE_ENTRIES: usize = 64;
const MAX_POLICY_SCOPE_VALUE_BYTES: usize = 512;
const MAX_HTTP_METHOD_BYTES: usize = 64;
const AUDIT_HASH_DOMAIN: &[u8] = b"dekopon-audit-record-v1\0";
const EVIDENCE_HASH_DOMAIN: &[u8] = b"dekopon-evidence-v1\0";
const POLICY_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.policy-decision+json";
const PROVIDER_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.provider-response+json";
const HTTP_EVIDENCE_MEDIA_TYPE: &str = "application/vnd.dekopon.http-evidence+json";

/// Default maximum exact policy rules in one broker instance.
pub const DEFAULT_MAX_POLICY_RULES: usize = 1_024;
/// Default process-lifetime invocation identifiers retained for replay rejection.
pub const DEFAULT_MAX_REPLAY_IDS: usize = 100_000;
/// Default maximum records retained by an in-memory audit log.
pub const DEFAULT_MAX_AUDIT_RECORDS: usize = 200_000;

/// Identity established by a trusted broker transport.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticatedContext {
    principal: PrincipalId,
    actor: Actor,
}

impl AuthenticatedContext {
    /// Binds a transport-authenticated principal to its trusted actor identity.
    ///
    /// Human and service actors must carry the same principal established by the transport.
    /// Agent actors may be represented by an authenticated daemon/service principal and are
    /// therefore bound by an explicit exact policy rule instead.
    pub fn new(principal: PrincipalId, actor: Actor) -> Result<Self, ContextError> {
        let actor_principal = match &actor {
            Actor::Human { principal } | Actor::Service { principal } => Some(principal),
            Actor::Agent { .. } => None,
        };
        if actor_principal.is_some_and(|actor_principal| actor_principal != &principal) {
            return Err(ContextError::PrincipalMismatch);
        }
        Ok(Self { principal, actor })
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
}

/// Invalid trusted identity binding.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ContextError {
    /// A human/service payload identity disagreed with transport authentication.
    #[error("authenticated principal does not match the human or service actor")]
    PrincipalMismatch,
}

/// Unprivileged invocation fields accepted from a broker client.
///
/// Actor and principal are deliberately absent: the transport supplies them separately.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InvocationRequest {
    /// Client-selected identifier reserved for replay rejection.
    pub id: InvocationId,
    /// Requested exact capability.
    pub capability: CapabilityId,
    /// End-to-end correlation identifier.
    pub trace: TraceId,
    /// Capability-specific untrusted input.
    pub input: Value,
}

/// One exact, deny-by-default broker policy rule.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PolicyRule {
    /// Exact authenticated transport principal.
    pub principal: PrincipalId,
    /// Exact trusted actor identity.
    pub actor: Actor,
    /// Exact capability route.
    pub capability: CapabilityId,
    /// Expected provider selected by the trusted route.
    pub provider: ProviderId,
    /// Trusted effect classification.
    pub effect: EffectKind,
    /// Trusted risk classification.
    pub risk: RiskLevel,
    /// Trusted retry classification.
    pub idempotency: Idempotency,
    /// Execution and optional HTTP authority granted by this rule.
    pub constraints: ExecutionConstraints,
}

/// Independent broker limits for policy and replay state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrokerLimits {
    /// Maximum exact rules accepted at construction.
    pub max_policy_rules: usize,
    /// Maximum invocation IDs retained for this process lifetime.
    pub max_replay_ids: usize,
}

impl Default for BrokerLimits {
    fn default() -> Self {
        Self {
            max_policy_rules: DEFAULT_MAX_POLICY_RULES,
            max_replay_ids: DEFAULT_MAX_REPLAY_IDS,
        }
    }
}

#[derive(Debug)]
struct ExactPolicy {
    revision: String,
    rules: Vec<PolicyRule>,
}

impl ExactPolicy {
    fn new(
        revision: String,
        rules: Vec<PolicyRule>,
        registry: &BrokerProviderRegistry,
        maximum: usize,
    ) -> Result<Self, BrokerBuildError> {
        if revision.trim().is_empty()
            || revision.trim() != revision
            || revision.len() > MAX_POLICY_REVISION_BYTES
        {
            return Err(BrokerBuildError::InvalidPolicyRevision);
        }
        if rules.len() > maximum {
            return Err(BrokerBuildError::TooManyPolicyRules {
                count: rules.len(),
                maximum,
            });
        }

        for (index, rule) in rules.iter().enumerate() {
            if matches!(
                &rule.actor,
                Actor::Human { principal } | Actor::Service { principal }
                    if principal != &rule.principal
            ) {
                return Err(BrokerBuildError::PolicyIdentityMismatch);
            }
            validate_rule_constraints(&rule.constraints)?;
            registry
                .validate_constraints(&rule.constraints)
                .map_err(|source| BrokerBuildError::HostConstraint { source })?;
            let (provider, capability) = registry
                .capabilities()
                .find(|(_, capability)| capability.id == rule.capability)
                .ok_or_else(|| BrokerBuildError::UnknownCapability {
                    capability: rule.capability.clone(),
                })?;
            validate_trusted_metadata(rule, provider, capability)?;

            if rules[..index].iter().any(|candidate| {
                candidate.principal == rule.principal
                    && candidate.actor == rule.actor
                    && candidate.capability == rule.capability
            }) {
                return Err(BrokerBuildError::DuplicatePolicyRule {
                    principal: rule.principal.clone(),
                    capability: rule.capability.clone(),
                });
            }
        }

        Ok(Self { revision, rules })
    }

    fn matching_rule(
        &self,
        context: &AuthenticatedContext,
        capability: &CapabilityId,
    ) -> Option<&PolicyRule> {
        self.rules.iter().find(|rule| {
            &rule.principal == context.principal()
                && &rule.actor == context.actor()
                && &rule.capability == capability
        })
    }
}

fn validate_trusted_metadata(
    rule: &PolicyRule,
    provider: &ProviderId,
    capability: &ProviderCapability,
) -> Result<(), BrokerBuildError> {
    if &rule.provider != provider {
        return Err(BrokerBuildError::ProviderMismatch {
            capability: rule.capability.clone(),
            expected: rule.provider.clone(),
            actual: provider.clone(),
        });
    }
    for (field, matches) in [
        ("effect", rule.effect == capability.effect),
        ("risk", rule.risk == capability.risk),
        ("idempotency", rule.idempotency == capability.idempotency),
    ] {
        if !matches {
            return Err(BrokerBuildError::CapabilityMetadataMismatch {
                capability: rule.capability.clone(),
                field,
            });
        }
    }
    Ok(())
}

fn validate_rule_constraints(constraints: &ExecutionConstraints) -> Result<(), BrokerBuildError> {
    if constraints.timeout_ms == 0 || constraints.max_output_bytes == 0 {
        return Err(BrokerBuildError::InvalidPolicyConstraints);
    }
    let Some(http) = &constraints.http else {
        return Ok(());
    };
    if http.allowed_hosts.is_empty()
        || http.allowed_methods.is_empty()
        || http.max_requests == 0
        || http.max_request_bytes == 0
        || http.max_response_bytes == 0
        || http.allowed_hosts.len() > MAX_POLICY_SCOPE_ENTRIES
        || http.allowed_methods.len() > MAX_POLICY_SCOPE_ENTRIES
        || http
            .allowed_hosts
            .iter()
            .any(|value| !is_authority_scope(value))
        || http
            .allowed_methods
            .iter()
            .any(|value| value.len() > MAX_HTTP_METHOD_BYTES || !is_http_token(value))
    {
        return Err(BrokerBuildError::InvalidPolicyConstraints);
    }
    Ok(())
}

fn is_authority_scope(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_POLICY_SCOPE_VALUE_BYTES
        && value.trim() == value
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        && !value.contains(['/', '?', '#', '@', '*'])
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
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
    /// Policy rule count exceeded the broker ceiling.
    #[error("policy contains {count} rules; broker maximum is {maximum}")]
    TooManyPolicyRules {
        /// Actual count.
        count: usize,
        /// Maximum count.
        maximum: usize,
    },
    /// An exact principal/actor/capability rule was duplicated.
    #[error("policy duplicates principal {principal} capability {capability}")]
    DuplicatePolicyRule {
        /// Authenticated principal.
        principal: PrincipalId,
        /// Capability.
        capability: CapabilityId,
    },
    /// A policy rule named no loaded capability.
    #[error("policy capability {capability} has no loaded provider route")]
    UnknownCapability {
        /// Unknown capability.
        capability: CapabilityId,
    },
    /// Trusted provider did not match the loaded route.
    #[error("policy expected provider {expected} for {capability}, but route selects {actual}")]
    ProviderMismatch {
        /// Capability.
        capability: CapabilityId,
        /// Trusted expected provider.
        expected: ProviderId,
        /// Loaded route provider.
        actual: ProviderId,
    },
    /// Trusted effect/risk/idempotency did not match the component manifest.
    #[error("policy metadata {field} does not match loaded capability {capability}")]
    CapabilityMetadataMismatch {
        /// Capability.
        capability: CapabilityId,
        /// Mismatched field.
        field: &'static str,
    },
    /// Human/service actor did not match the exact transport principal in its rule.
    #[error("policy human or service actor does not match its authenticated principal")]
    PolicyIdentityMismatch,
    /// Rule omitted a positive bound or supplied an overbroad scope value.
    #[error("policy execution constraints are incomplete or overbroad")]
    InvalidPolicyConstraints,
    /// A rule attempted to exceed the component host's independent ceilings.
    #[error("policy execution constraints exceed component host ceilings")]
    HostConstraint {
        /// Host validation failure.
        #[source]
        source: BrokerHostError,
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
        /// Authenticated caller principal.
        principal: PrincipalId,
        /// Trusted actor.
        actor: Actor,
        /// Requested capability.
        capability: CapabilityId,
        /// Selected provider when a rule matched.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<ProviderId>,
        /// Broker principal that owns the authorization transition.
        authorized_by: PrincipalId,
        /// Broker decision identifier.
        decision_id: String,
        /// Evaluated policy revision.
        policy_revision: String,
        /// Whether execution was authorized.
        allowed: bool,
        /// Stable denial class; absent for an allow.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Digest binding the complete decision material without logging it.
        decision_digest: String,
    },
    /// Terminal provider execution metadata.
    Execution {
        /// Invocation identifier.
        invocation: InvocationId,
        /// Trace identifier.
        trace: TraceId,
        /// Authenticated caller principal.
        principal: PrincipalId,
        /// Trusted actor.
        actor: Actor,
        /// Executed capability.
        capability: CapabilityId,
        /// Trusted selected provider.
        provider: ProviderId,
        /// Broker principal that owned the authorization transition.
        authorized_by: PrincipalId,
        /// Broker decision identifier.
        decision_id: String,
        /// Evaluated policy revision.
        policy_revision: String,
        /// Trusted effect classification.
        effect: EffectKind,
        /// Trusted risk classification.
        risk: RiskLevel,
        /// Trusted retry classification.
        idempotency: Idempotency,
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
    /// Bounded in-memory storage was exhausted.
    #[error("audit log reached its {maximum}-record bound")]
    Full {
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
    event: &'a AuditEvent,
}

fn audit_record_hash(
    sequence: u64,
    previous_hash: Option<&str>,
    event: &AuditEvent,
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
    policy: ExactPolicy,
    broker_principal: PrincipalId,
    gate: AuthorizationGate,
    audit: Arc<A>,
    replay: ReplayLedger,
}

impl<A> Broker<A>
where
    A: AuditLog,
{
    /// Builds a broker around an already validated privileged component registry.
    pub fn new(
        registry: BrokerProviderRegistry,
        broker_principal: PrincipalId,
        policy_revision: String,
        rules: Vec<PolicyRule>,
        audit: Arc<A>,
        limits: BrokerLimits,
    ) -> Result<Self, BrokerBuildError> {
        if limits.max_policy_rules == 0 {
            return Err(BrokerBuildError::ZeroLimit {
                field: "max_policy_rules",
            });
        }
        if limits.max_replay_ids == 0 {
            return Err(BrokerBuildError::ZeroLimit {
                field: "max_replay_ids",
            });
        }
        let policy = ExactPolicy::new(policy_revision, rules, &registry, limits.max_policy_rules)?;
        Ok(Self {
            registry,
            policy,
            broker_principal,
            gate: AuthorizationGate::new(),
            audit,
            replay: ReplayLedger {
                maximum: limits.max_replay_ids,
                ids: Mutex::new(BTreeSet::new()),
            },
        })
    }

    /// Evaluates and, when allowed, executes one authenticated proposal exactly once.
    pub async fn invoke(
        &self,
        context: &AuthenticatedContext,
        request: InvocationRequest,
    ) -> Result<InvocationResult, BrokerError> {
        if !self.replay.reserve(&request.id).await? {
            return self.deny(context, &request, "replayed-invocation").await;
        }
        let Some(rule) = self
            .policy
            .matching_rule(context, &request.capability)
            .cloned()
        else {
            return self.deny(context, &request, "policy-denied").await;
        };
        self.execute(context, request, rule).await
    }

    async fn deny(
        &self,
        context: &AuthenticatedContext,
        request: &InvocationRequest,
        reason: &'static str,
    ) -> Result<InvocationResult, BrokerError> {
        let decision_id = format!("deny-{}", request.id);
        let decision = self.decision_reference(&decision_id);
        let material = DecisionMaterial {
            invocation: &request.id,
            trace: &request.trace,
            principal: context.principal(),
            actor: context.actor(),
            capability: &request.capability,
            provider: None,
            authorized_by: &self.broker_principal,
            policy_revision: &self.policy.revision,
            constraints: None,
            allowed: false,
            reason: Some(reason),
        };
        let digest = evidence_digest("policy-decision", &material)?;
        self.audit
            .append(AuditEvent::Decision {
                invocation: request.id.clone(),
                trace: request.trace.clone(),
                principal: context.principal().clone(),
                actor: context.actor().clone(),
                capability: request.capability.clone(),
                provider: None,
                authorized_by: self.broker_principal.clone(),
                decision_id: decision_id.clone(),
                policy_revision: self.policy.revision.clone(),
                allowed: false,
                reason: Some(reason.to_owned()),
                decision_digest: digest.clone(),
            })
            .await
            .map_err(|source| BrokerError::DecisionAudit { source })?;
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

    async fn execute(
        &self,
        context: &AuthenticatedContext,
        request: InvocationRequest,
        rule: PolicyRule,
    ) -> Result<InvocationResult, BrokerError> {
        let decision_id = format!("allow-{}", request.id);
        let decision = self.decision_reference(&decision_id);
        let invocation_id = request.id.clone();
        let trace = request.trace.clone();
        let capability = request.capability.clone();
        let proposal = ProposedInvocation::new(
            request.id,
            request.capability,
            context.actor().clone(),
            request.trace,
            request.input,
        );
        let authorized = self
            .gate
            .authorize(
                proposal,
                rule.provider.clone(),
                decision_id.clone(),
                self.broker_principal.clone(),
                self.policy.revision.clone(),
                rule.constraints.clone(),
            )
            .map_err(|source| BrokerError::Authorization { source })?;
        let decision_digest = evidence_digest("authorized-invocation", &authorized)?;
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
                principal: context.principal().clone(),
                actor: context.actor().clone(),
                capability: capability.clone(),
                provider: Some(rule.provider.clone()),
                authorized_by: self.broker_principal.clone(),
                decision_id: decision_id.clone(),
                policy_revision: self.policy.revision.clone(),
                allowed: true,
                reason: None,
                decision_digest,
            })
            .await
            .map_err(|source| BrokerError::DecisionAudit { source })?;

        let started = Instant::now();
        let execution = self.registry.invoke(authorized).await;
        let duration_ms = duration_millis(started.elapsed());
        let (result, audit_event) = match execution {
            Ok(output) => {
                let output_digest = evidence_digest("provider-response", &output.output)?;
                let mut evidence = vec![policy_evidence];
                evidence.push(Evidence {
                    kind: "provider-response".to_owned(),
                    digest: output_digest.clone(),
                    media_type: PROVIDER_EVIDENCE_MEDIA_TYPE.to_owned(),
                    uri: None,
                });
                if !output.http_calls.is_empty() {
                    evidence.push(Evidence {
                        kind: "http-calls".to_owned(),
                        digest: evidence_digest("http-calls", &output.http_calls)?,
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
                    &self.policy.revision,
                    &self.broker_principal,
                    &rule,
                    InvocationOutcome::Succeeded,
                    duration_ms,
                    None,
                    Some(output_digest),
                    output.http_calls,
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
            Err(error) => {
                let error = public_host_error(&error).to_owned();
                let event = execution_event(
                    context,
                    &invocation_id,
                    &trace,
                    &capability,
                    &decision_id,
                    &self.policy.revision,
                    &self.broker_principal,
                    &rule,
                    InvocationOutcome::Failed,
                    duration_ms,
                    Some(error.clone()),
                    None,
                    Vec::new(),
                );
                (
                    InvocationResult {
                        invocation: invocation_id.clone(),
                        decision,
                        outcome: InvocationOutcome::Failed,
                        output: None,
                        error: Some(error),
                        evidence: vec![policy_evidence],
                    },
                    event,
                )
            }
        };

        self.audit
            .append(audit_event)
            .await
            .map_err(|source| BrokerError::OutcomeAudit {
                invocation: invocation_id,
                source,
            })?;
        Ok(result)
    }

    fn decision_reference(&self, decision_id: &str) -> DecisionReference {
        DecisionReference {
            decision_id: decision_id.to_owned(),
            authorized_by: self.broker_principal.clone(),
            policy_revision: self.policy.revision.clone(),
        }
    }
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
    authorized_by: &PrincipalId,
    rule: &PolicyRule,
    outcome: InvocationOutcome,
    duration_ms: u64,
    error: Option<String>,
    output_digest: Option<String>,
    http_calls: Vec<HttpCallEvidence>,
) -> AuditEvent {
    AuditEvent::Execution {
        invocation: invocation.clone(),
        trace: trace.clone(),
        principal: context.principal().clone(),
        actor: context.actor().clone(),
        capability: capability.clone(),
        provider: rule.provider.clone(),
        authorized_by: authorized_by.clone(),
        decision_id: decision_id.to_owned(),
        policy_revision: policy_revision.to_owned(),
        effect: rule.effect,
        risk: rule.risk,
        idempotency: rule.idempotency,
        outcome,
        duration_ms,
        error,
        output_digest,
        http_calls,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecisionMaterial<'a> {
    invocation: &'a InvocationId,
    trace: &'a TraceId,
    principal: &'a PrincipalId,
    actor: &'a Actor,
    capability: &'a CapabilityId,
    provider: Option<&'a ProviderId>,
    authorized_by: &'a PrincipalId,
    policy_revision: &'a str,
    constraints: Option<&'a ExecutionConstraints>,
    allowed: bool,
    reason: Option<&'a str>,
}

fn evidence_digest(label: &str, value: &impl Serialize) -> Result<String, BrokerError> {
    let bytes = serde_json::to_vec(value).map_err(|source| BrokerError::Evidence { source })?;
    let mut material = Vec::with_capacity(label.len() + 1 + bytes.len());
    material.extend_from_slice(label.as_bytes());
    material.push(0);
    material.extend_from_slice(&bytes);
    Ok(domain_digest(EVIDENCE_HASH_DOMAIN, &material))
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
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
        | BrokerHostError::HttpConfiguration { .. } => "authorization-constraint",
        BrokerHostError::UnknownCapability { .. }
        | BrokerHostError::ProviderDoesNotImplement { .. } => "capability-unavailable",
        BrokerHostError::AuthorizedProviderMismatch { .. } => "authorized-provider-mismatch",
        BrokerHostError::InputNotObject { .. }
        | BrokerHostError::SerializeInput { .. }
        | BrokerHostError::InputTooLarge { .. } => "invalid-input",
        BrokerHostError::OutputTooLarge { .. } | BrokerHostError::InvalidOutput { .. } => {
            "invalid-provider-output"
        }
        BrokerHostError::Timeout { .. } => "provider-timeout",
        BrokerHostError::HostCallRejected { .. } => "host-call-rejected",
        BrokerHostError::Invoke { .. } => "provider-trap",
        BrokerHostError::ProviderFailure { .. } => "provider-failure",
        BrokerHostError::NoProviders
        | BrokerHostError::InvalidLimit { .. }
        | BrokerHostError::Engine { .. }
        | BrokerHostError::Store { .. }
        | BrokerHostError::Linker { .. }
        | BrokerHostError::Compile { .. }
        | BrokerHostError::Instantiate { .. }
        | BrokerHostError::DescribeUsedHostImport { .. }
        | BrokerHostError::Describe { .. }
        | BrokerHostError::InvalidManifest { .. }
        | BrokerHostError::Manifest { .. }
        | BrokerHostError::DuplicateProvider { .. }
        | BrokerHostError::DuplicateCapability { .. } => "broker-host-failure",
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
    /// A validated policy rule could not create authorization state.
    #[error("broker could not create constrained authorization")]
    Authorization {
        /// Typestate transition failure.
        #[source]
        source: AuthorizationError,
    },
    /// Public evidence could not be hashed.
    #[error("broker could not serialize bounded evidence")]
    Evidence {
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

#[cfg(test)]
mod tests;
