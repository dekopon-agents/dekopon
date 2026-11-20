use std::time::SystemTime;

use dekopon_core::AgentId;
use dekopon_model_token_governor::{Call, HistoryRow, Tokens};
use serde::{Deserialize, Serialize};

use super::{DurableStore, StoreFailed};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct HoldId(u64);

pub(crate) struct Ledger<'store> {
    store: &'store DurableStore,
}

#[must_use = "an open hold is charged its full estimate at the next boot unless it is settled"]
#[derive(Debug)]
pub(crate) struct Hold {
    id: HoldId,
    agent: AgentId,
    model: String,
    estimate: Tokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Settlement {
    Spent(Tokens),
    Unreturned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Provenance {
    Settled,
    Unreturned,
    Crashed,
}

#[derive(Clone, Debug)]
pub(crate) struct ChargeRow {
    pub(crate) agent: AgentId,
    pub(crate) model: String,
    pub(crate) at: SystemTime,
    pub(crate) tokens: Tokens,
    pub(crate) provenance: Provenance,
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
    pub(crate) fn open_hold(&self, call: &Call, estimate: Tokens) -> Result<Hold, StoreFailed> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub(crate) fn settle(&self, hold: Hold, settlement: Settlement) -> Result<(), StoreFailed> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub(crate) fn charge_crashed_holds(
        &self,
        at: SystemTime,
    ) -> Result<Vec<ChargeRow>, StoreFailed> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/spend-slice1"
    )]
    pub(crate) fn history(&self, since: SystemTime) -> Result<Vec<HistoryRow>, StoreFailed> {
        todo!()
    }
}
