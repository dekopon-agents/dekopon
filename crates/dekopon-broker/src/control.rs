//! Core model/effort control admission, wholly inside the authenticated broker boundary.

use dekopon_broker_protocol::{
    ControlDecision, ControlOutcome, ControlProposal, ControlTarget, validate_control_targets,
};
use dekopon_core::SurfaceEpoch;
use dekopon_policy::AgentControlAction;

use super::*;

pub(super) fn fresh_epoch() -> Result<SurfaceEpoch, BrokerBuildError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| BrokerBuildError::SurfaceEntropy {
        reason: error.to_string(),
    })?;
    let epoch = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(epoch
        .parse()
        .expect("64 lowercase hex bytes are a valid epoch"))
}

impl<A: AuditLog> Broker<A> {
    /// Installs a startup-fixed model/effort ceiling. Empty disables controls, even with policy.
    pub fn with_control_targets(
        mut self,
        targets: Vec<ControlTarget>,
    ) -> Result<Self, BrokerBuildError> {
        validate_control_targets(&targets)?;
        self.control_targets = targets;
        Ok(self)
    }

    /// Random startup identity used only by hosts to invalidate private retained state.
    pub fn surface_epoch(&self) -> &SurfaceEpoch {
        &self.surface_epoch
    }

    /// Fresh, atomic admission for every changed dimension. Does not run a provider or model.
    ///
    /// Routing is re-derived for every proposal. Attested controls require explicit chat scope;
    /// unlike legacy provider operations there is no scope-less fallback. A direct peer must be
    /// mapped to the same agent. `from`/`to` are typed intent, not broker-verified client state.
    pub async fn authorize_control(
        &self,
        peer: &AuthenticatedContext,
        grant: Option<&AttestorGrant>,
        attestation: Option<&Attestation>,
        proposal: ControlProposal,
    ) -> Result<ControlDecision, BrokerError> {
        if !proposal.is_well_formed(attestation) {
            return Err(BrokerError::InvalidControl);
        }
        let reserved = self.replay.reserve(&proposal.id).await?;
        let mut context = peer.clone();
        let mut policy_ids = BTreeSet::new();
        let mut reason = (!reserved).then_some("replayed-control");
        if reason.is_none() && proposal.surface_epoch != self.surface_epoch {
            reason = Some("surface-changed");
        }
        if reason.is_none() {
            if let Some(claim) = attestation {
                // Do not call the legacy context resolver until explicit chat authority held.
                if !claim.scope.as_ref().is_some_and(|scope| {
                    grant.is_some_and(|grant| grant.permits_chat(&claim.subject, scope))
                }) {
                    context = peer.with_refused_subject(claim.subject.clone());
                    reason = Some("attestation-denied");
                } else {
                    let (derived, refusal) = self.resolve_context(peer, grant, claim);
                    context = derived;
                    if let Some(refusal) = refusal {
                        policy_ids.extend(refusal.policy_ids);
                        reason = Some(refusal.reason);
                    }
                }
            } else if !matches!(peer.actor(), Actor::Agent { agent } if agent == &proposal.scope.agent)
            {
                reason = Some("agent-mismatch");
            }
        }
        if reason.is_none() {
            // Re-evaluate prompt permission for direct AND attested controls, not a prior surface.
            // resolve_context already gated attested routing; this decision also supplies the allow
            // policy IDs in the atomic control record.
            let prompt = self.authorize_agent_prompt(&context, &proposal.scope.agent);
            policy_ids.extend(prompt.determining_policy_ids);
            if !prompt.allowed {
                reason = Some(if prompt.errors_present {
                    "policy-error"
                } else {
                    "agent-denied"
                });
            }
        }
        if reason.is_none() {
            let configured = |selection: &dekopon_core::ModelSelection| {
                self.control_targets.iter().any(|target| {
                    target.model == selection.model && target.efforts.contains(&selection.effort)
                })
            };
            if !configured(&proposal.from) || !configured(&proposal.to) {
                reason = Some("target-denied");
            } else if proposal.from == proposal.to {
                reason = Some("no-change");
            }
        }
        if reason.is_none() {
            for (changed, action) in [
                (
                    proposal.from.model != proposal.to.model,
                    AgentControlAction::ModelSelect,
                ),
                (
                    proposal.from.effort != proposal.to.effort,
                    AgentControlAction::EffortSet,
                ),
            ] {
                if !changed {
                    continue;
                }
                let decision = self.policy.authorize(PolicyRequest {
                    principal: context.principal().clone(),
                    target: PolicyTarget::AgentControl {
                        agent: proposal.scope.agent.clone(),
                        action,
                        from: proposal.from.clone(),
                        to: proposal.to.clone(),
                    },
                    context: policy_context(&context),
                });
                policy_ids.extend(decision.determining_policy_ids);
                // Evaluate both changed dimensions even when the first denies; no partial permit.
                // A later iteration never downgrades the reason: `policy-error` says the operator's
                // policy is broken and `policy-denied` says it worked, so letting the effort
                // dimension's ordinary denial overwrite the model dimension's evaluation error
                // would hide the one refusal an operator has to act on.
                if !decision.allowed && reason != Some("policy-error") {
                    reason = Some(if decision.errors_present {
                        "policy-error"
                    } else {
                        "policy-denied"
                    });
                }
            }
        }
        let allowed = reason.is_none();
        let policy_ids = policy_ids.into_iter().collect::<Vec<_>>();
        let decision_ref = decision_evidence_digest(
            "control-decision-v1",
            &serde_json::json!({
                "peer": peer, "context": context, "attestation": attestation,
                "proposal": proposal, "surfaceEpoch": self.surface_epoch,
                "authorizedBy": self.broker_principal, "policyRevision": self.policy_revision,
                "policyDigest": self.policy_digest, "policyIds": policy_ids,
                "allowed": allowed, "reason": reason,
            }),
        )?;
        // The deployed AuditLog also syncs the independent checkpoint. A failed append cannot
        // return an admission, and the already-reserved ID remains consumed in this process.
        self.audit
            .append(AuditEvent::ControlDecision {
                proposal: proposal.clone(),
                peer: peer.principal().clone(),
                principal: context.principal().clone(),
                actor: context.actor().clone(),
                via: context.via().cloned(),
                attested_subject: context.attested_subject().cloned(),
                surface_epoch: self.surface_epoch.clone(),
                authorized_by: self.broker_principal.clone(),
                policy_revision: self.policy_revision.clone(),
                policy_digest: self.policy_digest.clone(),
                policy_ids,
                allowed,
                reason: reason.map(str::to_owned),
                decision_ref: decision_ref.clone(),
            })
            .await
            .map_err(|source| {
                report_audit_failure("control-decision", &proposal.id, &source);
                BrokerError::DecisionAudit { source }
            })?;
        tracing::info!(audit.event = "broker.control.decision", control = %proposal.id,
            job = %proposal.scope.job, request = %proposal.scope.request,
            session = %proposal.scope.session, generation = %proposal.scope.generation,
            sequence = proposal.sequence, agent = %proposal.scope.agent,
            from_model = %proposal.from.model, to_model = %proposal.to.model,
            from_effort = %proposal.from.effort, to_effort = %proposal.to.effort,
            admitted = allowed, reason = reason.unwrap_or("admitted"), decision_ref = %decision_ref);
        Ok(ControlDecision {
            proposal,
            attestation: attestation.cloned(),
            surface_epoch: self.surface_epoch.clone(),
            decision_ref,
            outcome: if allowed {
                ControlOutcome::Admitted
            } else {
                ControlOutcome::Denied
            },
        })
    }
}
