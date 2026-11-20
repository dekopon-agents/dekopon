use std::num::NonZeroU32;

use dekopon_agent::wake::WakeId;
use dekopon_broker_protocol::{ChatTransportKind, Conversation, TraceParent};
use dekopon_core::{AgentId, Subject};
use serde::{Deserialize, Serialize};

use super::delivery::{DeliveryId, PendingDelivery};
use crate::{
    pull_request_links::PullRequestLinkTarget, transport::ReplyTarget, wake::CronSchedule,
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ConversationKey(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct RecordVersion(NonZeroU32);

pub(crate) struct Record {
    pub(crate) v: RecordVersion,
    pub(crate) snapshot: Snapshot,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub(crate) wakes: Vec<WakeRow>,
    pub(crate) follows: Vec<FollowRow>,
    pub(crate) ownership: Vec<OwnershipRow>,
    pub(crate) jobs: Vec<JobRow>,
    pub(crate) deliveries: Vec<PendingDelivery>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AnchorRow {
    pub(crate) transport: String,
    pub(crate) kind: ChatTransportKind,
    pub(crate) subject: Subject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subject_id: Option<Subject>,
    pub(crate) conversation: Conversation,
    pub(crate) reply: ReplyTarget,
    pub(crate) agent: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheduled_in: Option<TraceParent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct WakeRow {
    pub(crate) id: u32,
    pub(crate) anchor: AnchorRow,
    pub(crate) note: String,
    pub(crate) next_at_ms: u64,
    pub(crate) kind: WakeRowKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "type",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum WakeRowKind {
    Once,
    Watch {
        script: String,
        prev: String,
        every_ms: u64,
        until_ms: u64,
    },
    Digest {
        script: String,
        prev: String,
        every_ms: u64,
        until_ms: u64,
    },
    Subscription {
        target: PullRequestLinkTarget,
    },
    Cron {
        schedule: CronSchedule,
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct FollowRow {
    pub(crate) key: String,
    pub(crate) at_ms: u64,
    pub(crate) until_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct OwnershipRow {
    pub(crate) team_id: String,
    pub(crate) channel_id: String,
    pub(crate) thread_ts: String,
    pub(crate) user_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct JobRow {
    pub(crate) id: u64,
    pub(crate) script_head: String,
    pub(crate) deadline_ms: u64,
}

pub(crate) enum Change {
    PutWake(WakeRow),
    RemoveWake(WakeId),
    Follow(FollowRow),
    Leave { key: String },
    Claim(OwnershipRow),
    Revoke(OwnershipRow),
    JobStarted(JobRow),
    JobEnded { id: u64 },
    Deliver(PendingDelivery),
    Acknowledged(DeliveryId),
}
