#![expect(
    clippy::todo,
    reason = "skeleton: state/grant-settle fills these bodies"
)]

use std::num::NonZeroU64;

use dekopon_core::VmSession;
use serde::{Deserialize, Serialize};

use crate::RunOutcome;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct GrantId(NonZeroU64);

impl GrantId {
    #[must_use]
    pub const fn new(id: NonZeroU64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UsageClasses {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub output: u64,
    pub reasoning: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Grant {
    pub id: GrantId,
    pub amount: NonZeroU64,
    pub weights: UsageClasses,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PlanReading {
    pub used_percent: u8,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Tally {
    pub grant: GrantId,
    pub classes: UsageClasses,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanReading>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GrantedSession {
    pub grant: GrantId,
    pub session: VmSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GrantUse {
    Used(GrantedSession),
    Unused(GrantId),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GrantSettlement {
    pub grant_use: Option<GrantUse>,
    pub tallies: Vec<Tally>,
    pub session_gone: Option<VmSession>,
}

impl GrantSettlement {
    #[must_use]
    pub fn grant_used(&self) -> Option<GrantedSession> {
        todo!()
    }

    #[must_use]
    pub fn grant_unused(&self) -> Option<GrantId> {
        todo!()
    }
}

impl RunOutcome {
    #[must_use]
    pub fn grant_required() -> Self {
        todo!()
    }

    #[must_use]
    pub fn is_grant_required(&self) -> bool {
        todo!()
    }
}
