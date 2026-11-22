use std::sync::Arc;

use dekopon_core::{AgentId, VmSession};
use dekopon_model_token_governor::{
    Admission, Call, Metering, ModelUsage, Outcome, Refusal, Tokens, Via,
};
use parking_lot::Mutex;
use thiserror::Error;

const MAX_OPEN_GRANTS: usize = 4;
const GRANT_BACKEND: &str = "vm-grant";

pub struct SessionGrants {
    metering: Arc<Metering>,
    agent: AgentId,
    open: Mutex<Vec<(VmSession, Admission)>>,
    ledger: Option<Arc<dyn GrantLedger>>,
}

pub trait GrantLedger: Send + Sync {
    fn carved(
        &self,
        agent: &AgentId,
        session: &VmSession,
        model: &str,
        estimate: Tokens,
    ) -> Result<(), GrantLedgerFailed>;

    fn tallied(&self, session: &VmSession, spent: Tokens) -> Result<(), GrantLedgerFailed>;

    fn released(&self, session: &VmSession, release: GrantRelease)
    -> Result<(), GrantLedgerFailed>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantRelease {
    Tallied,
    Unreturned,
}

#[derive(Clone, Copy, Debug, Error)]
#[error("the gateway could not record this grant")]
pub struct GrantLedgerFailed;

#[derive(Debug, Error)]
pub enum GrantRefusal {
    #[error("the agent's model budget cannot cover this grant")]
    Budget(Refusal),
    #[error("this turn already holds its maximum of open grants")]
    Full,
    #[error(transparent)]
    Ledger(#[from] GrantLedgerFailed),
}

impl SessionGrants {
    #[must_use]
    pub fn new(metering: Arc<Metering>, agent: AgentId) -> Self {
        Self {
            metering,
            agent,
            open: Mutex::new(Vec::with_capacity(MAX_OPEN_GRANTS)),
            ledger: None,
        }
    }

    #[must_use]
    pub fn with_ledger(mut self, ledger: Arc<dyn GrantLedger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    pub fn carve(
        &self,
        session: VmSession,
        model: &str,
        estimate: Tokens,
    ) -> Result<(), GrantRefusal> {
        // The lock is held through the ledger write so a same-session carve cannot reuse a hold
        // that a failed write is about to refund.
        let mut open = self.open.lock();
        if open.iter().any(|(open, _)| *open == session) {
            return Ok(());
        }
        if open.len() >= MAX_OPEN_GRANTS {
            return Err(GrantRefusal::Full);
        }
        let admission = self
            .metering
            .admit_up_to(
                Call {
                    agent: self.agent.clone(),
                    model: model.to_owned(),
                    backend: GRANT_BACKEND,
                    via: Via::Proxy {
                        session: Some(session),
                    },
                },
                estimate,
            )
            .map_err(GrantRefusal::Budget)?;
        if let Some(ledger) = &self.ledger
            && let Err(failed) =
                ledger.carved(&self.agent, &session, model, admission.estimate().input)
        {
            drop(open);
            admission.settle(Outcome::NotSent);
            return Err(GrantRefusal::Ledger(failed));
        }
        open.push((session, admission));
        Ok(())
    }

    pub fn tally(&self, session: &VmSession, spent: Tokens) {
        let Some(admission) = self.take(session) else {
            return;
        };
        admission.observe_usage(ModelUsage {
            input_tokens: Some(spent.0),
            output_tokens: Some(0),
            ..ModelUsage::default()
        });
        admission.settle(Outcome::Succeeded);
        if let Some(ledger) = &self.ledger {
            report(session, ledger.tallied(session, spent));
            report(session, ledger.released(session, GrantRelease::Tallied));
        }
    }

    pub fn release(&self, session: &VmSession) {
        if let Some(admission) = self.take(session) {
            self.unreturned(session, admission);
        }
    }

    pub fn release_all(&self) {
        let open = std::mem::take(&mut *self.open.lock());
        for (session, admission) in open {
            self.unreturned(&session, admission);
        }
    }

    fn take(&self, session: &VmSession) -> Option<Admission> {
        let mut open = self.open.lock();
        let index = open.iter().position(|(open, _)| open == session)?;
        Some(open.swap_remove(index).1)
    }

    fn unreturned(&self, session: &VmSession, admission: Admission) {
        admission.settle_with_kind(Outcome::Failed, "grant_unreturned");
        if let Some(ledger) = &self.ledger {
            report(session, ledger.released(session, GrantRelease::Unreturned));
        }
    }
}

impl Drop for SessionGrants {
    fn drop(&mut self) {
        self.release_all();
    }
}

fn report(session: &VmSession, recorded: Result<(), GrantLedgerFailed>) {
    if recorded.is_err() {
        tracing::warn!(
            event = "agent_grant_ledger_failed",
            vm.session = session.as_str(),
            "agent.grant.ledger.failed"
        );
    }
}
