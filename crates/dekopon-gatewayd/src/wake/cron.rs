#![expect(
    dead_code,
    reason = "skeleton: state/cron-wakes wires cron wakes into the tool and the store"
)]

use std::time::SystemTime;

use dekopon_agent::wake::WakeRefusal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug)]
pub(crate) struct CronSchedule {
    expression: String,
    parsed: Box<croner::Cron>,
}

impl CronSchedule {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: state/cron-wakes")]
    pub(crate) fn parse(expression: &str) -> Result<Self, WakeRefusal> {
        todo!()
    }

    #[expect(clippy::todo, unused_variables, reason = "skeleton: state/cron-wakes")]
    pub(crate) fn next_after(&self, after: SystemTime) -> Option<SystemTime> {
        todo!()
    }

    #[expect(clippy::todo, unused_variables, reason = "skeleton: state/cron-wakes")]
    pub(crate) fn missed_between(&self, from: SystemTime, to: SystemTime) -> MissedTicks {
        todo!()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MissedTicks {
    None,
    Merged { count: u32, first: SystemTime },
}

impl Serialize for CronSchedule {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: state/cron-wakes")]
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        todo!()
    }
}

impl<'de> Deserialize<'de> for CronSchedule {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: state/cron-wakes")]
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        todo!()
    }
}

#[must_use = "a cron registration is stored only by WakeStore::register_cron"]
pub(crate) struct CronRegistration {
    pub(crate) anchor: super::Anchor,
    pub(crate) schedule: CronSchedule,
    pub(crate) note: String,
}

impl super::WakeStore {
    #[expect(clippy::todo, unused_variables, reason = "skeleton: state/cron-wakes")]
    pub(crate) fn register_cron(
        &self,
        registration: CronRegistration,
        now: SystemTime,
    ) -> Result<dekopon_agent::wake::WakeSummary, WakeRefusal> {
        todo!()
    }
}
