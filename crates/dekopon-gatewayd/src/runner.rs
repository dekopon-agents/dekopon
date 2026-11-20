#![expect(
    dead_code,
    reason = "skeleton: rpi/tick-runner wires the runner into serving"
)]

mod receipt;

use serde::Deserialize;

use crate::{
    config::ResolvedBroker, notice::Destination, transport::ChatDriver, wake::CronSchedule,
};

#[expect(unused_imports, reason = "skeleton: rpi/tick-runner")]
pub(crate) use receipt::{Receipt, ReceiptBatches, ReceiptOutcome, ReceiptOutput};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitSha(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EpochSlug(String);

impl EpochSlug {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        todo!()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, serde::Serialize)]
#[serde(transparent)]
pub(crate) struct IntentId(String);

#[derive(Clone, Debug)]
pub(crate) struct RunnerEntry {
    pub(crate) principal: dekopon_core::Subject,
    pub(crate) schedule: CronSchedule,
    pub(crate) epoch: EpochSlug,
    pub(crate) approved: GitSha,
    pub(crate) packets: Destination,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Effect {
    pub(crate) intent: IntentId,
    pub(crate) capability: String,
    pub(crate) input: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TickPlan {
    pub(crate) effects: Vec<Effect>,
    #[serde(default)]
    pub(crate) packet: Option<String>,
}

#[derive(Debug)]
pub(crate) enum ParkReason {
    Truncated,
    Unparseable,
    Refused { intent: IntentId },
    BatchFailed { intents: Vec<IntentId> },
}

#[derive(Debug)]
#[must_use]
pub(crate) enum CycleOutcome {
    Idle,
    Applied { receipts: usize, packet: bool },
    Parked(ParkReason),
}

impl GitSha {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: rpi/tick-runner")]
    pub(crate) fn parse(text: &str) -> Option<Self> {
        todo!()
    }
}

impl RunnerEntry {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: rpi/tick-runner")]
    pub(crate) async fn run_cycle(
        &self,
        broker: &ResolvedBroker,
        packets: &dyn ChatDriver,
    ) -> CycleOutcome {
        todo!()
    }
}
