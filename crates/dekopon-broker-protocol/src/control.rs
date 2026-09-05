//! Strict core-session control proposals and single-response client verification.

use std::collections::BTreeSet;

use dekopon_core::{
    AgentId, ConfiguredModelId, Effort, GenerationId, InvocationId, JobId, ModelSelection,
    RequestId, SessionId, SurfaceEpoch, TraceId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{Attestation, TraceParent};

/// Independent startup ceiling on configured model targets.
pub const MAX_CONTROL_TARGETS: usize = 16;
/// The range a proposal's `sequence` must fall in, and the ceiling a control client spends against.
///
/// Two things enforce it, and neither is a host-wide count of transitions. [`ControlClient`] refuses
/// to transmit once a job's own monotonic sequence reaches it, and the broker refuses any proposal
/// whose `sequence` falls outside `1..=MAX_CONTROL_ATTEMPTS` ([`ControlProposal::is_well_formed`]).
/// The broker keeps no per-job attempt counter: distinct proposal identifiers each carrying
/// `sequence: 1` are each well formed, and the replay reservation — not this constant — is what
/// stops the same identifier twice. The budget is therefore the host's to spend honestly, which is
/// the scope `docs/security-model.md` states for every client-side bound.
pub const MAX_CONTROL_ATTEMPTS: u32 = 4;

/// Broker-owned allowlist entry; no endpoints, credentials, or client options live here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ControlTarget {
    pub model: ConfiguredModelId,
    pub efforts: Vec<Effort>,
}

/// All conflicts in one authored control-target list.
#[derive(Debug, Error)]
#[error("invalid controlTargets: {}", .0.join("; "))]
pub struct ControlTargetsError(pub Vec<String>);

/// Checks every target conflict before startup; an empty list disables controls.
pub fn validate_control_targets(targets: &[ControlTarget]) -> Result<(), ControlTargetsError> {
    let mut conflicts = Vec::new();
    let mut models = BTreeSet::new();
    if targets.len() > MAX_CONTROL_TARGETS {
        conflicts.push(format!("more than {MAX_CONTROL_TARGETS} targets"));
    }
    for target in targets {
        if !models.insert(&target.model) {
            conflicts.push(format!("duplicate model {}", target.model));
        }
        if target.efforts.is_empty() {
            conflicts.push(format!("model {} has no efforts", target.model));
        }
        let mut efforts = BTreeSet::new();
        for effort in &target.efforts {
            if !efforts.insert(effort) {
                conflicts.push(format!("model {} repeats effort {effort}", target.model));
            }
        }
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(ControlTargetsError(conflicts))
    }
}

/// Opaque coordinates supplied by authenticated host routing, never by tool arguments.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ControlScope {
    pub agent: AgentId,
    pub job: JobId,
    pub session: SessionId,
    pub request: RequestId,
    pub generation: GenerationId,
}

/// Complete untrusted transition intent. Both selection dimensions are always explicit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ControlProposal {
    /// Shares the global durable invocation replay domain, including refused proposals.
    pub id: InvocationId,
    pub scope: ControlScope,
    /// One-based attempt within the job. The host preserves it across switches and resume.
    pub sequence: u32,
    /// The active job's startup epoch. A changed epoch halts rather than reauthorizing old work.
    pub surface_epoch: SurfaceEpoch,
    pub from: ModelSelection,
    pub to: ModelSelection,
    pub trace: TraceId,
    pub trace_parent: Option<TraceParent>,
}

impl ControlProposal {
    /// Structural validation only. A well-formed proposal may still be denied by the broker.
    pub fn is_well_formed(&self, attestation: Option<&Attestation>) -> bool {
        (1..=MAX_CONTROL_ATTEMPTS).contains(&self.sequence)
            && attestation.is_none_or(|claim| {
                claim.is_well_formed() && claim.binds(&self.id) && claim.agent == self.scope.agent
            })
    }
}

/// Admission is not application, execution evidence, or permission for a later request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlOutcome {
    Admitted,
    #[serde(rename = "control-denied")]
    Denied,
}

/// Public decision linkage. This wire value is deliberately NOT a usable admission type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ControlDecision {
    pub proposal: ControlProposal,
    pub attestation: Option<Attestation>,
    pub surface_epoch: SurfaceEpoch,
    /// SHA-256 commitment to authenticated context, proposal, policy and decision.
    pub decision_ref: String,
    pub outcome: ControlOutcome,
}

/// A response consumed from one live, server-UID-verified exchange. Not cloneable or deserializable.
///
/// There is no constructor from provider JSON, checkpoint data, or a public `ControlDecision`.
/// No broker operation accepts this value as authority. The host consumes it at its safe boundary.
///
/// ```compile_fail
/// let forged: dekopon_broker_protocol::VerifiedControlDecision =
///     serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug)]
pub struct VerifiedControlDecision(Box<ControlDecision>);

