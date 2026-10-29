use parking_lot::Mutex;
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_agent::wake::{WakeId, WakeRefusal, WakeSummary, WatchSummary};
use dekopon_broker_protocol::{ChatTransportKind, Conversation, TraceParent};
use dekopon_core::github::{GithubEvent, GithubEventName, GithubThread};
use dekopon_core::{AgentId, ExternalSubject};
use dekopon_shell::{CapabilityInvoker, Interpreter};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{Anchor, Probe, Verdict, fired_text};
use crate::{
    config::{ResolvedWakes, WakeBounds},
    pull_request_links::PullRequestLinkTarget,
    transport::{InboundMessage, ReplyTarget},
};

#[derive(Debug, Error)]
pub enum WakeStoreError {
    #[error("could not read or write the wake store")]
    Io { kind: io::ErrorKind },
    #[error("wake store line {line} did not parse; remove the file to reset every pending wake")]
    Corrupt { line: usize },
}

impl From<io::Error> for WakeStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}

impl From<WakeStoreError> for WakeRefusal {
    fn from(error: WakeStoreError) -> Self {
        tracing::error!(
            event = "gateway_wake_store_failed",
            error = %error,
            "gateway.wake.store.failed"
        );
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
}

#[derive(Clone, Copy)]
enum SubscriptionOperation {
    Created,
    Fired,
    Expired,
    Cancelled,
}

impl SubscriptionOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Fired => "fired",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
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

    fn fired(&self, reason: &str, output: Option<&str>) -> Fired {
        Fired {
            anchor: self.anchor.clone(),
            id: self.id,
            text: fired_text(self.id, &self.note, reason, output),
            notice: match output {
                Some(output) => format!("{}\n\n{output}", self.note),
                None => self.note.clone(),
            },
            subscription: match &self.kind {
                WakeKind::Subscription { target } => Some(target.clone()),
                WakeKind::Once | WakeKind::Watch { .. } => None,
            },
        }
    }

    fn record_subscription(&self, operation: SubscriptionOperation) {
        if let WakeKind::Subscription { target } = &self.kind {
            record_subscription(self.id, &self.anchor, target, operation);
        }
    }
}

pub(crate) struct Fired {
    anchor: Anchor,
    id: WakeId,
    text: String,
    notice: String,
    subscription: Option<PullRequestLinkTarget>,
}

impl Fired {
    pub(crate) fn is_subscription(&self) -> bool {
        self.subscription.is_some()
    }
    pub(crate) const fn id(&self) -> WakeId {
        self.id
    }

    pub(crate) const fn agent(&self) -> &AgentId {
        self.anchor.agent()
    }

    pub(crate) fn into_inbound(self) -> InboundMessage {
        let text = match &self.subscription {
            Some(target) => format!(
                "[gateway: PR subscription wake] Re-review https://github.com/{}/pull/{} at its current head using granted tools. Read the prior review and current diff; submit a fresh head-pinned COMMENT review. Decide whether the blocking findings changed. After successfully refreshing this PR's review, reply in this Slack thread only if they changed; otherwise call decline_chat_reply. Capability work without a successful refresh requires a Slack report. This turn runs as the space alone; no person's grants are available.",
                target.repository, target.number
            ),
            None => self.text,
        };
        self.anchor
            .inbound(self.id, text, self.notice, self.subscription)
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
    Fired(Box<Fired>),
    Gone,
}

impl Tick {
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
    ) -> Result<Resolved, WakeStoreError> {
        store.mutate(SubscriptionOperation::Expired, |wakes| {
            let Some(index) = wakes.iter().position(|wake| wake.id == self.id) else {
                return Ok(Resolved::Gone);
            };
            let WakeKind::Watch { probe, until, .. } = &mut wakes[index].kind else {
                return Ok(Resolved::Gone);
            };
            let fired = match verdict {
                Verdict::Wait { output } if now < *until => {
                    probe.prev = output;
                    return Ok(Resolved::Rearmed);
                }
                Verdict::Wait { output } => wakes[index].fired("gave up waiting", Some(&output)),
                Verdict::Fire { output } => wakes[index].fired("fired", Some(&output)),
                Verdict::Failed { exit, output } => wakes[index].fired(
                    &format!("stopped because its probe exited {}", exit.get()),
                    Some(&output),
                ),
            };
            wakes.remove(index);
            Ok(Resolved::Fired(Box::new(fired)))
        })
    }
}

