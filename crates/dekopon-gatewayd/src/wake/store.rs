use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_agent::wake::{WakeId, WakeRefusal, WakeSummary, WatchSummary};
use dekopon_core::github::{GithubEvent, GithubEventName, GithubThread};
use dekopon_core::{AgentId, AttestedSubject, Subject};
use dekopon_process::JobId;
use dekopon_shell::{CapabilityInvoker, Interpreter};

use super::{
    Anchor, Probe, Verdict, WakeOperation, WakeOrigin, WatchPurpose,
    delivery::{Delivered, Delivery, DeliveryDisposition, DeliveryTicket, JobNotice, Replay},
    fired_text, record_digest,
};
use crate::{
    config::WakeBounds,
    pull_request_links::PullRequestLinkTarget,
    store::{
        AnchorRow, Change, ConversationKey, Durability, DurableStore, JobRow, PendingDelivery,
        StoreFailed, WakeRow, WakeRowKind, WatchRow, millis,
    },
    transport::{InboundMessage, ReplyTarget},
};

const REOFFER: Duration = Duration::from_secs(30);

impl From<StoreFailed> for WakeRefusal {
    fn from(StoreFailed: StoreFailed) -> Self {
        Self::Store
    }
}

pub(crate) enum Schedule {
    Once {
        after: Duration,
    },
    Watch {
        probe: Probe,
        every: Duration,
        until: Duration,
    },
}

#[derive(Clone, Debug)]
struct Wake {
    id: WakeId,
    anchor: Anchor,
    note: String,
    next_at: SystemTime,
    kind: WakeKind,
    run_as: Option<RunAs>,
    pending: Option<Pending>,
}

#[derive(Clone, Debug)]
struct Pending {
    delivery: PendingDelivery,
    offer: Offer,
}

#[derive(Clone, Copy, Debug)]
enum Offer {
    InFlight,
    At(SystemTime),
}

impl Offer {
    fn due(self, now: SystemTime) -> bool {
        match self {
            Self::InFlight => false,
            Self::At(at) => at <= now,
        }
    }

