use crate::asset::AssetSourceRef;
use dekopon_agent::memory::Item;
use dekopon_core::AssetNumber;
use dekopon_core::asset::AssetKind;
use dekopon_model::model::ModelKey;
use serde::{Deserialize, Serialize};
use std::{
    io,
    num::{NonZeroU32, NonZeroU64},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RecordId(pub NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TurnId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct GrantDigest(pub [u8; 8]);

pub(crate) struct TurnRecord {
    pub id: RecordId,
    pub turn: TurnId,
    pub at_ms: u64,
    pub grant: GrantDigest,
    pub model: ModelKey,
    pub original: Vec<Item>,
    pub summaries: Vec<Slot>,
    pub plan: Option<SummaryPlan>,
    pub assets: Vec<AssetRow>,
    pub next_asset_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AssetRow {
    pub id: u64,
    pub at_ms: u64,
    pub name: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AssetSourceRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SummaryPlan {
    pub model: String,
    pub parts: Vec<NonZeroU32>,
    pub max_input_bytes: NonZeroU32,
}

#[derive(Clone, Debug)]
pub(crate) enum Slot {
    Queued,
    Running,
    Generated(String),
    Error(SummaryError),
}

#[derive(Clone, Debug)]
pub(crate) struct SummaryError {
    pub kind: SummaryFailure,
    pub detail: String,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SummaryFailure {
    Timeout,
    Refused,
    ModelFailed,
    Empty,
    Overflow,
    Panicked,
    Io { kind: io::ErrorKind },
    Unconfigured,
}

#[derive(Clone, Debug)]
pub(crate) struct ArrivedAsset {
    pub item: u32,
    pub number: AssetNumber,
    pub kind: AssetKind,
    pub bytes: Option<u64>,
    pub summaries: Vec<Slot>,
}
