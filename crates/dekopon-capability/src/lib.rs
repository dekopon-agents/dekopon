//! Capability metadata and invocation-state types for Dekopon.
//!
//! The central API distinction is between [`ProposedInvocation`] and
//! [`AuthorizedInvocation`]. A model-facing tool call can create only the former. The
//! latter has private fields and can be produced only by the broker-oriented transition
//! in this crate. The broker-owned execution boundary creates and consumes this state;
//! its serialized representation is inert audit/evidence data, not transferable authority.
//!
//! ```compile_fail
//! use dekopon_capability::{AuthorizedInvocation, ExecutionConstraints, ProposedInvocation};
//!
//! fn fabricate(proposal: ProposedInvocation, constraints: ExecutionConstraints) {
//!     // Ordinary callers cannot use a struct literal to cross the authority boundary.
//!     let _forged = AuthorizedInvocation {
//!         proposal,
//!         provider: todo!(),
//!         receipt: todo!(),
//!         constraints,
//!     };
//! }
//! ```
//!
//! Serialized authorization state cannot be decoded into executable authority:
//!
//! ```compile_fail
//! use dekopon_capability::AuthorizedInvocation;
//! use serde::de::DeserializeOwned;
//!
//! fn require_deserializable<T: DeserializeOwned>() {}
//!
//! fn main() {
//!     require_deserializable::<AuthorizedInvocation>();
//! }
//! ```
//!
//! Authorization is also intentionally single-use at the type boundary:
//!
//! ```compile_fail
//! use dekopon_capability::AuthorizedInvocation;
//!
//! fn require_clone<T: Clone>() {}
//!
//! fn main() {
//!     require_clone::<AuthorizedInvocation>();
//! }
//! ```
//!
//! Rust visibility is defense in depth. It is not a substitute for process isolation,
//! authenticated broker messages, authorization policy, or credential separation.

#![forbid(unsafe_code)]

use std::fmt;

use dekopon_core::{
    Actor, CapabilityId, InvocationId, PrincipalId, ProviderId, RiskLevel, TraceId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Whether invoking a capability can cause an externally observable effect.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum EffectKind {
    /// Reads data without intentionally mutating local or external state.
    ReadOnly,
    /// Mutates only broker-controlled local state.
    LocalWrite,
    /// Mutates a system outside the Dekopon trust boundary.
    ExternalWrite,
}

impl fmt::Display for EffectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::ReadOnly => "read-only",
            Self::LocalWrite => "local-write",
            Self::ExternalWrite => "external-write",
        };
        formatter.write_str(value)
    }
}

/// Declared retry behavior for an invocation.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Idempotency {
    /// Repeating an identical invocation has no additional effect.
    Idempotent,
    /// Repetition is safe only when a provider-enforced key or condition is present.
    Conditional,
    /// Repeating the invocation can create an additional effect.
    NonIdempotent,
}

impl fmt::Display for Idempotency {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Idempotent => "idempotent",
            Self::Conditional => "conditional",
            Self::NonIdempotent => "non-idempotent",
        };
        formatter.write_str(value)
    }
}

/// A provider permission needed to execute a capability.
#[derive(
    Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Permission {
    /// Provider-specific operation, such as `pull_requests:read`.
    pub operation: String,
    /// Optional provider-specific resource scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// Human- and policy-readable metadata for a capability.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    /// Stable capability identifier.
    pub id: CapabilityId,
    /// Provider that implements this capability.
    pub provider: ProviderId,
    /// Concise operator-facing description.
    pub description: String,
    /// External-effect class.
    pub effect: EffectKind,
    /// Coarse policy risk input.
    pub risk: RiskLevel,
    /// Retry behavior.
    pub idempotency: Idempotency,
    /// Least-privilege provider permissions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<Permission>,
}

/// An invocation proposed by a model, agent, or human.
///
/// This type carries intent but no authority to execute an external effect.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProposedInvocation {
    /// Unique invocation identifier.
    pub id: InvocationId,
    /// Requested capability.
    pub capability: CapabilityId,
    /// Authenticated actor attributed by the trusted message envelope.
    pub actor: Actor,
    /// End-to-end trace identifier.
    pub trace: TraceId,
    /// Capability-specific, untrusted arguments.
    pub input: Value,
}

impl ProposedInvocation {
    /// Constructs an unprivileged invocation proposal.
    #[must_use]
    pub fn new(
        id: InvocationId,
        capability: CapabilityId,
        actor: Actor,
        trace: TraceId,
        input: Value,
    ) -> Self {
        Self {
            id,
            capability,
            actor,
            trace,
            input,
        }
    }
}