impl VerifiedControlDecision {
    /// Complete immutable intent checked against the sole pending request.
    pub fn proposal(&self) -> &ControlProposal {
        &self.0.proposal
    }
    /// Audit linkage only, never a bearer token.
    pub fn decision_ref(&self) -> &str {
        &self.0.decision_ref
    }
    /// Consume this response once. Admission alone does not claim the host applied a transition.
    pub fn consume(self) -> ControlOutcome {
        self.0.outcome
    }
}

/// Request-scoped live client. It holds no policy, provider credentials or reusable authority.
#[cfg(unix)]
#[derive(Debug)]
pub struct ControlClient {
    client: crate::BrokerClient,
    scope: ControlScope,
    surface_epoch: SurfaceEpoch,
    attestation: Option<Attestation>,
    attempts: u32,
    fenced: bool,
}

#[cfg(unix)]
impl crate::BrokerClient {
    /// Binds control requests to the host's active job and freshly obtained epoch.
    ///
    /// Restores must pass the already spent attempts, never zero them. Direct/replay runners must
    /// not install this authorizer merely because they have a provider broker leg.
    pub fn control_client(
        &self,
        scope: ControlScope,
        surface_epoch: SurfaceEpoch,
        attestation: Option<Attestation>,
        spent_attempts: u32,
    ) -> Result<ControlClient, crate::ClientError> {
        if spent_attempts > MAX_CONTROL_ATTEMPTS
            || attestation.as_ref().is_some_and(|claim| {
                claim.agent != scope.agent || !claim.is_well_formed() || claim.invocation.is_some()
            })
        {
            return Err(crate::ClientError::InvalidControl);
        }
        Ok(ControlClient {
            client: self.clone(),
            scope,
            surface_epoch,
            attestation,
            attempts: spent_attempts,
            fenced: false,
        })
    }
}

#[cfg(unix)]
impl ControlClient {
    /// Host-bound coordinates; never supplied by model arguments.
    pub fn scope(&self) -> &ControlScope {
        &self.scope
    }
    /// Active broker-startup fence from fresh admission.
    pub fn surface_epoch(&self) -> &SurfaceEpoch {
        &self.surface_epoch
    }

    /// Makes exactly one fresh authenticated exchange. No timeout, late reply, or denial is retried.
    /// `sequence` is the host's monotonic spent-attempt count; gaps are locally refused attempts.
    /// Repeated/backward sequences and values above the hard job ceiling fail before transmission.
    ///
    /// Infrastructure failure, cancellation of this future, epoch change, or response substitution
    /// permanently fences this client. Certain denials retain the old model and spend one attempt.
    pub async fn authorize(
        &mut self,
        sequence: u32,
        id: InvocationId,
        from: ModelSelection,
        to: ModelSelection,
        trace: TraceId,
        trace_parent: Option<TraceParent>,
    ) -> Result<VerifiedControlDecision, crate::ClientError> {
        use crate::{BrokerRequest, BrokerResponse, ClientError, ProtocolVersion, RequestEnvelope};
        if self.fenced {
            return Err(ClientError::ControlFenced);
        }
        if sequence > MAX_CONTROL_ATTEMPTS || self.attempts >= MAX_CONTROL_ATTEMPTS {
            return Err(ClientError::ControlAttempts);
        }
        if sequence <= self.attempts {
            return Err(ClientError::InvalidControl);
        }
        // Gaps are local refusals charged by the harness, not permission to reset a budget.
        self.attempts = sequence;
        // Fenced before await, so dropping a pending response cannot leave a reusable client.
        self.fenced = true;
        let proposal = ControlProposal {
            id,
            scope: self.scope.clone(),
            sequence: self.attempts,
            surface_epoch: self.surface_epoch.clone(),
            from,
            to,
            trace,
            trace_parent,
        };
        let attestation = self
            .attestation
            .as_ref()
            .map(|claim| claim.bound_to(proposal.id.clone()));
        let response = self
            .client
            .exchange(RequestEnvelope {
                api_version: ProtocolVersion::V1Alpha3,
                request: BrokerRequest::AuthorizeControl {
                    proposal: proposal.clone(),
                    attestation: attestation.clone(),
                },
            })
            .await?;
        let decision = match response {
            BrokerResponse::ControlDecision { decision } => decision,
            BrokerResponse::Error { code, message } => {
                return Err(ClientError::Remote { code, message });
            }
            _ => return Err(ClientError::UnexpectedResponse),
        };
        if decision.surface_epoch != self.surface_epoch {
            return Err(ClientError::SurfaceChanged);
        }
        if decision.proposal != proposal
            || decision.attestation != attestation
            || decision.decision_ref.len() != 71
            || !decision.decision_ref.starts_with("sha256:")
            || !decision.decision_ref[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ClientError::ControlBinding);
        }
        self.fenced = false;
        Ok(VerifiedControlDecision(decision))
    }
}

#[cfg(all(test, unix))]
mod tests;
