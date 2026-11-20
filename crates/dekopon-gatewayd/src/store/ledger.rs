use std::time::SystemTime;

use dekopon_core::AgentId;
use serde::{Deserialize, Serialize};

use super::{DurableStore, StoreError};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct HoldId(u64);

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct SpendMicros(u64);

pub(crate) struct Ledger<'store> {
    store: &'store DurableStore,
}

#[must_use = "an open hold is charged its full estimate at the next boot unless it is settled"]
#[derive(Debug)]
pub(crate) struct Hold {
    id: HoldId,
    agent: AgentId,
    estimate: SpendMicros,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HoldOutcome {
    Spent(SpendMicros),
    Unreturned,
}

impl DurableStore {
    #[expect(clippy::todo, reason = "skeleton: state/spend-slice1")]
    pub(crate) fn ledger(&self) -> Ledger<'_> {
        todo!()
    }
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
        estimate: SpendMicros,
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
    pub(crate) fn charge_crashed_holds(&self) -> Result<Vec<(AgentId, SpendMicros)>, StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub(crate) fn spent(
        &self,
        agent: &AgentId,
        since: SystemTime,
    ) -> Result<SpendMicros, StoreError> {
        todo!()
    }
}
