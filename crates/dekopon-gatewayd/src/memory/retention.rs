use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, SystemTime},
};

use dekopon_broker_protocol::{ChatReclaim, HistoryUsage};
use dekopon_core::history::HistoryKey;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::{Notify, OwnedMutexGuard};

use super::journal::{HistoryChat, Journal, JournalError, millis};
use crate::config::ResolvedBroker;

// Bounds one admission or expiry pass however activity moves while it runs.
const RECLAIMS_PER_PASS: usize = 64;
// Boot runs a full expiry pass; after that a turn's persistence runs one at most this often.
const EXPIRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Error)]
pub(crate) enum HistoryError {
    #[error("a history write of {need} bytes exceeds the {budget}-byte chat-history budget")]
    TooLarge { need: u64, budget: u64 },
    #[error("chat history is over its budget and no chat can be reclaimed")]
    Exhausted,
    #[error("the broker did not answer a chat-history request")]
    Broker,
    #[error("the conversation journal failed")]
    Journal(#[from] JournalError),
    #[error("a chat-history task stopped")]
    Task,
    #[error("the chat clock could not be published before a history write")]
    Clock,
    #[error("a chat has an unfinished reclaim and cannot be used until it completes")]
    Pending,
    #[error("an unfinished reclaim could not complete because part of the chat is in use")]
    ReplayBlocked,
    #[error("chat history needs a conversation journal to keep its budget and clock")]
    Unavailable,
    #[error("the broker holds chat history it cannot measure, so the budget is unknown")]
    Unmeasured,
}

impl HistoryError {
    pub(crate) const fn category(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "history-too-large",
            Self::Exhausted => "history-exhausted",
            Self::Broker => "history-broker",
            Self::Journal(_) => "history-journal",
            Self::Task => "history-task",
            Self::Clock => "history-clock",
            Self::Pending => "history-pending",
            Self::ReplayBlocked => "history-replay-blocked",
            Self::Unavailable => "history-unavailable",
            Self::Unmeasured => "history-unmeasured",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Reclaimed {
    Chat,
    Busy,
    Active,
}

// History growth counts against the whole budget; broker growth must also fit the broker's root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Growth {
    pub history: u64,
    pub broker: u64,
}

impl Growth {
    pub(crate) const fn journal(bytes: u64) -> Self {
        Self {
            history: bytes,
            broker: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Decide,
    Replay,
}

#[derive(Clone, Debug)]
enum Candidate {
    Chat(HistoryChat),
    Unlinked { key: HistoryKey, activity_ms: u64 },
}

impl Candidate {
    const fn activity_ms(&self) -> u64 {
        match self {
            Self::Chat(chat) => chat.activity_ms,
            Self::Unlinked { activity_ms, .. } => *activity_ms,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Chat(chat) => &chat.stem,
            Self::Unlinked { key, .. } => key.as_str(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Reason {
    Idle { cutoff_ms: u64 },
    Budget,
}

impl Reason {
    const fn label(self) -> &'static str {
        match self {
            Self::Idle { .. } => "idle",
            Self::Budget => "budget",
        }
    }
}

#[derive(Clone, Copy)]
enum Claim {
    Used(NonZeroUsize),
    Reclaiming,
}

#[derive(Default)]
struct Claims {
    stems: Mutex<HashMap<String, Claim>>,
    changed: Notify,
}

impl Claims {
    fn release(&self, stem: &str, reclaim: bool) {
        let mut stems = self.stems.lock();
        match (stems.get(stem).copied(), reclaim) {
            (Some(Claim::Reclaiming), true) => {
                stems.remove(stem);
            }
            (Some(Claim::Used(count)), false) => match NonZeroUsize::new(count.get() - 1) {
                Some(count) => {
                    stems.insert(stem.to_owned(), Claim::Used(count));
                }
                None => {
                    stems.remove(stem);
                }
            },
            (Some(Claim::Reclaiming | Claim::Used(_)) | None, _) => {}
        }
        drop(stems);
        self.changed.notify_waiters();
    }
}

#[must_use = "a chat is protected only while its use is held"]
pub(crate) struct HistoryUse {
    claims: Arc<Claims>,
    stem: String,
}

impl Drop for HistoryUse {
    fn drop(&mut self) {
        self.claims.release(&self.stem, false);
    }
}

struct ReclaimClaim {
    claims: Arc<Claims>,
    stem: String,
}

impl Drop for ReclaimClaim {
    fn drop(&mut self) {
        self.claims.release(&self.stem, true);
    }
}

pub(crate) struct HistoryRetention {
    journal: Arc<Journal>,
    broker: ResolvedBroker,
    budget: u64,
    idle: Duration,
    admission: Arc<tokio::sync::Mutex<()>>,
    claims: Arc<Claims>,
    // Broker recordings whose outcome never arrived: their whole bound stays spent until the broker
    // reports none in flight, across gateway restarts.
    indeterminate: Mutex<u64>,
    expired_at: Mutex<std::time::Instant>,
}

#[must_use = "admission serializes history writes only while it is held"]
pub(crate) struct HistoryAdmission {
    retention: Arc<HistoryRetention>,
    _turn: OwnedMutexGuard<()>,
}

// Holds admission across one broker recording; dropped unsettled, its bound stays debited.
#[must_use = "a recording's allowance is released only by settling its outcome"]
pub(crate) struct RecordTicket {
    _admission: HistoryAdmission,
    retention: Arc<HistoryRetention>,
    bound: u64,
    settled: bool,
}

impl RecordTicket {
    pub(crate) async fn answered(mut self) {
        self.settled = true;
        let journal = Arc::clone(&self.retention.journal);
        let remaining = *self.retention.indeterminate.lock();
        // A debit that cannot be lowered stays persisted, which only overstates use.
        if let Err(error) = blocking(move || journal.write_debit(remaining)).await {
            tracing::warn!(
                event = "gateway_history_debit_failed",
                category = error.category(),
                "gateway.history.debit.failed"
            );
        }
    }
}

impl Drop for RecordTicket {
    fn drop(&mut self) {
        if !self.settled {
            let mut indeterminate = self.retention.indeterminate.lock();
            *indeterminate = indeterminate.saturating_add(self.bound);
        }
    }
}

impl HistoryRetention {
    pub(crate) fn new(
        journal: Arc<Journal>,
        broker: ResolvedBroker,
        budget: u64,
        idle: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            journal,
            broker,
            budget,
            idle,
            admission: Arc::new(tokio::sync::Mutex::new(())),
            claims: Arc::default(),
            indeterminate: Mutex::new(0),
            expired_at: Mutex::new(std::time::Instant::now()),
        })
    }

    // A chat with a published reclaim is finished before anyone may use it again.
    pub(crate) async fn begin_use(&self, stem: &str) -> Result<HistoryUse, HistoryError> {
        loop {
            let changed = self.claims.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let pending = {
                let mut stems = self.claims.stems.lock();
                match stems.get(stem).copied() {
                    Some(Claim::Reclaiming) => None,
                    claim => {
                        if self.journal.reclaiming(stem) {
                            stems.insert(stem.to_owned(), Claim::Reclaiming);
                            Some(ReclaimClaim {
                                claims: Arc::clone(&self.claims),
                                stem: stem.to_owned(),
                            })
                        } else {
                            let count = match claim {
                                Some(Claim::Used(count)) => count.saturating_add(1),
                                Some(Claim::Reclaiming) | None => NonZeroUsize::MIN,
                            };
                            stems.insert(stem.to_owned(), Claim::Used(count));
                            return Ok(HistoryUse {
                                claims: Arc::clone(&self.claims),
                                stem: stem.to_owned(),
                            });
                        }
                    }
                }
            };
            match pending {
                Some(claim) => match self.finish(claim, Mode::Replay).await {
                    Ok(Reclaimed::Chat) => {}
                    Ok(Reclaimed::Busy | Reclaimed::Active) | Err(_) => {
                        return Err(HistoryError::Pending);
                    }
                },
                None => changed.await,
            }
        }
    }

    fn try_reclaim(&self, stem: &str) -> Option<ReclaimClaim> {
        let mut stems = self.claims.stems.lock();
        if stems.contains_key(stem) {
            return None;
        }
        stems.insert(stem.to_owned(), Claim::Reclaiming);
        Some(ReclaimClaim {
            claims: Arc::clone(&self.claims),
            stem: stem.to_owned(),
        })
    }

    pub(crate) async fn admit(self: &Arc<Self>) -> HistoryAdmission {
        HistoryAdmission {
            retention: Arc::clone(self),
            _turn: Arc::clone(&self.admission).lock_owned().await,
        }
    }

    // A broker recording proceeds only once its whole bound fits and the chat's clock is
    // durably ahead of it.
    pub(crate) async fn begin_record(
        self: &Arc<Self>,
        stem: &str,
        at: SystemTime,
    ) -> Result<RecordTicket, HistoryError> {
        let admission = self.admit().await;
        if self.journal.reclaiming(stem) {
            return Err(HistoryError::Pending);
        }
        let bound = self.usage(Vec::new()).await?.record_bound;
        admission
            .ensure(
                stem,
                Growth {
                    history: bound.saturating_add(super::journal::SIDECAR_BYTES),
                    broker: bound,
                },
            )
            .await?;
        let journal = Arc::clone(&self.journal);
        let stem = stem.to_owned();
        blocking(move || journal.note_activity(&stem, at))
            .await
            .map_err(|_unpublished| HistoryError::Clock)?;
        let debit = self.indeterminate.lock().saturating_add(bound);
        let journal = Arc::clone(&self.journal);
        blocking(move || journal.write_debit(debit)).await?;
        Ok(RecordTicket {
            _admission: admission,
            retention: Arc::clone(self),
            bound,
            settled: false,
        })
    }

    // Boot replays every intent before any session can claim its chat.
    pub(crate) async fn recover(&self) -> Result<usize, HistoryError> {
        let journal = Arc::clone(&self.journal);
        let debit = blocking(move || journal.debit()).await?;
        *self.indeterminate.lock() = debit;
        let journal = Arc::clone(&self.journal);
        let adopted = blocking(move || journal.adopt_history()).await?;
        if adopted > 0 {
            tracing::info!(
                event = "gateway_history_adopted",
                history.chats = adopted,
                "gateway.history.adopted"
            );
        }
        let journal = Arc::clone(&self.journal);
        let intents = blocking(move || journal.reclaim_intents()).await?;
        let mut finished = 0;
        for stem in intents {
            let Some(claim) = self.try_reclaim(&stem) else {
                continue;
            };
            if self.finish(claim, Mode::Replay).await? == Reclaimed::Chat {
                finished += 1;
            }
        }
        Ok(finished)
    }

    pub(crate) async fn expire_due(&self, now: SystemTime) -> Result<usize, HistoryError> {
        {
            let mut expired_at = self.expired_at.lock();
            if expired_at.elapsed() < EXPIRY_INTERVAL {
                return Ok(0);
            }
            *expired_at = std::time::Instant::now();
        }
        self.expire(now).await
    }

    pub(crate) async fn expire(&self, now: SystemTime) -> Result<usize, HistoryError> {
        let _turn = self.admission.lock().await;
        let cutoff_ms =
            millis(now).saturating_sub(u64::try_from(self.idle.as_millis()).unwrap_or(u64::MAX));
        let mut expired = 0;
        let mut skipped = HashSet::new();
        for _ in 0..RECLAIMS_PER_PASS {
            let Some(candidate) = self.oldest(None, &skipped).await? else {
                break;
            };
            if candidate.activity_ms() >= cutoff_ms {
                break;
            }
            if self.reclaim(&candidate, Reason::Idle { cutoff_ms }).await? == Reclaimed::Chat {
                expired += 1;
            } else {
                skipped.insert(candidate.name().to_owned());
            }
        }
        Ok(expired)
    }

    // The least recently active of the gateway's chats and the broker's unlinked units.
    async fn oldest(
        &self,
        admitting: Option<&str>,
        skipped: &HashSet<String>,
    ) -> Result<Option<Candidate>, HistoryError> {
        let journal = Arc::clone(&self.journal);
        let mut excluded = skipped.clone();
        if let Some(stem) = admitting {
            excluded.insert(stem.to_owned());
        }
        let chat = blocking(move || journal.oldest_chat(&excluded))
            .await?
            .map(Candidate::Chat);
        let skip = skipped
            .iter()
            .filter_map(|name| HistoryKey::new(name.clone()).ok())
            .collect();
        let unlinked =
            self.usage(skip)
                .await?
                .oldest_unlinked
                .map(|unlinked| Candidate::Unlinked {
                    key: unlinked.history,
                    activity_ms: unlinked.activity_ms,
                });
        Ok(match (chat, unlinked) {
            (Some(chat), Some(unlinked)) if unlinked.activity_ms() < chat.activity_ms() => {
                Some(unlinked)
            }
            (Some(chat), _) => Some(chat),
            (None, unlinked) => unlinked,
        })
    }

    async fn usage(&self, skip: Vec<HistoryKey>) -> Result<HistoryUsage, HistoryError> {
        let client = self
            .broker
            .leg_client()
            .map_err(|_unreachable| HistoryError::Broker)?;
        client
            .history_usage(skip)
            .await
            .map_err(|_failed| HistoryError::Broker)
    }

    async fn reclaim(
        &self,
        candidate: &Candidate,
        reason: Reason,
    ) -> Result<Reclaimed, HistoryError> {
        let chat = match candidate {
            Candidate::Unlinked { key, .. } => return self.reclaim_unlinked(key).await,
            Candidate::Chat(chat) => chat,
        };
        let Some(claim) = self.try_reclaim(&chat.stem) else {
            return Ok(Reclaimed::Busy);
        };
        // An earlier decision whose deletion stopped part way is finished, never re-decided.
        if self.journal.reclaiming(&chat.stem) {
            return match self.finish(claim, Mode::Replay).await {
                Err(HistoryError::ReplayBlocked) => Ok(Reclaimed::Busy),
                outcome => outcome,
            };
        }
        let journal = Arc::clone(&self.journal);
        let stem = chat.stem.clone();
        let activity_ms = blocking(move || journal.activity(&stem))
            .await?
            .unwrap_or(0);
        let active = match reason {
            Reason::Idle { cutoff_ms } => activity_ms >= cutoff_ms,
            Reason::Budget => activity_ms > chat.activity_ms,
        };
        if active {
            return Ok(Reclaimed::Active);
        }
        // The claim rides with the publication, so an aborted caller cannot free the chat for a
        // new turn while an intent that would delete that turn is still being written.
        let journal = Arc::clone(&self.journal);
        let claim = blocking(move || {
            journal.write_reclaim_intent(&claim.stem)?;
            Ok(claim)
        })
        .await?;
        let outcome = self.finish(claim, Mode::Decide).await;
        if matches!(outcome, Ok(Reclaimed::Chat)) {
            tracing::info!(
                event = "gateway_history_reclaimed",
                reason = reason.label(),
                history.activity_ms = activity_ms,
                "gateway.history.reclaimed"
            );
        }
        outcome
    }

    async fn reclaim_unlinked(&self, key: &HistoryKey) -> Result<Reclaimed, HistoryError> {
        let Some(_claim) = self.try_reclaim(key.as_str()) else {
            return Ok(Reclaimed::Busy);
        };
        match self.broker_reclaim(key.clone()).await? {
            ChatReclaim::Reclaimed { .. } => {
                tracing::info!(
                    event = "gateway_history_reclaimed",
                    reason = "unlinked",
                    "gateway.history.reclaimed"
                );
                Ok(Reclaimed::Chat)
            }
            ChatReclaim::Busy => Ok(Reclaimed::Busy),
        }
    }

    async fn broker_reclaim(&self, key: HistoryKey) -> Result<ChatReclaim, HistoryError> {
        let client = self
            .broker
            .leg_client()
            .map_err(|_unreachable| HistoryError::Broker)?;
        client
            .reclaim_chat(key)
            .await
            .map_err(|_failed| HistoryError::Broker)
    }

    // The broker copy goes first, then the transcript, then the intent: a crash between any two
    // steps leaves the intent, and replaying it finishes the same steps. Until the intent is gone
    // the chat stays claimed by the intent itself, whatever this call returns.
    async fn finish(&self, claim: ReclaimClaim, mode: Mode) -> Result<Reclaimed, HistoryError> {
        let key = HistoryKey::new(claim.stem.clone()).map_err(|_invalid| JournalError::Corrupt)?;
        match (self.broker_reclaim(key).await?, mode) {
            (ChatReclaim::Reclaimed { .. }, Mode::Decide | Mode::Replay) => {}
            (ChatReclaim::Busy, Mode::Decide) => {
                let journal = Arc::clone(&self.journal);
                blocking(move || {
                    journal.finish_reclaim_intent(&claim.stem)?;
                    drop(claim);
                    Ok(())
                })
                .await?;
                return Ok(Reclaimed::Busy);
            }
            (ChatReclaim::Busy, Mode::Replay) => return Err(HistoryError::ReplayBlocked),
        }
        let journal = Arc::clone(&self.journal);
        blocking(move || {
            journal.remove_history(&claim.stem)?;
            journal.finish_reclaim_intent(&claim.stem)?;
            drop(claim);
            Ok(())
        })
        .await?;
        Ok(Reclaimed::Chat)
    }
}

impl HistoryAdmission {
    pub(crate) async fn ensure(&self, stem: &str, growth: Growth) -> Result<(), HistoryError> {
        let retention = &self.retention;
        if growth.history > retention.budget {
            return Err(HistoryError::TooLarge {
                need: growth.history,
                budget: retention.budget,
            });
        }
        let mut skipped = HashSet::new();
        for _ in 0..RECLAIMS_PER_PASS {
            if retention.fits(growth).await? {
                return Ok(());
            }
            let Some(candidate) = retention.oldest(Some(stem), &skipped).await? else {
                return Err(HistoryError::Exhausted);
            };
            if retention.reclaim(&candidate, Reason::Budget).await? != Reclaimed::Chat {
                skipped.insert(candidate.name().to_owned());
            }
        }
        if retention.fits(growth).await? {
            Ok(())
        } else {
            Err(HistoryError::Exhausted)
        }
    }
}

impl HistoryRetention {
    async fn fits(&self, growth: Growth) -> Result<bool, HistoryError> {
        let usage = self.usage(Vec::new()).await?;
        if usage.unmeasured > 0 {
            return Err(HistoryError::Unmeasured);
        }
        // Admission is held, so no recording of ours is in flight: none in flight means every
        // debited recording has landed in the broker's own count.
        if usage.recording == 0 && *self.indeterminate.lock() > 0 {
            let journal = Arc::clone(&self.journal);
            blocking(move || journal.write_debit(0)).await?;
            *self.indeterminate.lock() = 0;
        }
        let journal = Arc::clone(&self.journal);
        let journal_bytes = blocking(move || journal.history_bytes()).await?;
        let total = journal_bytes
            .saturating_add(usage.chat_used)
            .saturating_add(usage.chat_reserved)
            .saturating_add(*self.indeterminate.lock())
            .saturating_add(growth.history);
        Ok(total <= self.budget && usage.root_free_bytes >= growth.broker)
    }

    #[cfg(test)]
    pub(crate) fn claimed(&self, stem: &str) -> bool {
        self.claims.stems.lock().contains_key(stem)
    }
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, JournalError> + Send + 'static,
) -> Result<T, HistoryError> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|_stopped| HistoryError::Task)?
        .map_err(HistoryError::from)
}