#[derive(Default)]
pub(crate) struct Due {
    pub fired: Vec<Fired>,
    pub ticks: Vec<Tick>,
}

#[derive(Default)]
pub(crate) struct SubscriptionDelivery {
    pub(crate) matched: bool,
    pub(crate) fired: Vec<Fired>,
}

struct State {
    wakes: Vec<Wake>,
    next_id: u32,
}

pub(crate) struct WakeStore {
    path: PathBuf,
    bounds: WakeBounds,
    state: Mutex<State>,
}

impl WakeStore {
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
        let mut state = self.state.lock();
        let mut wakes = state.wakes.clone();
        expire_subscriptions(&mut wakes, now);
        if let Some(wake) = wakes.iter().find(|wake| {
            wake.anchor.same_thread(&anchor)
                && match &wake.kind {
                    WakeKind::Subscription { target: existing } => existing == &target,
                    WakeKind::Once | WakeKind::Watch { .. } => false,
                }
        }) {
            return Ok(wake.summary(now));
        }
        let maximum = self.bounds.max_subscriptions_per_agent;
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
            id: WakeId(state.next_id),
            anchor,
            note: format!("PR subscription {}#{}", target.repository, target.number),
            next_at: now + self.bounds.subscription_expiry,
            kind: WakeKind::Subscription { target },
        };
        let summary = wake.summary(now);
        wakes.push(wake);
        write(&self.path, &wakes)?;
        let retained: std::collections::BTreeSet<_> = wakes.iter().map(|wake| wake.id).collect();
        for removed in &state.wakes {
            if !retained.contains(&removed.id) {
                removed.record_subscription(SubscriptionOperation::Expired);
            }
        }
        if let Some(wake) = wakes.last() {
            wake.record_subscription(SubscriptionOperation::Created);
        }
        state.wakes = wakes;
        state.next_id = state.next_id.wrapping_add(1);
        Ok(summary)
    }

    pub(crate) fn list_subscriptions(
        &self,
        anchor: &Anchor,
        now: SystemTime,
    ) -> Result<Vec<WakeSummary>, WakeRefusal> {
        Ok(self.mutate(SubscriptionOperation::Expired, |wakes| {
            expire_subscriptions(wakes, now);
            Ok(wakes
                .iter()
                .filter(|wake| {
                    wake.anchor.same_channel(anchor)
                        && match wake.kind {
                            WakeKind::Subscription { .. } => true,
                            WakeKind::Once | WakeKind::Watch { .. } => false,
                        }
                })
                .map(|wake| wake.summary(now))
                .collect())
        })?)
    }

    pub(crate) fn cancel_subscription(
        &self,
        anchor: &Anchor,
        id: WakeId,
    ) -> Result<(), WakeRefusal> {
        let found = self.mutate(SubscriptionOperation::Cancelled, |wakes| {
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
            Ok(before != wakes.len())
        })?;
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
    ) -> Result<SubscriptionDelivery, WakeStoreError> {
        let delivery = self.mutate(SubscriptionOperation::Expired, |wakes| {
            expire_subscriptions(wakes, now);
            let mut delivery = SubscriptionDelivery::default();
            wakes.retain(|wake| {
                if !subscription_matches(wake, agent, event) {
                    return true;
                }
                delivery.matched = true;
                match event.action.as_ref().map(|action| action.as_str()) {
                    Some("closed") => false,
                    Some("synchronize") => {
                        delivery.fired.push(wake.fired("fired", None));
                        true
                    }
                    _ => true,
                }
            });
            Ok(delivery)
        })?;
        for fired in &delivery.fired {
            if let Some(target) = &fired.subscription {
                record_subscription(
                    fired.id,
                    &fired.anchor,
                    target,
                    SubscriptionOperation::Fired,
                );
            }
        }
        Ok(delivery)
    }
    pub(crate) fn open(config: &ResolvedWakes) -> Result<Self, WakeStoreError> {
        if let Some(parent) = config.path.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let wakes = match File::open(&config.path) {
            Ok(file) => read(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let next_id = wakes
            .iter()
            .map(|wake| wake.id.0)
            .max()
            .map_or(1, |id| id.wrapping_add(1));
        Ok(Self {
            path: config.path.clone(),
            bounds: config.bounds,
            state: Mutex::new(State { wakes, next_id }),
        })
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
        let mut state = self.state.lock();
        let pending = state
            .wakes
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
            id: WakeId(state.next_id),
            anchor,
            note,
            next_at,
            kind,
        };
        let summary = wake.summary(now);
        let mut wakes = state.wakes.clone();
        wakes.push(wake);
        write(&self.path, &wakes)?;
        state.wakes = wakes;
        state.next_id = state.next_id.wrapping_add(1);
        Ok(summary)
    }

    pub(crate) fn cancel(&self, subject: &ExternalSubject, id: WakeId) -> Result<(), WakeRefusal> {
        let found = self.mutate(SubscriptionOperation::Expired, |wakes| {
            let before = wakes.len();
            wakes.retain(|wake| {
                wake.id != id
                    || wake.anchor.subject() != subject
                    || match wake.kind {
                        WakeKind::Subscription { .. } => true,
                        WakeKind::Once | WakeKind::Watch { .. } => false,
                    }
            });
            Ok(wakes.len() != before)
        })?;
        if found {
            Ok(())
        } else {
            Err(WakeRefusal::NotFound)
        }
    }

    pub(crate) fn list(&self, subject: &ExternalSubject, now: SystemTime) -> Vec<WakeSummary> {
        let state = self.state.lock();
        state
            .wakes
            .iter()
            .filter(|wake| {
                wake.anchor.subject() == subject
                    && match wake.kind {
                        WakeKind::Subscription { .. } => false,
                        WakeKind::Once | WakeKind::Watch { .. } => true,
                    }
            })
            .map(|wake| wake.summary(now))
            .collect()
    }

    pub(crate) fn next_at(&self) -> Option<SystemTime> {
        let state = self.state.lock();
        state.wakes.iter().map(|wake| wake.next_at).min()
    }

    /// A due watch is leased by moving its next check forward in the same write, so a slow tick
    /// can never be leased twice.
    pub(crate) fn take_due(&self, now: SystemTime) -> Result<Due, WakeStoreError> {
        self.mutate(SubscriptionOperation::Expired, |wakes| {
            let mut due = Due::default();
            wakes.retain_mut(|wake| {
                if wake.next_at > now {
                    return true;
                }
                match &wake.kind {
                    WakeKind::Subscription { .. } => false,
                    WakeKind::Once => {
                        due.fired.push(wake.fired("fired", None));
                        false
                    }
                    WakeKind::Watch { probe, every, .. } => {
                        wake.next_at = now + *every;
                        due.ticks.push(Tick {
                            id: wake.id,
                            anchor: wake.anchor.clone(),
                            probe: probe.clone(),
                        });
                        true
                    }
                }
            });
            Ok(due)
        })
    }

    fn mutate<T>(
        &self,
        operation: SubscriptionOperation,
        change: impl FnOnce(&mut Vec<Wake>) -> Result<T, WakeStoreError>,
    ) -> Result<T, WakeStoreError> {
        let mut state = self.state.lock();
        let mut wakes = state.wakes.clone();
        let result = change(&mut wakes)?;
        write(&self.path, &wakes)?;
        let retained: std::collections::BTreeSet<_> = wakes.iter().map(|wake| wake.id).collect();
        for wake in &state.wakes {
            if !retained.contains(&wake.id) {
                wake.record_subscription(operation);
            }
        }
        state.wakes = wakes;
        Ok(result)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Line {
    id: u32,
    transport: String,
    kind: ChatTransportKind,
    subject: ExternalSubject,
    conversation: Conversation,
    reply: ReplyTarget,
    agent: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheduled_in: Option<TraceParent>,
    note: String,
    next_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch: Option<WatchLine>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subscription: Option<PullRequestLinkTarget>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WatchLine {
    script: String,
    prev: String,
    every_ms: u64,
    until_ms: u64,
}

fn millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

fn at(millis: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(millis)
}

impl From<&Wake> for Line {
    fn from(wake: &Wake) -> Self {
        let anchor = &wake.anchor;
        Self {
            id: wake.id.0,
            transport: anchor.transport.to_string(),
            kind: anchor.kind,
            subject: anchor.subject.clone(),
            conversation: anchor.conversation.clone(),
            reply: anchor.reply.clone(),
            agent: anchor.agent.clone(),
            scheduled_in: anchor.scheduled_in,
            note: wake.note.clone(),
            next_at_ms: millis(wake.next_at),
            watch: match &wake.kind {
                WakeKind::Once | WakeKind::Subscription { .. } => None,
                WakeKind::Watch {
                    probe,
                    every,
                    until,
                } => Some(WatchLine {
                    script: probe.script.clone(),
                    prev: probe.prev.clone(),
                    every_ms: u64::try_from(every.as_millis()).unwrap_or(u64::MAX),
                    until_ms: millis(*until),
                }),
            },
            subscription: match &wake.kind {
                WakeKind::Subscription { target } => Some(target.clone()),
                WakeKind::Once | WakeKind::Watch { .. } => None,
            },
        }
    }
}

impl Line {
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn wake(self) -> Option<Wake> {
        if self.subscription.is_some() && self.watch.is_some() {
            return None;
        }
        let wake = Wake {
            id: WakeId(self.id),
            anchor: Anchor {
                transport: self.transport.parse().ok()?,
                reply: match self.reply {
                    ReplyTarget::Local { .. } => return None,
                    reply => reply,
                },
                kind: self.kind,
                subject: self.subject,
                conversation: self.conversation,
                agent: self.agent,
                scheduled_in: self.scheduled_in,
            },
            note: self.note,
            next_at: at(self.next_at_ms),
            kind: match (self.watch, self.subscription) {
                (None, None) => WakeKind::Once,
                (None, Some(target)) => WakeKind::Subscription { target },
                (Some(_), Some(_)) => return None,
                (Some(watch), None) => WakeKind::Watch {
                    probe: Probe {
                        script: watch.script,
                        prev: watch.prev,
                    },
                    every: Duration::from_millis(watch.every_ms),
                    until: at(watch.until_ms),
                },
            },
        };
        match &wake.kind {
            WakeKind::Subscription { .. } => {
                let space = wake.anchor.as_space()?;
                if space.subject() != wake.anchor.subject()
                    || !wake
                        .anchor
                        .conversation
                        .is_canonical_for(wake.anchor.kind, wake.anchor.subject())
                {
                    return None;
                }
            }
            WakeKind::Once | WakeKind::Watch { .. } => {}
        }
        Some(wake)
    }
}

fn record_subscription(
    id: WakeId,
    anchor: &Anchor,
    target: &PullRequestLinkTarget,
    operation: SubscriptionOperation,
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
        WakeKind::Subscription { .. } if wake.next_at <= now => false,
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

fn read(file: File) -> Result<Vec<Wake>, WakeStoreError> {
    let mut wakes = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        wakes.push(
            serde_json::from_str::<Line>(&line)
                .ok()
                .and_then(Line::wake)
                .ok_or(WakeStoreError::Corrupt { line: index + 1 })?,
        );
    }
    Ok(wakes)
}

fn write(path: &Path, wakes: &[Wake]) -> Result<(), WakeStoreError> {
    let temporary = path.with_extension("next");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    for wake in wakes {
        let mut encoded = serde_json::to_vec(&Line::from(wake)).map_err(io::Error::from)?;
        encoded.push(b'\n');
        file.write_all(&encoded)?;
    }
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}
