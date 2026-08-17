//! What a `persistent` route remembers between one message and the next.
//!
//! The whole store is a bounded map in the daemon's memory. It is never written to disk, never sent
//! to the broker, and lost on restart — a person who asks a follow-up across a restart gets a
//! first-message answer. That placement is the point rather than a shortcut: the broker holds
//! provider credentials and a deliberately metadata-only audit chain, and conversation text there
//! would put the most sensitive content in the system inside the most privileged process. The
//! gateway already read the message and wrote the answer, so keeping the history here adds no new
//! reader.
//!
//! Three properties are load-bearing and each has a test:
//!
//! - **The key includes the sender.** Two people in one channel are two histories, and the
//!   alternative replays one person's exchange into another person's prompt.
//! - **The granted capability set travels with the conversation.** A grant that differs from the one
//!   this message's fresh broker leg reported drops the history, so output fetched under a wider
//!   grant stops being replayed once the grant narrows.
//! - **Nothing here caches authorization.** The stored grant is an invalidation input and never a
//!   permission: every message still opens its own attested leg and asks the broker again.

use std::{
    collections::HashMap,
    fmt,
    sync::Mutex,
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{ConversationTurn, History};

use crate::config::ConversationWindow;

/// One remembered conversation: a transport, the conversation on it, and whose exchange this is.
///
/// Deliberately *not* the admission key, which is `(transport, channel, thread)` and has no subject
/// in it. The two answer different questions — serialization asks "is this bot already busy on this
/// thread", history asks "whose exchange was this" — and the same two people talking at once in one
/// thread are one thing to serialize and two things to remember.
///
/// It carries no `Debug`, on purpose. Both a canonical subject and a service-native conversation
/// identifier are payload telemetry rather than metadata, so making a key unprintable is what stops
/// one `?key` in a log line from putting either into a span at the metadata level.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct ConversationKey {
    transport: String,
    conversation: String,
    subject: String,
}

impl ConversationKey {
    /// Keys one sender's history within one conversation on one transport.
    pub fn new(transport: &str, conversation: &str, subject: &str) -> Self {
        Self {
            transport: transport.to_owned(),
            conversation: conversation.to_owned(),
            subject: subject.to_owned(),
        }
    }
}

/// One conversation, plus what invalidates it.
struct Conversation {
    history: History,
    /// The capability identifiers the leg that last wrote this reported as granted.
    ///
    /// A sorted deterministic `Vec<String>`, which is what `CapabilityInvoker::granted` returns, so
    /// comparing two of them is a comparison rather than a hash of one.
    granted: Vec<String>,
    /// Last time a message touched this conversation, for the idle timeout and the LRU ceiling.
    touched: Instant,
}

/// Why a conversation stopped being remembered, as the lifecycle event records it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvictionReason {
    /// Untouched for longer than the route's idle timeout.
    Idle,
    /// The least recently used conversation, displaced by a newer one at the ceiling.
    Capacity,
    /// Built under a granted capability set that this message's fresh leg no longer reports.
    GrantChanged,
}

impl EvictionReason {
    /// Stable low-cardinality label for `gateway_conversation_evicted`.
    const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Capacity => "capacity",
            Self::GrantChanged => "grant-changed",
        }
    }
}

/// Every conversation this process remembers, bounded and evicted without a timer.
///
/// There is no sweeper task and no shutdown hook, which is deliberate rather than missing. A stale
/// entry is dropped by the lookup that would have used it, and the ceiling is enforced by the insert
/// that would have exceeded it — the same shape as the Slack transport's redelivery ring. History is
/// process memory and dies with the process, so there is nothing to flush.
pub(crate) struct ConversationStore {
    capacity: usize,
    entries: Mutex<HashMap<ConversationKey, Conversation>>,
}

impl ConversationStore {
    /// Creates a store tracking at most `capacity` conversations at once.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Seeds one session, dropping whatever this message invalidated.
    ///
    /// An entry idle past the route's timeout, or built under a different granted capability set, is
    /// dropped rather than used. What survives comes back as a clone the session prompts with; the
    /// stored copy stays where it is so a concurrent session on the same conversation reads the same
    /// thing rather than racing for ownership of it.
    pub fn begin(
        &self,
        key: &ConversationKey,
        granted: &[String],
        window: ConversationWindow,
        now: Instant,
    ) -> History {
        let mut entries = self.entries.lock().expect("conversation store");
        let stale = entries.get(key).and_then(|existing| {
            if expired(existing, window.idle_timeout, now) {
                Some(EvictionReason::Idle)
            } else if existing.granted != granted {
                Some(EvictionReason::GrantChanged)
            } else {
                None
            }
        });
        if let Some(reason) = stale {
            entries.remove(key);
            evicted(reason);
            return History::new(window.limits);
        }
        entries.get(key).map_or_else(
            || History::new(window.limits),
            |entry| entry.history.clone(),
        )
    }

