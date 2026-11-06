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
}

impl HistoryError {
    pub(crate) const fn category(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "history-too-large",
            Self::Exhausted => "history-exhausted",
            Self::Broker => "history-broker",
            Self::Journal(_) => "history-journal",
            Self::Task => "history-task",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Reclaimed {
    Chat,
    Busy,
    Active,
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
}

#[must_use = "admission serializes history writes only while it is held"]
pub(crate) struct HistoryAdmission {
    retention: Arc<HistoryRetention>,
    _turn: OwnedMutexGuard<()>,
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
        })
    }

    pub(crate) async fn begin_use(&self, stem: &str) -> HistoryUse {
        loop {
            let changed = self.claims.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut stems = self.claims.stems.lock();
                match stems.get(stem).copied() {
                    None => {
                        stems.insert(stem.to_owned(), Claim::Used(NonZeroUsize::MIN));
                        break;
                    }
                    Some(Claim::Used(count)) => {
                        stems.insert(stem.to_owned(), Claim::Used(count.saturating_add(1)));
                        break;
                    }
                    Some(Claim::Reclaiming) => {}
                }
            }
            changed.await;
        }
        HistoryUse {
            claims: Arc::clone(&self.claims),
            stem: stem.to_owned(),
        }
    }

    fn try_reclaim(&self, stem: &str) -> Option<ReclaimClaim> {
        let mut stems = self.claims.stems.lock();
        if matches!(stems.get(stem), Some(Claim::Reclaiming)) {
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

    pub(crate) async fn note_activity(&self, stem: &str, at: SystemTime) {
        let journal = Arc::clone(&self.journal);
        let stem = stem.to_owned();
        let noted = tokio::task::spawn_blocking(move || journal.note_activity(&stem, at)).await;
        if !matches!(noted, Ok(Ok(()))) {
            tracing::warn!(
                event = "gateway_history_activity_failed",
                "gateway.history.activity.failed"
            );
        }
    }

    // Boot replays every intent before any session can claim its chat.
    pub(crate) async fn recover(&self) -> Result<usize, HistoryError> {
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
            let Some(_claim) = self.try_reclaim(&stem) else {
                continue;
            };
            let journal = Arc::clone(&self.journal);
            blocking(move || journal.finish_reclaim_intent(&stem)).await?;
            finished += 1;
        }
        Ok(finished)
    }

    pub(crate) async fn expire(&self, now: SystemTime) -> Result<usize, HistoryError> {
        let _turn = self.admission.lock().await;
        let cutoff_ms =
            millis(now).saturating_sub(u64::try_from(self.idle.as_millis()).unwrap_or(u64::MAX));
        let journal = Arc::clone(&self.journal);
        let chats = blocking(move || journal.history_chats()).await?;
        let mut expired = 0;
        for chat in chats
            .into_iter()
            .take_while(|chat| chat.activity_ms < cutoff_ms)
            .take(RECLAIMS_PER_PASS)
        {
            if self.reclaim(&chat, Reason::Idle { cutoff_ms }).await? == Reclaimed::Chat {
                expired += 1;
            }
        }
        Ok(expired)
    }

    async fn usage(&self) -> Result<HistoryUsage, HistoryError> {
        let client = self
            .broker
            .leg_client()
            .map_err(|_unreachable| HistoryError::Broker)?;
        client
            .history_usage()
            .await
            .map_err(|_failed| HistoryError::Broker)
    }

    async fn reclaim(&self, chat: &HistoryChat, reason: Reason) -> Result<Reclaimed, HistoryError> {
        let Some(_claim) = self.try_reclaim(&chat.stem) else {
            return Ok(Reclaimed::Busy);
        };
        let journal = Arc::clone(&self.journal);
        let current = blocking(move || journal.history_chats()).await?;
        let activity_ms = current
            .iter()
            .find(|found| found.stem == chat.stem)
            .map_or(0, |found| found.activity_ms);
        let active = match reason {
            Reason::Idle { cutoff_ms } => activity_ms >= cutoff_ms,
            Reason::Budget => activity_ms > chat.activity_ms,
        };
        if active {
            return Ok(Reclaimed::Active);
        }
        let journal = Arc::clone(&self.journal);
        let stem = chat.stem.clone();
        blocking(move || journal.write_reclaim_intent(&stem)).await?;
        let outcome = self.finish(&chat.stem).await;
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

    // The broker copy goes first, then the transcript, then the intent: a crash between any two
    // steps leaves the intent, and replaying it finishes the same steps.
    async fn finish(&self, stem: &str) -> Result<Reclaimed, HistoryError> {
        let key = HistoryKey::new(stem.to_owned()).map_err(|_invalid| JournalError::Corrupt)?;
        let client = self
            .broker
            .leg_client()
            .map_err(|_unreachable| HistoryError::Broker);
        let reclaimed = match client {
            Ok(client) => client
                .reclaim_chat(key)
                .await
                .map_err(|_failed| HistoryError::Broker),
            Err(error) => Err(error),
        };
        match reclaimed {
            Ok(ChatReclaim::Reclaimed { .. }) => {}
            Ok(ChatReclaim::Busy) => {
                let journal = Arc::clone(&self.journal);
                let stem = stem.to_owned();
                blocking(move || journal.finish_reclaim_intent(&stem)).await?;
                return Ok(Reclaimed::Busy);
            }
            Err(error) => return Err(error),
        }
        let journal = Arc::clone(&self.journal);
        let stem = stem.to_owned();
        blocking(move || {
            journal.remove_history(&stem)?;
            journal.finish_reclaim_intent(&stem)
        })
        .await?;
        Ok(Reclaimed::Chat)
    }
}

impl HistoryAdmission {
    pub(crate) async fn ensure(&self, stem: &str, need: u64) -> Result<(), HistoryError> {
        let retention = &self.retention;
        if need > retention.budget {
            return Err(HistoryError::TooLarge {
                need,
                budget: retention.budget,
            });
        }
        let mut skipped = HashSet::new();
        for _ in 0..=RECLAIMS_PER_PASS {
            let usage = retention.usage().await?;
            let journal = Arc::clone(&retention.journal);
            let journal_bytes = blocking(move || journal.history_bytes()).await?;
            let total = journal_bytes
                .saturating_add(usage.chat_used)
                .saturating_add(usage.chat_reserved)
                .saturating_add(need);
            if total <= retention.budget && usage.root_free_bytes >= need {
                return Ok(());
            }
            let journal = Arc::clone(&retention.journal);
            let chats = blocking(move || journal.history_chats()).await?;
            let Some(candidate) = chats
                .into_iter()
                .find(|chat| chat.stem != stem && !skipped.contains(&chat.stem))
            else {
                return Err(HistoryError::Exhausted);
            };
            if retention.reclaim(&candidate, Reason::Budget).await? != Reclaimed::Chat {
                skipped.insert(candidate.stem);
            }
        }
        Err(HistoryError::Exhausted)
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