    const fn at(self) -> Option<SystemTime> {
        match self {
            Self::InFlight => None,
            Self::At(at) => Some(at),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum RunAs {
    Space,
}

#[derive(Clone, Debug)]
enum WakeKind {
    Once,
    Subscription {
        target: PullRequestLinkTarget,
    },
    Watch {
        probe: Probe,
        every: Duration,
        until: SystemTime,
    },
}

impl Wake {
    fn is_digest(&self) -> bool {
        self.run_as.is_some()
            && match &self.kind {
                WakeKind::Watch { probe, .. } => match probe.purpose {
                    WatchPurpose::Digest => true,
                    WatchPurpose::Ordinary => false,
                },
                WakeKind::Once | WakeKind::Subscription { .. } => false,
            }
    }

    fn expired_digest(&self, now: SystemTime) -> bool {
        self.pending.is_none()
            && self.is_digest()
            && match &self.kind {
                WakeKind::Watch { until, .. } => *until <= now,
                WakeKind::Once | WakeKind::Subscription { .. } => false,
            }
    }

    fn durable(&self) -> bool {
        match self.anchor.durability {
            Durability::Durable => true,
            Durability::Memory => false,
        }
    }

    fn removed_once_delivered(&self) -> bool {
        match &self.kind {
            WakeKind::Once => true,
            WakeKind::Watch { .. } => !self.is_digest(),
            WakeKind::Subscription { .. } => false,
        }
    }

    fn summary(&self, now: SystemTime) -> WakeSummary {
        let until = |at: SystemTime| at.duration_since(now).unwrap_or_default();
        WakeSummary {
            id: self.id,
            note: self.note.clone(),
            due_in: until(self.next_at),
            watch: match &self.kind {
                WakeKind::Once | WakeKind::Subscription { .. } => None,
                WakeKind::Watch {
                    probe,
                    every,
                    until: end,
                } => Some(WatchSummary {
                    every: *every,
                    ends_in: until(*end),
                    last_output: probe.prev.clone(),
                }),
            },
        }
    }

    fn origin(&self) -> WakeOrigin {
        match &self.kind {
            WakeKind::Subscription { target } => WakeOrigin::Subscription(target.clone()),
            WakeKind::Watch { .. } if self.is_digest() => WakeOrigin::Digest,
            WakeKind::Once | WakeKind::Watch { .. } => WakeOrigin::Scheduled,
        }
    }

    fn fire(&mut self, reason: &str, output: Option<&str>) -> WakeId {
        let delivery = PendingDelivery {
            text: fired_text(self.id, &self.note, reason, output),
            notice: match output {
                Some(output) => format!("{}\n\n{output}", self.note),
                None => self.note.clone(),
            },
        };
        self.pending = Some(Pending {
            delivery,
            offer: Offer::InFlight,
        });
        self.id
    }

    fn fired(&self) -> Option<Fired> {
        let pending = self.pending.as_ref()?;
        Some(Fired {
            anchor: self.anchor.clone(),
            id: self.id,
            text: pending.delivery.text.clone(),
            notice: pending.delivery.notice.clone(),
            origin: self.origin(),
        })
    }

    fn record_lifecycle(&self, operation: WakeOperation) {
        if let WakeKind::Subscription { target } = &self.kind {
            record_subscription(self.id, &self.anchor, target, operation);
        }
        if self.is_digest() {
            let cause = match operation {
                WakeOperation::Failed => Some("probe"),
                WakeOperation::Expired => Some("horizon"),
                WakeOperation::Created
                | WakeOperation::Fired
                | WakeOperation::Declined
                | WakeOperation::Closed
                | WakeOperation::Cancelled => None,
            };
            record_digest(self.id, operation, cause);
        }
    }

    fn row(&self) -> WakeRow {
        WakeRow {
            id: self.id,
            anchor: self.anchor.row(),
            note: self.note.clone(),
            next_at_ms: millis(self.next_at),
            kind: match &self.kind {
                WakeKind::Once => WakeRowKind::Once,
                WakeKind::Subscription { target } => WakeRowKind::Subscription {
                    target: target.clone(),
                },
                WakeKind::Watch {
                    probe,
                    every,
                    until,
                } => {
                    let watch = WatchRow {
                        script: probe.script.clone(),
                        prev: probe.prev.clone(),
                        every_ms: u64::try_from(every.as_millis()).unwrap_or(u64::MAX),
                        until_ms: millis(*until),
                    };
                    match probe.purpose {
                        WatchPurpose::Ordinary => WakeRowKind::Watch(watch),
                        WatchPurpose::Digest => WakeRowKind::Digest(watch),
                    }
                }
            },
            pending: self
                .pending
                .as_ref()
                .map(|pending| pending.delivery.clone()),
        }
    }

    fn from_row(row: WakeRow, offer: Offer) -> Option<Self> {
        let anchor = Anchor::restored(row.anchor)?;
        let space = anchor.subject().principal_kind() == dekopon_core::PrincipalKind::Space;
        let run_as = space.then_some(RunAs::Space);
        let watch = |watch: WatchRow, purpose: WatchPurpose| WakeKind::Watch {
            probe: Probe {
                script: watch.script,
                prev: watch.prev,
                purpose,
            },
            every: Duration::from_millis(watch.every_ms),
            until: at(watch.until_ms),
        };
        let kind = match row.kind {
            WakeRowKind::Once => WakeKind::Once,
            WakeRowKind::Watch(row) => watch(row, WatchPurpose::Ordinary),
            WakeRowKind::Digest(row) if space => watch(row, WatchPurpose::Digest),
            WakeRowKind::Digest(_) => return None,
            WakeRowKind::Subscription { .. } if !space => return None,
            WakeRowKind::Subscription { target } => WakeKind::Subscription { target },
        };
        let wake = Self {
            id: row.id,
            anchor,
            note: row.note,
            next_at: at(row.next_at_ms),
            kind,
            run_as,
            pending: row.pending.map(|delivery| Pending { delivery, offer }),
        };
        if wake.run_as.is_some() {
            let space = wake.anchor.as_space()?;
            if space.subject() != wake.anchor.subject()
                || !wake
                    .anchor
                    .conversation
                    .is_canonical_for(wake.anchor.kind, wake.anchor.subject())
            {
                return None;
            }
            if let WakeKind::Watch { probe, every, .. } = &wake.kind
                && (every.is_zero()
                    || (probe.purpose == WatchPurpose::Digest && *every < Probe::DIGEST_INTERVAL))
            {
                return None;
            }
        }
        Some(wake)
    }
}

impl Anchor {
    pub(crate) fn restored(row: AnchorRow) -> Option<Self> {
        Some(Self {
            transport: row.transport,
            reply: match row.reply {
                ReplyTarget::Local { .. } => return None,
                reply @ (ReplyTarget::Slack { .. }
                | ReplyTarget::Fauxlack { .. }
                | ReplyTarget::Discord { .. }
                | ReplyTarget::Telegram { .. }
                | ReplyTarget::WhatsApp { .. }
                | ReplyTarget::GithubTrace) => reply,
            },
            kind: row.kind,
            subject: match row.subject_id {
                Some(id) => AttestedSubject::paired(row.subject, id),
                None => AttestedSubject::one(row.subject),
            }
            .ok()?,
            conversation: row.conversation,
            agent: row.agent,
            scheduled_in: row.scheduled_in,
            durability: Durability::Durable,
        })
    }

    pub(crate) fn row(&self) -> AnchorRow {
        AnchorRow {
            transport: self.transport.clone(),
            kind: self.kind,
            subject: self.subject.name().clone(),
            subject_id: self.subject.id_form().cloned(),
            conversation: self.conversation.clone(),
            reply: self.reply.clone(),
            agent: self.agent.clone(),
            scheduled_in: self.scheduled_in,
        }
    }

    pub(crate) fn key(&self) -> ConversationKey {
        ConversationKey::of(&self.transport, &self.conversation)
    }
}

pub(crate) struct Fired {
    anchor: Anchor,
    id: WakeId,
    text: String,
    notice: String,
    origin: WakeOrigin,
}

impl Fired {
    pub(crate) fn is_space(&self) -> bool {
        self.anchor.subject().principal_kind() == dekopon_core::PrincipalKind::Space
    }
    pub(crate) const fn id(&self) -> WakeId {
        self.id
    }

    pub(crate) const fn agent(&self) -> &AgentId {
        self.anchor.agent()
    }

    pub(crate) fn into_inbound(self) -> InboundMessage {
        let text = match &self.origin {
            WakeOrigin::Subscription(target) => format!(
                "[gateway: PR subscription wake] Re-review https://github.com/{}/pull/{} at its current head using granted tools. Read the prior review and current diff; submit a fresh head-pinned COMMENT review. Decide whether the blocking findings changed. If every capability invocation in this turn successfully refreshed this PR's review, reply in this chat thread only if they changed; otherwise call decline_chat_reply. Any other invocation requires a chat report even after a successful refresh. This turn runs as the space alone; no person's grants are available.",
                target.repository, target.number
            ),
            WakeOrigin::Digest => format!(
                "[gateway: daily space digest] Verify the waiting items through granted reads. Post one concise threaded digest of stale PRs, unanswered review requests and unassigned issues only if nonempty and changed. Write GitHub logins as plain text so nobody gets pinged. Otherwise call decline_chat_reply, including after read work. This turn runs as the space alone.\n{}",
                self.text
            ),
            WakeOrigin::Scheduled => self.text,
        };
        self.anchor.inbound(self.id, text, self.notice, self.origin)
    }
}

#[must_use]
pub(crate) struct Tick {
    id: WakeId,
    anchor: Anchor,
    probe: Probe,
}

pub(crate) enum Resolved {
    Rearmed,
    Fired(Box<Delivery>),
    Gone,
}

impl Tick {
    #[cfg(test)]
    pub(crate) fn for_test(id: WakeId, anchor: Anchor) -> Self {
        Self {
            id,
            anchor,
            probe: Probe::scripted_for_test("echo"),
        }
    }

    pub(crate) const fn id(&self) -> WakeId {
        self.id
    }

    pub(crate) const fn anchor(&self) -> &Anchor {
        &self.anchor
    }

    pub(crate) fn run(&self, leg: &dyn CapabilityInvoker, shell: &Interpreter) -> Verdict {
        self.probe.tick(leg, shell)
    }

    pub(crate) fn resolve(
        self,
        store: &WakeStore,
        verdict: Verdict,
        now: SystemTime,
    ) -> Result<Resolved, StoreFailed> {
        let digest = self.probe.purpose == WatchPurpose::Digest;
        let operation = match &verdict {
            Verdict::Failed { .. } => WakeOperation::Failed,
            Verdict::Fire { .. } | Verdict::Wait { .. } => WakeOperation::Expired,
        };
        let fired = store.mutate(
            |_| operation,
            |wakes| {
                let index = wakes.iter().position(|wake| wake.id == self.id)?;
                if wakes[index].pending.is_some() {
                    return None;
                }
                if wakes[index].is_digest() {
                    if wakes[index].expired_digest(now) {
                        wakes.remove(index);
                        return None;
                    }
                    let (output, fire) = match verdict {
                        Verdict::Fire { output } => (output, true),
                        Verdict::Wait { output } => (output, false),
                        Verdict::Failed { .. } => {
                            wakes.remove(index);
                            return None;
                        }
                    };
                    let changed = match &wakes[index].kind {
                        WakeKind::Watch { probe, .. } => {
                            !output.trim().is_empty() && output != probe.prev
                        }
                        WakeKind::Once | WakeKind::Subscription { .. } => false,
                    };
                    let fired =
                        (fire && changed).then(|| wakes[index].fire("fired", Some(&output)));
                    if let WakeKind::Watch {
                        probe,
                        every,
                        until,
                    } = &mut wakes[index].kind
                    {
                        probe.prev = output;
                        wakes[index].next_at = (now + *every).min(*until);
                    }
                    return Some(fired);
                }
                let WakeKind::Watch { probe, until, .. } = &mut wakes[index].kind else {
                    return None;
                };
                Some(Some(match verdict {
                    Verdict::Wait { output } if now < *until => {
                        probe.prev = output;
                        return Some(None);
                    }
                    Verdict::Wait { output } => wakes[index].fire("gave up waiting", Some(&output)),
                    Verdict::Fire { output } => wakes[index].fire("fired", Some(&output)),
                    Verdict::Failed { exit, output } => wakes[index].fire(
                        &format!("stopped because its probe exited {}", exit.get()),
                        Some(&output),
                    ),
                }))
            },
        )?;
        let result = match fired {
            None => Resolved::Gone,
            Some(None) => Resolved::Rearmed,
            Some(Some(id)) => match store.delivery(id) {
                Some(delivery) => Resolved::Fired(Box::new(delivery)),
                None => Resolved::Gone,
            },
        };
        if digest {
            match &result {
                Resolved::Fired(_) => record_digest(self.id, WakeOperation::Fired, None),
                Resolved::Rearmed => record_digest(self.id, WakeOperation::Declined, Some("probe")),
                Resolved::Gone => {}
            }
        }
        Ok(result)
    }
}

#[derive(Default)]
pub(crate) struct Due {
    pub fired: Vec<Delivery>,
    pub ticks: Vec<Tick>,
    pub jobs: Vec<JobNotice>,
}

#[derive(Default)]
pub(crate) struct SubscriptionDelivery {
    pub(crate) matched: bool,
    pub(crate) fired: Vec<Delivery>,
}

struct Interrupted {
    key: ConversationKey,
    row: JobRow,
    offer: Offer,
}

struct State {
    wakes: Vec<Wake>,
    jobs: Vec<Interrupted>,
    next_id: u32,
}

pub(crate) struct WakeStore {
    durable: Option<Arc<DurableStore>>,
    bounds: WakeBounds,
    state: Mutex<State>,
    me: Weak<Self>,
}

impl WakeStore {
    #[cfg(test)]
    pub(crate) fn durable_for_test(&self) -> Option<&Arc<DurableStore>> {
        self.durable.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn in_memory(bounds: WakeBounds) -> Arc<Self> {
        Self::restored(None, Vec::new(), None, Vec::new(), bounds).0
    }

    pub(crate) fn restored(
        durable: Option<&Arc<DurableStore>>,
        rows: Vec<WakeRow>,
        reserved: Option<WakeId>,
        jobs: Vec<(ConversationKey, JobRow)>,
        bounds: WakeBounds,
    ) -> (Arc<Self>, Replay) {
        let mut wakes = Vec::new();
        for row in rows {
            let id = row.id;
            match Wake::from_row(row, Offer::InFlight) {
                Some(wake) => wakes.push(wake),
                None => tracing::warn!(
                    event = "gateway_wake_restore_skipped",
                    wake.id = %id,
                    reason = "invalid",
                    "gateway.wake.restore.skipped"
                ),
            }
        }
        let next_id = wakes
            .iter()
            .map(|wake| wake.id.0)
            .chain(reserved.map(|id| id.0))
            .max()
            .map_or(1, |id| id.wrapping_add(1));
        let jobs = jobs
            .into_iter()
            .map(|(key, row)| Interrupted {
                key,
                row,
                offer: Offer::InFlight,
            })
            .collect();
        let store = Arc::new_cyclic(|me| Self {
            durable: durable.cloned(),
            bounds,
            state: Mutex::new(State {
                wakes,
                jobs,
                next_id,
            }),
            me: Weak::clone(me),
        });
        let replay = {
            let state = store.state.lock();
            Replay {
                wakes: state
                    .wakes
                    .iter()
                    .filter_map(|wake| store.issue(wake))
                    .collect(),
                jobs: state.jobs.iter().map(|job| store.job_notice(job)).collect(),
            }
        };
        (store, replay)
    }

    fn issue(&self, wake: &Wake) -> Option<Delivery> {
        Some(Delivery {
            fired: wake.fired()?,
            ticket: DeliveryTicket::issue(
                wake.anchor.key(),
                Delivered::Wake(wake.id),
                self.me.upgrade(),
            ),
        })
    }

    fn job_notice(&self, job: &Interrupted) -> JobNotice {
        JobNotice {
            row: job.row.clone(),
            ticket: DeliveryTicket::issue(
                job.key.clone(),
                Delivered::Job(job.row.id),
                self.me.upgrade(),
            ),
        }
    }

    fn delivery(&self, id: WakeId) -> Option<Delivery> {
        let state = self.state.lock();
        let wake = state.wakes.iter().find(|wake| wake.id == id)?;
        self.issue(wake)
    }

    pub(crate) fn subscribe(
        &self,
        anchor: Anchor,
        target: PullRequestLinkTarget,
        now: SystemTime,
    ) -> Result<WakeSummary, WakeRefusal> {
        if anchor
            .as_space()
            .is_none_or(|space| space.subject() != anchor.subject())
        {
            return Err(WakeRefusal::MappedChannelRequired);
        }
        let maximum = self.bounds.max_subscriptions_per_agent;
        let expiry = self.bounds.subscription_expiry;
        let created = self.mutate_with_id(
            |_| WakeOperation::Expired,
            |wakes, next_id| {
                expire_subscriptions(wakes, now);
                if let Some(wake) = wakes.iter().find(|wake| {
                    wake.anchor.same_thread(&anchor)
                        && match &wake.kind {
                            WakeKind::Subscription { target: existing } => existing == &target,
                            WakeKind::Once | WakeKind::Watch { .. } => false,
                        }
                }) {
                    return Ok((wake.summary(now), false));
                }
                let pending = wakes
                    .iter()
                    .filter(|wake| {
                        wake.anchor.agent() == anchor.agent()
                            && match wake.kind {
                                WakeKind::Subscription { .. } => true,
                                WakeKind::Once | WakeKind::Watch { .. } => false,
                            }
                    })
                    .count();
                if pending >= maximum {
                    return Err(WakeRefusal::SubscriptionsFull { maximum });
                }
                let wake = Wake {
                    id: next_id.take(),
                    anchor,
                    note: format!("PR subscription {}#{}", target.repository, target.number),
                    next_at: now + expiry,
                    kind: WakeKind::Subscription { target },
                    run_as: Some(RunAs::Space),
                    pending: None,
                };
                let summary = wake.summary(now);
                wakes.push(wake);
                Ok((summary, true))
            },
        )??;
        let (summary, created) = created;
        if created {
            self.record_created(summary.id);
        }
        Ok(summary)
    }

    fn record_created(&self, id: WakeId) {
        if let Some(wake) = self.state.lock().wakes.iter().find(|wake| wake.id == id) {
            wake.record_lifecycle(WakeOperation::Created);
        }
    }

    pub(crate) fn list_subscriptions(
        &self,
        anchor: &Anchor,
        now: SystemTime,
    ) -> Result<Vec<WakeSummary>, WakeRefusal> {
        Ok(self.mutate(
            |_| WakeOperation::Expired,
            |wakes| {
                expire_subscriptions(wakes, now);
                wakes
                    .iter()
                    .filter(|wake| {
                        wake.anchor.same_channel(anchor)
                            && match wake.kind {
                                WakeKind::Subscription { .. } => true,
                                WakeKind::Once | WakeKind::Watch { .. } => false,
                            }
                    })
                    .map(|wake| wake.summary(now))
                    .collect()
            },
        )?)
    }

    pub(crate) fn cancel_subscription(
        &self,
        anchor: &Anchor,
        id: WakeId,
    ) -> Result<(), WakeRefusal> {
        let found = self.mutate(
            |_| WakeOperation::Cancelled,
            |wakes| {
                let before = wakes.len();
                wakes.retain(|wake| {
                    let remove = wake.id == id
                        && wake.anchor.same_channel(anchor)
                        && match wake.kind {
                            WakeKind::Subscription { .. } => true,
                            WakeKind::Once | WakeKind::Watch { .. } => false,
                        };
                    !remove
                });
                before != wakes.len()
            },
        )?;
        if found {
            Ok(())
        } else {
            Err(WakeRefusal::NotFound)
        }
    }

    pub(crate) fn matches_subscription(&self, agent: &AgentId, event: &GithubEvent) -> bool {
        self.state
            .lock()
            .wakes
            .iter()
            .any(|wake| subscription_matches(wake, agent, event))
    }

    pub(crate) fn deliver_subscriptions(
        &self,
        agent: &AgentId,
        event: &GithubEvent,
        now: SystemTime,
    ) -> Result<SubscriptionDelivery, StoreFailed> {
        let (matched, fired) = self.mutate(
            |wake| {
                if wake.next_at > now
                    && subscription_matches(wake, agent, event)
                    && event
                        .action
                        .as_ref()
                        .is_some_and(|action| action.as_str() == "closed")
                {
                    WakeOperation::Closed
                } else {
                    WakeOperation::Expired
                }
            },
            |wakes| {
                expire_subscriptions(wakes, now);
                let mut matched = false;
                let mut fired = Vec::new();
                wakes.retain_mut(|wake| {
                    if !subscription_matches(wake, agent, event) {
                        return true;
                    }
                    matched = true;
                    match event.action.as_ref().map(|action| action.as_str()) {
                        Some("closed") => false,
                        Some("synchronize") => {
                            match &mut wake.pending {
                                None => fired.push(wake.fire("fired", None)),
                                Some(pending) => match pending.offer {
                                    Offer::InFlight => {}
                                    Offer::At(_) => {
                                        pending.offer = Offer::InFlight;
                                        fired.push(wake.id);
                                    }
                                },
                            }
                            true
                        }
                        _ => true,
                    }
                });
                (matched, fired)
            },
        )?;
        let fired: Vec<Delivery> = fired
            .into_iter()
            .filter_map(|id| self.delivery(id))
            .collect();
        for delivery in &fired {
            if let WakeOrigin::Subscription(target) = &delivery.fired.origin {
                record_subscription(
                    delivery.fired.id,
                    &delivery.fired.anchor,
                    target,
                    WakeOperation::Fired,
                );
            }
        }
        Ok(SubscriptionDelivery { matched, fired })
    }

    pub(crate) const fn bounds(&self) -> WakeBounds {
        self.bounds
    }

    pub(crate) fn register(
        &self,
        anchor: Anchor,
        note: String,
        now: SystemTime,
        schedule: Schedule,
    ) -> Result<WakeSummary, WakeRefusal> {
        let run_as = match anchor.subject().principal_kind() {
            dekopon_core::PrincipalKind::Person
            | dekopon_core::PrincipalKind::Repository
            | dekopon_core::PrincipalKind::Service => None,
            dekopon_core::PrincipalKind::Space => {
                if anchor
                    .as_space()
                    .is_none_or(|space| space.subject() != anchor.subject())
                {
                    return Err(WakeRefusal::MappedChannelRequired);
                }
                Some(RunAs::Space)
            }
        };
        if let Schedule::Watch {
            probe,
            every,
            until,
        } = &schedule
        {
            if probe.purpose == WatchPurpose::Digest && run_as.is_none() {
                return Err(WakeRefusal::MappedChannelRequired);
            }
            let minimum = probe.minimum_interval(self.bounds.min_interval);
            if *every < minimum || every > until {
                return Err(WakeRefusal::Interval { minimum });
            }
            if *until > self.bounds.max_horizon {
                return Err(WakeRefusal::Horizon {
                    maximum: self.bounds.max_horizon,
                });
            }
        }
        let (next_at, kind) = match schedule {
            Schedule::Once { after } => (now + after, WakeKind::Once),
            Schedule::Watch {
                probe,
                every,
                until,
            } => (
                now + every,
                WakeKind::Watch {
                    probe,
                    every,
                    until: now + until,
                },
            ),
        };
        let maximum = self.bounds.max_per_subject;
        let summary = self.mutate_with_id(
            |_| WakeOperation::Expired,
            |wakes, next_id| {
                wakes.retain(|wake| !wake.expired_digest(now));
                let digest = match &kind {
                    WakeKind::Watch { probe, .. } => probe.purpose == WatchPurpose::Digest,
                    WakeKind::Once | WakeKind::Subscription { .. } => false,
                };
                if digest
                    && wakes
                        .iter()
                        .any(|wake| wake.is_digest() && wake.anchor.subject() == anchor.subject())
                {
                    return Err(WakeRefusal::DigestExists);
                }
                let pending = wakes
                    .iter()
                    .filter(|wake| {
                        wake.anchor.subject() == anchor.subject()
                            && match wake.kind {
                                WakeKind::Subscription { .. } => false,
                                WakeKind::Once | WakeKind::Watch { .. } => true,
                            }
                    })
                    .count();
                if pending >= maximum {
                    return Err(WakeRefusal::Full { maximum });
                }
                let wake = Wake {
                    id: next_id.take(),
                    anchor,
                    note,
                    next_at,
                    kind,
                    run_as,
                    pending: None,
                };
                let summary = wake.summary(now);
                wakes.push(wake);
                Ok(summary)
            },
        )??;
        self.record_created(summary.id);
        Ok(summary)
    }

    pub(crate) fn cancel(&self, subject: &Subject, id: WakeId) -> Result<(), WakeRefusal> {
        let found = self.mutate(
            |_| WakeOperation::Expired,
            |wakes| {
                let before = wakes.len();
                wakes.retain(|wake| {
                    wake.id != id
                        || wake.anchor.subject() != subject
                        || wake.run_as.is_some()
                        || match wake.kind {
                            WakeKind::Subscription { .. } => true,
                            WakeKind::Once | WakeKind::Watch { .. } => false,
                        }
                });
                wakes.len() != before
            },
        )?;
        if found {
            Ok(())
        } else {
            Err(WakeRefusal::NotFound)
        }
    }

    pub(crate) fn list(&self, subject: &Subject, now: SystemTime) -> Vec<WakeSummary> {
        let state = self.state.lock();
        state
            .wakes
            .iter()
            .filter(|wake| {
                wake.anchor.subject() == subject
                    && (wake.pending.is_none() || !wake.removed_once_delivered())
                    && wake.run_as.is_none()
                    && match wake.kind {
                        WakeKind::Subscription { .. } => false,
                        WakeKind::Once | WakeKind::Watch { .. } => true,
                    }
            })
            .map(|wake| wake.summary(now))
            .collect()
    }

    pub(crate) fn list_for(
        &self,
        person: &Anchor,
        space: Option<&Anchor>,
        now: SystemTime,
    ) -> Vec<WakeSummary> {
        let mut summaries = self.list(person.subject(), now);
        if let Some(space) = space {
            summaries.extend(
                self.state
                    .lock()
                    .wakes
                    .iter()
                    .filter(|wake| {
                        wake.run_as.is_some()
                            && wake.anchor.same_channel(space)
                            && !wake.expired_digest(now)
                            && (wake.pending.is_none() || !wake.removed_once_delivered())
                            && match wake.kind {
                                WakeKind::Once | WakeKind::Watch { .. } => true,
                                WakeKind::Subscription { .. } => false,
                            }
                    })
                    .map(|wake| wake.summary(now)),
            );
        }
        summaries
    }

    pub(crate) fn cancel_for(
        &self,
        person: &Anchor,
        space: Option<&Anchor>,
        id: WakeId,
    ) -> Result<(), WakeRefusal> {
        let Some(space) = space else {
            return self.cancel(person.subject(), id);
        };
        let found = self.mutate(
            |_| WakeOperation::Cancelled,
            |wakes| {
                let before = wakes.len();
                wakes.retain(|wake| {
                    let owned = match wake.run_as {
                        None => wake.anchor.subject() == person.subject(),
                        Some(RunAs::Space) => wake.anchor.same_channel(space),
                    };
                    let scheduled = match wake.kind {
                        WakeKind::Once | WakeKind::Watch { .. } => true,
                        WakeKind::Subscription { .. } => false,
                    };
                    !(wake.id == id && owned && scheduled)
                });
                before != wakes.len()
            },
        )?;
        if found {
            Ok(())
        } else {
            Err(WakeRefusal::NotFound)
        }
    }

    pub(crate) fn next_at(&self) -> Option<SystemTime> {
        let state = self.state.lock();
        let wakes = state.wakes.iter().filter_map(|wake| match &wake.pending {
            Some(pending) => pending.offer.at(),
            None => Some(wake.next_at),
        });
        let jobs = state.jobs.iter().filter_map(|job| job.offer.at());
        wakes.chain(jobs).min()
    }

    pub(crate) fn take_due(&self, now: SystemTime) -> Result<Due, StoreFailed> {
        let (fired, ticks) = self.mutate(
            |_| WakeOperation::Expired,
            |wakes| {
                let mut fired = Vec::new();
                let mut ticks = Vec::new();
                wakes.retain_mut(|wake| {
                    if let Some(pending) = &mut wake.pending {
                        if pending.offer.due(now) {
                            pending.offer = Offer::InFlight;
                            fired.push(wake.id);
                        }
                        return true;
                    }
                    if wake.expired_digest(now) {
                        return false;
                    }
                    if wake.next_at > now {
                        return true;
                    }
                    match &wake.kind {
                        WakeKind::Subscription { .. } => false,
                        WakeKind::Once => {
                            fired.push(wake.fire("fired", None));
                            true
                        }
                        WakeKind::Watch {
                            probe,
                            every,
                            until,
                        } => {
                            wake.next_at = match probe.purpose {
                                WatchPurpose::Digest => (now + *every).min(*until),
                                WatchPurpose::Ordinary => now + *every,
                            };
                            ticks.push(Tick {
                                id: wake.id,
                                anchor: wake.anchor.clone(),
                                probe: probe.clone(),
                            });
                            true
                        }
                    }
                });
                (fired, ticks)
            },
        )?;
        let jobs = {
            let mut state = self.state.lock();
            let mut due = Vec::new();
            for job in &mut state.jobs {
                if job.offer.due(now) {
                    job.offer = Offer::InFlight;
                    due.push(self.job_notice(job));
                }
            }
            due
        };
        Ok(Due {
            fired: fired
                .into_iter()
                .filter_map(|id| self.delivery(id))
                .collect(),
            ticks,
            jobs,
        })
    }

    pub(crate) fn settle(
        &self,
        ticket: DeliveryTicket,
        disposition: DeliveryDisposition,
    ) -> Result<(), StoreFailed> {
        let (conversation, delivered) = ticket.settled();
        let done = match disposition {
            DeliveryDisposition::Handled | DeliveryDisposition::Orphaned => true,
            DeliveryDisposition::Rejected => false,
        };
        if !done {
            self.reoffer(delivered, SystemTime::now());
            return Ok(());
        }
        match delivered {
            Delivered::Wake(id) => self.mutate(
                |_| WakeOperation::Fired,
                |wakes| {
                    if let Some(index) = wakes.iter().position(|wake| wake.id == id) {
                        if wakes[index].removed_once_delivered() {
                            wakes.remove(index);
                        } else {
                            wakes[index].pending = None;
                        }
                    }
                },
            ),
            Delivered::Job(id) => {
                let mut state = self.state.lock();
                let durable = state
                    .jobs
                    .iter()
                    .any(|job| job.row.id == id && job.key == conversation);
                if durable && let Some(store) = &self.durable {
                    store.commit(vec![(conversation.clone(), Change::JobEnded(id))])?;
                }
                state
                    .jobs
                    .retain(|job| !(job.row.id == id && job.key == conversation));
                Ok(())
            }
        }
    }

    pub(super) fn reoffer(&self, delivered: Delivered, now: SystemTime) {
        let at = Offer::At(now + REOFFER);
        let mut state = self.state.lock();
        match delivered {
            Delivered::Wake(id) => {
                if let Some(pending) = state
                    .wakes
                    .iter_mut()
                    .find(|wake| wake.id == id)
                    .and_then(|wake| wake.pending.as_mut())
                {
                    pending.offer = at;
                }
            }
            Delivered::Job(id) => {
                for job in state.jobs.iter_mut().filter(|job| job.row.id == id) {
                    job.offer = at;
                }
            }
        }
    }

    pub(crate) fn reserved_job_ids(&self) -> Option<JobId> {
        self.state
            .lock()
            .jobs
            .iter()
            .map(|job| job.row.id)
            .max_by_key(|id| id.get())
    }

    fn mutate<T>(
        &self,
        operation: impl Fn(&Wake) -> WakeOperation,
        change: impl FnOnce(&mut Vec<Wake>) -> T,
    ) -> Result<T, StoreFailed> {
        self.mutate_with_id(operation, |wakes, _| change(wakes))
    }

    fn mutate_with_id<T>(
        &self,
        operation: impl Fn(&Wake) -> WakeOperation,
        change: impl FnOnce(&mut Vec<Wake>, &mut NextId) -> T,
    ) -> Result<T, StoreFailed> {
        let mut state = self.state.lock();
        let mut wakes = state.wakes.clone();
        let mut next_id = NextId(state.next_id);
        let result = change(&mut wakes, &mut next_id);
        if let Some(store) = &self.durable {
            store.commit(changes(&state.wakes, &wakes))?;
        }
        let retained: std::collections::BTreeSet<_> = wakes.iter().map(|wake| wake.id).collect();
        for wake in &state.wakes {
            if !retained.contains(&wake.id) {
                wake.record_lifecycle(operation(wake));
            }
        }
        state.wakes = wakes;
        state.next_id = next_id.0;
        Ok(result)
    }
}

struct NextId(u32);

impl NextId {
    fn take(&mut self) -> WakeId {
        let id = WakeId(self.0);
        self.0 = self.0.wrapping_add(1);
        id
    }
}

fn changes(before: &[Wake], after: &[Wake]) -> Vec<(ConversationKey, Change)> {
    let encoded = |wake: &Wake| serde_json::to_value(wake.row()).ok();
    let previous: BTreeMap<WakeId, (&Wake, Option<serde_json::Value>)> = before
        .iter()
        .filter(|wake| wake.durable())
        .map(|wake| (wake.id, (wake, encoded(wake))))
        .collect();
    let mut changes = Vec::new();
    let mut kept = std::collections::BTreeSet::new();
    for wake in after.iter().filter(|wake| wake.durable()) {
        kept.insert(wake.id);
        let row = wake.row();
        let unchanged = previous
            .get(&wake.id)
            .is_some_and(|(_, old)| old.is_some() && *old == serde_json::to_value(&row).ok());
        if !unchanged {
            changes.push((wake.anchor.key(), Change::PutWake(row)));
        }
    }
    for (id, (wake, _)) in previous {
        if !kept.contains(&id) {
            changes.push((wake.anchor.key(), Change::RemoveWake(id)));
        }
    }
    changes
}

fn at(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}

fn record_subscription(
    id: WakeId,
    anchor: &Anchor,
    target: &PullRequestLinkTarget,
    operation: WakeOperation,
) {
    tracing::info!(
        audit.event = "gateway.admission", admission.trigger = "pr-subscription",
        subscription.operation = operation.as_str(), wake.id = %id,
        github.repository = target.repository.as_str(), github.number = %target.number,
        agent = %anchor.agent(), conversation.id = %anchor.conversation().key(),
        "gateway.admission"
    );
}

fn expire_subscriptions(wakes: &mut Vec<Wake>, now: SystemTime) {
    wakes.retain(|wake| match wake.kind {
        WakeKind::Subscription { .. } if wake.next_at <= now && wake.pending.is_none() => false,
        WakeKind::Subscription { .. } | WakeKind::Once | WakeKind::Watch { .. } => true,
    });
}

fn subscription_matches(wake: &Wake, agent: &AgentId, event: &GithubEvent) -> bool {
    if event.event != GithubEventName::PullRequest || wake.anchor.agent() != agent {
        return false;
    }
    match (&wake.kind, event.thread()) {
        (WakeKind::Subscription { target }, GithubThread::Number(number)) => {
            target.repository == event.repository.full_name && target.number == number
        }
        (WakeKind::Subscription { .. }, GithubThread::Repository)
        | (WakeKind::Once | WakeKind::Watch { .. }, _) => false,
    }
}
