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
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ConversationKey(String);

impl ConversationKey {
    pub(crate) fn of(transport: &TransportId, conversation: &Conversation) -> Self {
        let parts = (
            conversation.kind.as_str(),
            conversation.container.as_deref(),
            conversation.id.as_str(),
            conversation.thread.as_deref(),
        );
        let encoded = serde_json::to_string(&parts).unwrap_or_default();
        Self(format!("{transport}:{encoded}"))
    }

    pub(super) const fn stored(key: String) -> Self {
        Self(key)
    }

    pub(crate) fn transport(&self) -> Option<TransportId> {
        self.0.split_once(':')?.0.parse().ok()
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

impl RecordVersion {
    pub(super) const CURRENT: Self = Self(NonZeroU32::MIN);

    pub(super) fn of(stored: i64) -> Option<Self> {
        u32::try_from(stored)
            .ok()
            .and_then(NonZeroU32::new)
            .map(Self)
    }

    pub(super) const fn get(self) -> u32 {
        self.0.get()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Snapshot {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) wakes: Vec<WakeRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) follows: Vec<FollowRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) ownership: Vec<ThreadClaim>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) jobs: Vec<JobRow>,
}

impl Snapshot {
    pub(super) fn is_empty(&self) -> bool {
        self.wakes.is_empty()
            && self.follows.is_empty()
            && self.ownership.is_empty()
            && self.jobs.is_empty()
    }

    pub(super) fn apply(&mut self, change: Change) {
        match change {
            Change::PutWake(row) => {
                self.wakes.retain(|wake| wake.id != row.id);
                self.wakes.push(row);
            }
            Change::RemoveWake(id) => self.wakes.retain(|wake| wake.id != id),
            Change::Follow(row) => {
                self.follows.retain(|follow| follow.key != row.key);
                self.follows.push(row);
            }
            Change::Unfollow { follow } => self.follows.retain(|row| row.key != follow),
            Change::Leave { follow } => {
                self.follows.retain(|row| row.key != follow);
                self.ownership.clear();
            }
            Change::Claim(claim) => {
                if !self.ownership.contains(&claim) {
                    self.ownership.push(claim);
                }
            }
            Change::Revoke(claim) => self.ownership.retain(|owned| owned != &claim),
            Change::Release(claim) => self.ownership.retain(|owned| !owned.same_thread(&claim)),
            Change::JobStarted(row) => {
                self.jobs.retain(|job| job.id != row.id);
                self.jobs.push(row);
            }
            Change::JobEnded(id) => self.jobs.retain(|job| job.id != id),
        }
    }

    pub(super) fn expire(&mut self, now_ms: u64, retention_ms: u64) -> bool {
        let kept = |at_ms: u64| at_ms.saturating_add(retention_ms) > now_ms;
        let before = (self.wakes.len(), self.jobs.len(), self.follows.len());
        self.wakes.retain(|wake| kept(wake.next_at_ms));
        self.jobs.retain(|job| kept(job.deadline_ms));
        self.follows
            .retain(|follow| follow.marker.until_ms > now_ms);
        before != (self.wakes.len(), self.jobs.len(), self.follows.len())
    }
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
    Unfollow { follow: String },
    Leave { follow: String },
    Claim(ThreadClaim),
    Revoke(ThreadClaim),
    Release(ThreadClaim),
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
