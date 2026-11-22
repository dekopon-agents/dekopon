use std::{fmt, num::NonZeroU32};

use dekopon_agent::wake::WakeId;
use dekopon_broker_protocol::{ChatTransportKind, Conversation, TraceParent};
use dekopon_core::{AgentId, Subject, TransportId};
use dekopon_process::JobId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    memory::journal::FollowMarker,
    pull_request_links::PullRequestLinkTarget,
    transport::{ReplyTarget, ThreadClaim},
    wake::CronSchedule,
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ConversationKey(String);

impl ConversationKey {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn of(transport: &TransportId, conversation: &Conversation) -> Self {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/durable-store")]
    pub(crate) fn transport(&self) -> TransportId {
        todo!()
    }
}

impl fmt::Display for ConversationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub(super) struct RecordVersion(NonZeroU32);

pub(super) struct Record {
    pub(super) version: RecordVersion,
    pub(super) snapshot: Snapshot,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub(crate) wakes: Vec<WakeRow>,
    pub(crate) follows: Vec<FollowRow>,
    pub(crate) ownership: Vec<ThreadClaim>,
    pub(crate) jobs: Vec<JobRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AnchorRow {
    pub(crate) transport: TransportId,
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
    #[serde(with = "wake_id")]
    pub(crate) id: WakeId,
    pub(crate) anchor: AnchorRow,
    pub(crate) note: String,
    pub(crate) next_at_ms: u64,
    pub(crate) kind: WakeRowKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending: Option<PendingDelivery>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields, rename_all = "camelCase")]
pub(crate) enum WakeRowKind {
    Once,
    Watch(WatchRow),
    Digest(WatchRow),
    Subscription { target: PullRequestLinkTarget },
    Cron { schedule: CronSchedule },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct WatchRow {
    pub(crate) script: String,
    pub(crate) prev: String,
    pub(crate) every_ms: u64,
    pub(crate) until_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PendingDelivery {
    pub(crate) text: String,
    pub(crate) notice: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct FollowRow {
    pub(crate) key: String,
    pub(crate) marker: FollowMarker,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct JobRow {
    #[serde(with = "job_id")]
    pub(crate) id: JobId,
    pub(crate) anchor: AnchorRow,
    pub(crate) script_head: String,
    pub(crate) deadline_ms: u64,
}

pub(crate) enum Change {
    PutWake(WakeRow),
    RemoveWake(WakeId),
    Follow(FollowRow),
    Leave { follow: String },
    Claim(ThreadClaim),
    Revoke(ThreadClaim),
    JobStarted(JobRow),
    JobEnded(JobId),
}

mod wake_id {
    use super::{Deserialize, Deserializer, Serializer, WakeId};

    pub(super) fn serialize<S: Serializer>(id: &WakeId, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(id.0)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<WakeId, D::Error> {
        u32::deserialize(deserializer).map(WakeId)
    }
}

mod job_id {
    use super::{Deserialize, Deserializer, JobId, Serializer};

    pub(super) fn serialize<S: Serializer>(id: &JobId, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(id.get())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<JobId, D::Error> {
        u64::deserialize(deserializer).map(JobId::new)
    }
}
