//! Capability metadata and invocation-state types for Dekopon.
//!
//! The central API distinction is between [`ProposedInvocation`] and
//! [`AuthorizedInvocation`]. A model-facing tool call can create only the former. The
//! latter has private fields and can be produced only by the broker-oriented transition
//! in this crate.
//!
//! ```compile_fail
//! use dekopon_capability::{AuthorizedInvocation, ExecutionConstraints, ProposedInvocation};
//!
//! fn fabricate(proposal: ProposedInvocation, constraints: ExecutionConstraints) {
//!     // Ordinary callers cannot use a struct literal to cross the authority boundary.
//!     let _forged = AuthorizedInvocation {
//!         proposal,
//!         receipt: todo!(),
//!         constraints,
//!     };
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

/// Broker-enforced execution limits attached to an authorization.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ExecutionConstraints {
    /// Maximum wall-clock duration allowed for provider execution.
    pub timeout_ms: u64,
    /// Maximum serialized output size.
    pub max_output_bytes: u64,
    /// Optional host allow-list. An empty list permits no network destinations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
}

impl Default for ExecutionConstraints {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_output_bytes: 1_048_576,
            allowed_hosts: Vec::new(),
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
/// Private fields prevent accidental conversion from an untrusted proposal. This type is
/// serializable for evidence and future broker responses, but intentionally is not
/// deserializable in `0.1.0`.
#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizedInvocation {
    proposal: ProposedInvocation,
    receipt: AuthorizationReceipt,
    constraints: ExecutionConstraints,
}

impl AuthorizedInvocation {
    /// Returns the original untrusted proposal.
    #[must_use]
    pub fn proposal(&self) -> &ProposedInvocation {
        &self.proposal
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
}

/// Broker-only authority transition.
///
/// `AuthorizationGate` is public so its role is visible in API documentation, but its
/// constructor remains crate-private until a real broker process and authenticated
/// transport exist. A future broker adapter will own gate construction; normal callers
/// can never obtain one from model-provided data.
pub mod broker {
    use dekopon_core::PrincipalId;

    use super::{
        AuthorizationError, AuthorizationReceipt, AuthorizedInvocation, ExecutionConstraints,
        ProposedInvocation,
    };

    /// Handle that owns the proposal-to-authorization state transition.
    #[derive(Debug)]
    pub struct AuthorizationGate {
        _private: (),
    }

    impl AuthorizationGate {
        #[allow(
            dead_code,
            reason = "construction is reserved for the future authenticated broker adapter"
        )]
        pub(crate) const fn new() -> Self {
            Self { _private: () }
        }

        /// Converts a proposal only after a broker has made an authorization decision.
        pub fn authorize(
            &self,
            proposal: ProposedInvocation,
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

            Ok(AuthorizedInvocation {
                proposal,
                receipt: AuthorizationReceipt {
                    decision_id,
                    authorized_by,
                    policy_revision,
                },
                constraints,
            })
        }
    }

    #[cfg(test)]
    pub(crate) const fn test_gate() -> AuthorizationGate {
        AuthorizationGate::new()
    }
}

#[cfg(test)]
mod tests {
    use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};
    use serde_json::json;

    use super::{AuthorizationError, ExecutionConstraints, ProposedInvocation, broker};

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
        let authorized = broker::test_gate()
            .authorize(
                proposal(),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                ExecutionConstraints::default(),
            )
            .expect("valid broker decision");

        assert_eq!(authorized.proposal().id.as_str(), "invoke-1");
        assert_eq!(authorized.receipt().decision_id(), "decision-1");
        assert_eq!(authorized.constraints().timeout_ms, 30_000);
    }

    #[test]
    fn broker_gate_rejects_unbounded_execution() {
        let constraints = ExecutionConstraints {
            timeout_ms: 0,
            ..ExecutionConstraints::default()
        };
        let error = broker::test_gate()
            .authorize(
                proposal(),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                constraints,
            )
            .expect_err("zero timeout must fail");

        assert_eq!(error, AuthorizationError::ZeroTimeout);
    }

    #[test]
    fn authorized_invocation_is_evidence_serializable() {
        let authorized = broker::test_gate()
            .authorize(
                proposal(),
                "decision-1".to_owned(),
                "broker".parse::<PrincipalId>().expect("valid fixture"),
                "policy-1".to_owned(),
                ExecutionConstraints::default(),
            )
            .expect("valid broker decision");
        let value = serde_json::to_value(authorized).expect("authorization serializes");

        assert_eq!(value["receipt"]["decisionId"], "decision-1");
    }
}
