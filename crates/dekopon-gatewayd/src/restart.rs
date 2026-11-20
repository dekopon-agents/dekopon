#![expect(
    dead_code,
    reason = "skeleton: rpi/tick-runner wires reader restarts and tick panics into serving"
)]

use std::time::Duration;

use dekopon_agent::wake::WakeId;
use dekopon_core::Backoff;

use crate::TransportConnectProblem;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReaderVerdict {
    Restart { after: Duration },
    Stop,
}

pub(crate) struct ReaderRestarts {
    backoff: Backoff,
}

impl ReaderRestarts {
    #[expect(clippy::todo, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn new() -> Self {
        todo!()
    }

    #[expect(clippy::todo, unused_variables, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn failed(&mut self, problem: &TransportConnectProblem) -> ReaderVerdict {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn connected(&mut self) {
        todo!()
    }
}

#[expect(clippy::todo, unused_variables, reason = "skeleton: rpi/tick-runner")]
pub(crate) fn record_tick_panic(wake: WakeId) {
    todo!()
}