/// Broker-enforced buffered HTTP limits attached to one authorization.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpConstraints {
    /// Exact DNS names or IP authorities the invocation may contact.
    pub allowed_hosts: Vec<String>,
    /// Exact case-sensitive HTTP method tokens the invocation may use.
    pub allowed_methods: Vec<String>,
    /// Maximum number of HTTP requests in this provider invocation.
    pub max_requests: u32,
    /// Maximum encoded request bytes, including headers and body.
    pub max_request_bytes: u64,
    /// Maximum encoded response bytes, including headers and body.
    pub max_response_bytes: u64,
    /// Whether explicitly allowed loopback hosts may use plaintext HTTP.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_plaintext_loopback: bool,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// Exact component storage interface selected for one capability.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum StorageInterface {
    /// Curated invocation-transactional JSONL operations.
    Jsonl,
    /// Engine-neutral positional durable-file operations.
    DurableFiles,
}

/// Storage mutation authority selected for one capability.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum StorageAccess {
    /// Reads only; every mutating host call is terminally denied.
    ReadOnly,
    /// Reads and invocation-transactional writes.
    ReadWrite,
}

/// Broker-owned logical namespace class.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum StorageNamespace {
    /// Owner-private chat memory, scoped from trusted chat attestation.
    Chat,
}

/// Exact namespace-bound storage authority attached to one capability.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StorageConstraints {
    /// The only storage interface this invocation may call.
    pub interface: StorageInterface,
    /// Whether mutation is permitted.
    pub access: StorageAccess,
    /// Broker-owned namespace class; guests never supply namespace material.
    pub namespace: StorageNamespace,
}

/// Broker-enforced execution limits attached to an authorization.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExecutionConstraints {
    /// Maximum wall-clock duration allowed for provider execution.
    pub timeout_ms: u64,
    /// Maximum serialized provider output size.
    pub max_output_bytes: u64,
    /// Optional buffered HTTP grant. Its absence permits no HTTP host calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpConstraints>,
    /// Optional exact storage grant. HTTP and storage cannot coexist in v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageConstraints>,
}

impl Default for ExecutionConstraints {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_output_bytes: 1_048_576,
            http: None,
            storage: None,
        }
    }
}

/// Evidence that an authorization decision occurred.
///
/// Receipts are emitted by the broker transition and cannot be assembled with a public
/// struct literal.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationReceipt {
    decision_id: String,
    authorized_by: PrincipalId,
    policy_revision: String,
}

impl AuthorizationReceipt {
    /// Stable broker decision identifier.
    #[must_use]
    pub fn decision_id(&self) -> &str {
        &self.decision_id
    }

    /// Trusted principal that authorized the transition.
    #[must_use]
    pub fn authorized_by(&self) -> &PrincipalId {
        &self.authorized_by
    }

    /// Policy revision evaluated by the broker.
    #[must_use]
    pub fn policy_revision(&self) -> &str {
        &self.policy_revision
    }
}

/// An invocation for which a broker has explicitly granted authority.
///
/// Private fields prevent accidental conversion from an untrusted proposal. The selected provider
/// is bound alongside the proposal and constraints. The value is not cloneable or deserializable:
/// the broker-owned execution boundary creates and consumes it once. It is serializable as
/// inert data for broker-owned audit and evidence recording, but its serialized form is not a
/// transferable bearer grant and intentionally cannot be deserialized.
#[derive(Debug, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizedInvocation {
    proposal: ProposedInvocation,
    provider: ProviderId,
    receipt: AuthorizationReceipt,
    constraints: ExecutionConstraints,
}

impl AuthorizedInvocation {
    /// Returns the original untrusted proposal.
    #[must_use]
    pub fn proposal(&self) -> &ProposedInvocation {
        &self.proposal
    }

    /// Returns the exact provider selected by trusted policy and routing.
    #[must_use]
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    /// Returns the broker authorization receipt.
    #[must_use]
    pub fn receipt(&self) -> &AuthorizationReceipt {
        &self.receipt
    }

    /// Returns constraints the provider host must enforce.
    #[must_use]
    pub fn constraints(&self) -> &ExecutionConstraints {
        &self.constraints
    }
}

/// Public, inert linkage to the broker decision behind an invocation result.
///
/// Unlike [`AuthorizationReceipt`], this value is deserializable because it carries no execution
/// authority and cannot be converted into an [`AuthorizedInvocation`].
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DecisionReference {
    /// Stable broker decision identifier.
    pub decision_id: String,
    /// Broker principal that owned the authority transition.
    pub authorized_by: PrincipalId,
    /// Exact evaluated policy revision.
    pub policy_revision: String,
}

