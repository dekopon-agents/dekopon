use std::sync::Arc;

use dekopon_core::{AgentId, VmSession};
use dekopon_model_token_governor::{Admission, Metering, Refusal, Tokens};
use parking_lot::Mutex;
use thiserror::Error;

#[expect(dead_code, reason = "skeleton: state/grant-settle fills the carrier")]
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
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/grant-settle"
    )]
    pub fn new(metering: Arc<Metering>, agent: AgentId) -> Self {
        todo!()
    }

    #[must_use]
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub fn with_ledger(self, ledger: Arc<dyn GrantLedger>) -> Self {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/grant-settle"
    )]
    pub fn carve(
        &self,
        session: VmSession,
        model: &str,
        estimate: Tokens,
    ) -> Result<(), GrantRefusal> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/grant-settle"
    )]
    pub fn tally(&self, session: &VmSession, spent: Tokens) {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/grant-settle"
    )]
    pub fn release(&self, session: &VmSession) {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/grant-settle")]
    pub fn release_all(&self) {
        todo!()
    }
}
