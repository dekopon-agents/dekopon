#![expect(
    dead_code,
    reason = "skeleton: daemon/supervisor wires these into serve and boot"
)]

use std::time::Duration;

use dekopon_agent::wake::WakeId;
use dekopon_core::{Backoff, TaskOutcome};
use serde::{Deserialize, Serialize};

use crate::{TransportConnectProblem, store::StoreError, wake::Delivery};

#[derive(Debug)]
pub(crate) enum FatalCause {
    Store(StoreError),
    Reload(TaskOutcome),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FatalLabel {
    Store,
    Reload,
}

impl FatalCause {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub(crate) const fn label(&self) -> FatalLabel {
        todo!()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReaderVerdict {
    Restart { after: Duration },
    Stop,
}

pub(crate) struct ReaderRestarts {
    backoff: Backoff,
}

impl ReaderRestarts {
    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub(crate) fn new() -> Self {
        todo!()
    }

    #[expect(clippy::todo, unused_variables, reason = "skeleton: daemon/supervisor")]
    pub(crate) fn failed(&mut self, problem: &TransportConnectProblem) -> ReaderVerdict {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/supervisor")]
    pub(crate) fn connected(&mut self) {
        todo!()
    }
}

pub(crate) enum TickEnd {
    Resolved(Option<Box<Delivery>>),
    Panicked { wake: WakeId },
}

#[expect(clippy::todo, unused_variables, reason = "skeleton: daemon/supervisor")]
pub(crate) fn record_tick_panic(wake: WakeId) {
    todo!()
}