/// A piece of evidence produced during authorization or execution.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Evidence {
    /// Evidence category, such as `provider-response` or `policy-decision`.
    pub kind: String,
    /// Digest of canonical evidence bytes.
    pub digest: String,
    /// Media type of the referenced evidence.
    pub media_type: String,
    /// Optional durable reference; secrets must never be embedded here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// Terminal state of an attempted invocation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum InvocationOutcome {
    /// Provider execution completed successfully.
    Succeeded,
    /// Authorization denied execution.
    Denied,
    /// Provider execution began but failed.
    Failed,
}

/// Serializable result and evidence for an invocation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InvocationResult {
    /// Invocation identifier.
    pub invocation: InvocationId,
    /// Inert linkage to the broker decision and policy revision.
    pub decision: DecisionReference,
    /// Terminal state.
    pub outcome: InvocationOutcome,
    /// Provider output when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// Concise failure reason when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Evidence records collected during the invocation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

/// Failure to apply the broker authorization transition.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AuthorizationError {
    /// The broker supplied an empty decision identifier.
    #[error("authorization decision identifier must not be empty")]
    EmptyDecisionId,
    /// The broker supplied an empty policy revision.
    #[error("authorization policy revision must not be empty")]
    EmptyPolicyRevision,
    /// Provider execution was authorized without a positive timeout.
    #[error("authorization timeout must be greater than zero")]
    ZeroTimeout,
    /// Provider execution was authorized without a positive output bound.
    #[error("authorization output limit must be greater than zero")]
    ZeroOutputLimit,
    /// HTTP was granted without an exact destination.
    #[error("HTTP authorization requires at least one allowed host")]
    NoHttpHosts,
    /// HTTP was granted without an exact method.
    #[error("HTTP authorization requires at least one allowed method")]
    NoHttpMethods,
    /// HTTP was granted without positive call and byte limits.
    #[error("HTTP authorization limits must be greater than zero")]
    ZeroHttpLimit,
    /// HTTP and storage authority were combined in one v1 capability.
    #[error("HTTP and storage authority cannot coexist in one capability")]
    MixedHttpAndStorage,
}

/// Broker-only authority transition.
///
/// `AuthorizationGate` is constructed only by trusted broker code after its deployment boundary
/// has authenticated a caller and evaluated policy. Public construction lets a separately
/// packaged broker adapter own the transition; it does not authenticate anything by itself and
/// must never be driven directly from model-provided data.
pub mod broker {
    use dekopon_core::{PrincipalId, ProviderId};

    use super::{
        AuthorizationError, AuthorizationReceipt, AuthorizedInvocation, ExecutionConstraints,
        ProposedInvocation,
    };

    /// Handle that owns the proposal-to-authorization state transition.
    #[derive(Debug)]
    pub struct AuthorizationGate {
        _private: (),
    }

