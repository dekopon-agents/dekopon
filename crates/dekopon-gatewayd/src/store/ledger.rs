use dekopon_core::AgentId;
use serde::{Deserialize, Serialize};

use super::{DurableStore, StoreError};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct HoldId(u64);

pub(crate) struct Ledger<'store> {
    store: &'store DurableStore,
}

#[derive(Clone, Debug)]
pub(crate) struct Hold {
    pub(crate) id: HoldId,
    pub(crate) agent: AgentId,
    pub(crate) estimate_micros: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HoldOutcome {
    Settled { spent_micros: u64 },
    Crashed,
}

impl Ledger<'_> {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub(crate) fn open_hold(
        &self,
        agent: &AgentId,
        estimate_micros: u64,
    ) -> Result<Hold, StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub(crate) fn settle(&self, hold: Hold, outcome: HoldOutcome) -> Result<(), StoreError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/spend-slice1")]
    pub(crate) fn settle_crashed_at_boot(&self) -> Result<Vec<Hold>, StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub(crate) fn spent_micros(&self, agent: &AgentId, since_ms: u64) -> Result<u64, StoreError> {
        todo!()
    }
}