    /// Records one finished exchange, creating the conversation if this was its first message.
    ///
    /// The turn is *appended* to whatever is stored now rather than the session's own copy being
    /// written back over it. That difference is the whole answer to two sessions sharing one
    /// conversation, which admission control deliberately does not prevent: on Slack a message
    /// opening a thread and a reply inside it admit under different keys and share one
    /// `conversation_id`, so a sender replying to themselves before the bot answers runs two
    /// sessions against one history. Writing back a clone would silently discard whichever exchange
    /// finished first; appending lands both, ordered by when they were answered.
    pub fn commit(
        &self,
        key: &ConversationKey,
        granted: &[String],
        window: ConversationWindow,
        turn: ConversationTurn,
        now: Instant,
    ) {
        let mut entries = self.entries.lock().expect("conversation store");
        match entries.get_mut(key) {
            // A grant that changed while this session ran is the same invalidation `begin` applies,
            // arriving one message later: this leg's answer is the only text in the window that was
            // certainly produced under the grant now on record.
            Some(existing) if existing.granted != granted => {
                existing.history = History::new(window.limits);
                existing.granted = granted.to_vec();
                existing.history.record(turn);
                existing.touched = now;
                evicted(EvictionReason::GrantChanged);
            }
            Some(existing) => {
                existing.history.record(turn);
                existing.touched = now;
            }
            None => {
                let mut history = History::new(window.limits);
                history.record(turn);
                entries.insert(
                    key.clone(),
                    Conversation {
                        history,
                        granted: granted.to_vec(),
                        touched: now,
                    },
                );
                self.enforce_ceiling(&mut entries);
            }
        }
    }

    /// Forgets one conversation outright, reporting whether anything was there.
    ///
    /// This is what an empty grant gets. Refusing the message alone would leave a revoked subject's
    /// exchange resident in the process for the rest of its idle timeout, which is precisely the
    /// text a revocation was about.
    pub fn remove(&self, key: &ConversationKey, reason: EvictionReason) -> bool {
        let removed = self
            .entries
            .lock()
            .expect("conversation store")
            .remove(key)
            .is_some();
        if removed {
            evicted(reason);
        }
        removed
    }

    /// How many conversations are resident, against `sessions.maxConversations`.
    ///
    /// Test-only: nothing in the daemon reads this count, because a store that reported its own
    /// size into telemetry would be one more place a conversation could be described. Eviction is
    /// observable through `gateway_conversation_evicted`, which is what an operator watching a
    /// ceiling set too low needs.
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.entries.lock().expect("conversation store").len()
    }

    /// Drops the least recently used conversation until the ceiling holds.
    ///
    /// A ceiling evicts rather than refuses: a person talking now matters more than one who stopped
    /// an hour ago, and refusing would turn a memory bound into an admission bound. `touched` is the
    /// only ordering state, which is what keeps the idle timeout and the ceiling from being two
    /// structures that can disagree.
    fn enforce_ceiling(&self, entries: &mut HashMap<ConversationKey, Conversation>) {
        while entries.len() > self.capacity {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            entries.remove(&oldest);
            evicted(EvictionReason::Capacity);
        }
    }
}

/// Counts and byte totals, never text.
///
/// Written by hand because the derived form would print every remembered exchange: [`History`] and
/// [`ConversationTurn`] both derive `Debug`, so one `tracing::debug!(?store)` would put whole
/// conversations into the log stream outside the payload gate that governs chat text everywhere
/// else.
impl fmt::Debug for ConversationStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.lock().expect("conversation store");
        let turns = entries
            .values()
            .map(|entry| entry.history.len())
            .sum::<usize>();
        let bytes = entries
            .values()
            .map(|entry| entry.history.bytes())
            .sum::<usize>();
        formatter
            .debug_struct("ConversationStore")
            .field("capacity", &self.capacity)
            .field("conversations", &entries.len())
            .field("turns", &turns)
            .field("bytes", &bytes)
            .finish()
    }
}

/// Whether an entry has gone untouched for longer than its route allows.
///
/// `saturating_duration_since` rather than subtraction: the caller supplies the clock, and a caller
/// that supplies a `now` behind an entry's own timestamp gets a live conversation rather than a
/// panic.
fn expired(entry: &Conversation, idle_timeout: Duration, now: Instant) -> bool {
    now.saturating_duration_since(entry.touched) >= idle_timeout
}

/// One lifecycle event per forgotten conversation, carrying a reason and nothing else.
///
/// No key, no subject, no counts of what was in it: a ceiling set too low has to read as eviction
/// churn without the churn itself becoming a record of who was talking to the bot.
fn evicted(reason: EvictionReason) {
    tracing::info!(
        event = "gateway_conversation_evicted",
        reason = reason.label()
    );
}