    #[allow(
        clippy::new_without_default,
        reason = "authority transitions should require an explicit broker-owned constructor"
    )]
    impl AuthorizationGate {
        /// Creates a transition handle for trusted broker code.
        ///
        /// Construction itself conveys no authenticated identity or policy decision. Keep this
        /// handle inside the privileged broker process and call [`Self::authorize`] only after
        /// those checks have completed.
        #[must_use]
        pub const fn new() -> Self {
            Self { _private: () }
        }

        /// Converts a proposal only after a broker has made an authorization decision.
        pub fn authorize(
            &self,
            proposal: ProposedInvocation,
            provider: ProviderId,
            decision_id: String,
            authorized_by: PrincipalId,
            policy_revision: String,
            constraints: ExecutionConstraints,
        ) -> Result<AuthorizedInvocation, AuthorizationError> {
            if decision_id.trim().is_empty() {
                return Err(AuthorizationError::EmptyDecisionId);
            }
            if policy_revision.trim().is_empty() {
                return Err(AuthorizationError::EmptyPolicyRevision);
            }
            if constraints.timeout_ms == 0 {
                return Err(AuthorizationError::ZeroTimeout);
            }
            if constraints.max_output_bytes == 0 {
                return Err(AuthorizationError::ZeroOutputLimit);
            }
            if constraints.http.is_some() && constraints.storage.is_some() {
                return Err(AuthorizationError::MixedHttpAndStorage);
            }
            if let Some(http) = &constraints.http {
                if http.allowed_hosts.is_empty() {
                    return Err(AuthorizationError::NoHttpHosts);
                }
                if http.allowed_methods.is_empty() {
                    return Err(AuthorizationError::NoHttpMethods);
                }
                if http.max_requests == 0
                    || http.max_request_bytes == 0
                    || http.max_response_bytes == 0
                {
                    return Err(AuthorizationError::ZeroHttpLimit);
                }
            }

            Ok(AuthorizedInvocation {
                proposal,
                provider,
                receipt: AuthorizationReceipt {
                    decision_id,
                    authorized_by,
                    policy_revision,
                },
                constraints,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};
    use serde_json::json;

    use super::{
        AuthorizationError, ExecutionConstraints, HttpConstraints, ProposedInvocation, broker,
    };

    fn proposal() -> ProposedInvocation {
        ProposedInvocation::new(
            "invoke-1".parse::<InvocationId>().expect("valid fixture"),
            "github.pull-request.comment"
                .parse::<CapabilityId>()
                .expect("valid fixture"),
            Actor::Agent {
                agent: "reviewer".parse::<AgentId>().expect("valid fixture"),
            },
            "trace-1".parse::<TraceId>().expect("valid fixture"),
            json!({"body": "Looks good"}),
        )
    }

    #[test]
    fn broker_gate_performs_explicit_authority_transition() {
        let authorized = broker::AuthorizationGate::new()
            .authorize(
                proposal(),
                "github".parse().expect("valid provider fixture"),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                ExecutionConstraints::default(),
            )
            .expect("valid broker decision");

        assert_eq!(authorized.proposal().id.as_str(), "invoke-1");
        assert_eq!(authorized.provider().as_str(), "github");
        assert_eq!(authorized.receipt().decision_id(), "decision-1");
        assert_eq!(authorized.constraints().timeout_ms, 30_000);
    }

    #[test]
    fn broker_gate_rejects_unbounded_execution() {
        let constraints = ExecutionConstraints {
            timeout_ms: 0,
            ..ExecutionConstraints::default()
        };
        let error = broker::AuthorizationGate::new()
            .authorize(
                proposal(),
                "github".parse().expect("valid provider fixture"),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                constraints,
            )
            .expect_err("zero timeout must fail");

        assert_eq!(error, AuthorizationError::ZeroTimeout);
    }

    #[test]
    fn broker_gate_rejects_incomplete_http_authority() {
        let valid = HttpConstraints {
            allowed_hosts: vec!["api.example.test".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 1024,
            max_response_bytes: 1024,
            allow_plaintext_loopback: false,
        };
        let cases = [
            (
                HttpConstraints {
                    allowed_hosts: Vec::new(),
                    ..valid.clone()
                },
                AuthorizationError::NoHttpHosts,
            ),
            (
                HttpConstraints {
                    allowed_methods: Vec::new(),
                    ..valid.clone()
                },
                AuthorizationError::NoHttpMethods,
            ),
            (
                HttpConstraints {
                    max_requests: 0,
                    ..valid
                },
                AuthorizationError::ZeroHttpLimit,
            ),
        ];

        for (http, expected) in cases {
            let error = broker::AuthorizationGate::new()
                .authorize(
                    proposal(),
                    "github".parse().expect("valid provider fixture"),
                    "decision-1".to_owned(),
                    "broker".parse::<PrincipalId>().expect("valid fixture"),
                    "policy-1".to_owned(),
                    ExecutionConstraints {
                        http: Some(http),
                        ..ExecutionConstraints::default()
                    },
                )
                .expect_err("incomplete HTTP authority must fail");
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn authorized_invocation_serialization_preserves_linkage() {
        let constraints = ExecutionConstraints {
            http: Some(HttpConstraints {
                allowed_hosts: vec!["api.github.com".to_owned()],
                allowed_methods: vec!["POST".to_owned()],
                max_requests: 1,
                max_request_bytes: 65_536,
                max_response_bytes: 1_048_576,
                allow_plaintext_loopback: false,
            }),
            ..ExecutionConstraints::default()
        };
        let authorized = broker::AuthorizationGate::new()
            .authorize(
                proposal(),
                "github".parse().expect("valid provider fixture"),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                constraints,
            )
            .expect("valid broker decision");
        let value = serde_json::to_value(authorized).expect("authorization serializes");

        assert_eq!(
            value,
            json!({
                "proposal": {
                    "id": "invoke-1",
                    "capability": "github.pull-request.comment",
                    "actor": {"type": "agent", "agent": "reviewer"},
                    "trace": "trace-1",
                    "input": {"body": "Looks good"}
                },
                "provider": "github",
                "receipt": {
                    "decisionId": "decision-1",
                    "authorizedBy": "broker",
                    "policyRevision": "policy-1"
                },
                "constraints": {
                    "timeoutMs": 30_000,
                    "maxOutputBytes": 1_048_576,
                    "http": {
                        "allowedHosts": ["api.github.com"],
                        "allowedMethods": ["POST"],
                        "maxRequests": 1,
                        "maxRequestBytes": 65_536,
                        "maxResponseBytes": 1_048_576
                    }
                }
            })
        );
    }
}
