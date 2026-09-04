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
//! Five properties are load-bearing and each has a test:
//!
//! - **Trusted route configuration selects the audience.** Private history includes the canonical
//!   authenticated subject in its key. Explicit shared history omits only that subject and still
//!   includes the agent, configured transport, and transport-derived conversation identity.
//! - **The granted capability set travels with the conversation.** A grant that differs from the one
//!   this message's fresh broker leg reported drops the whole selected history, so output fetched
//!   under a wider grant stops being replayed once the grant narrows.
//! - **Nothing here caches authorization.** The stored grant is an invalidation input and never a
//!   permission: every message still opens its own attested leg and asks the broker again.
//! - **A generation fences every commit.** Removing, replacing, or evicting a slot makes every older
//!   in-flight session's lease inert, so stale work cannot recreate forgotten text.
//! - **The prompt cache key is minted, not derived.** It is stored beside the history so it lives
//!   and dies with the prefix it names, and it carries nothing about the audience whose key it sits
//!   under; [`crate::cache_key`] states why a hashed identifier was refused.

use std::{
    collections::HashMap,
    fmt,
    sync::Mutex,
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{ConversationTurn, History};
use dekopon_core::{AgentId, ExternalSubject};

use crate::{cache_key, config::ConversationWindow};

/// The audience discriminant on a remembered transcript.
///
/// Deliberately has no `Debug`: the private half contains a canonical authenticated subject.
#[derive(Clone, Eq, Hash, PartialEq)]
enum ConversationAudience {
    Private(ExternalSubject),
    Shared,
}

/// One remembered transcript's complete isolation key.
///
/// The configured transport name and transport-derived conversation identity prevent aliases
/// across chat installations and conversations. The agent prevents two routed agents from sharing
/// transcript or attachment state even if every transport coordinate is otherwise equal. The
/// audience then either adds the canonical authenticated subject or marks an explicit shared route.
///
/// It carries no `Debug`, on purpose. Both a canonical subject and a service-native conversation
/// identifier are payload telemetry rather than metadata, so making a key unprintable is what stops
/// one `?key` in a log line from putting either into a span at the metadata level.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct ConversationKey {
    agent: AgentId,
    transport: String,
    conversation: String,
    audience: ConversationAudience,
}

impl ConversationKey {
    /// Keys one authenticated subject's state within an exact routed conversation.
    pub fn private(
        agent: &AgentId,
        transport: &str,
        conversation: &str,
        subject: &ExternalSubject,
    ) -> Self {
        Self {
            agent: agent.clone(),
            transport: transport.to_owned(),
            conversation: conversation.to_owned(),
            audience: ConversationAudience::Private(subject.clone()),
        }
    }

    /// Keys intentionally shared state within an exact routed conversation.
    pub fn shared(agent: &AgentId, transport: &str, conversation: &str) -> Self {
        Self {
            agent: agent.clone(),
            transport: transport.to_owned(),
            conversation: conversation.to_owned(),
            audience: ConversationAudience::Shared,
        }
    }
}

/// One live conversation.
struct Conversation {
    history: History,
    /// The provider cache lane every message of this conversation routes to.
    ///
    /// Minted with the entry and stored beside the history rather than derived from the key,
    /// because the key may contain a canonical subject and a cache key must contain nothing about
    /// its audience; [`crate::cache_key`] has the whole argument. Held here so it shares the
    /// history's lifetime exactly: the window of messages that genuinely share a prompt prefix is
    /// the window worth routing together, and an entry that goes away takes its lane with it.
    cache_key: String,
    /// Last time a finished message touched this conversation, for idle timeout and LRU eviction.
    touched: Instant,
}

/// One current key generation, including sessions that have begun but not committed.
struct Slot {
    /// Globally non-reused token fencing leases issued before replacement or removal.
    generation: u64,
    /// Exact sorted capability identifiers reported by the fresh legs in this generation.
    granted: Vec<String>,
    /// Sessions holding a lease for this generation.
    pending: usize,
    /// Absent until one of those sessions records a turn.
    live: Option<Conversation>,
}

struct StoreState {
    next_generation: u64,
    slots: HashMap<ConversationKey, Slot>,
}

/// What one persistent session needs to continue a conversation.
///
/// The lease is the authority to append only to the exact generation this seed observed. It is not
/// authorization to run a capability; it merely prevents an older in-flight turn from restoring a
/// conversation that a later fresh grant, empty grant, idle check, or capacity eviction removed.
pub(crate) struct ConversationSeed<'a> {
    /// The remembered exchanges to replay, empty on the first message of a conversation.
    pub history: History,
    /// The cache lane this session's model calls declare.
    ///
    /// For a live conversation this is its retained key. Concurrent sessions opening a new
    /// conversation each mint a candidate; the first matching commit chooses the retained lane.
    pub cache_key: String,
    /// Generation-fenced append lease. Dropping it without committing stores no turn.
    pub lease: ConversationLease<'a>,
}

/// One generation-fenced right to append a completed prompt turn.
///
/// This type deliberately has no `Debug`: it contains the non-debug conversation key.
pub(crate) struct ConversationLease<'a> {
    store: &'a ConversationStore,
    key: ConversationKey,
    generation: u64,
    granted: Vec<String>,
    active: bool,
}

impl ConversationLease<'_> {
    /// Appends one turn if this lease still names the current key generation.
    ///
    /// Equal-generation sessions append in completion order. A stale lease is a no-op: it never
    /// recreates an absent slot and never overwrites a replacement generation.
    pub fn commit(
        mut self,
        window: ConversationWindow,
        turn: ConversationTurn,
        declared_cache_key: &str,
        now: Instant,
    ) {
        let mut state = self.store.state.lock().expect("conversation store");
        let current = state
            .slots
            .get(&self.key)
            .is_some_and(|slot| slot.generation == self.generation && slot.granted == self.granted);
        if current {
            let slot = state
                .slots
                .get_mut(&self.key)
                .expect("the matching conversation slot exists");
            decrement_pending(slot);
            match slot.live.as_mut() {
                Some(existing) => {
                    existing.history.record(turn);
                    existing.touched = now;
                }
                None => {
                    let mut history = History::new(window.limits);
                    history.record(turn);
                    slot.live = Some(Conversation {
                        history,
                        cache_key: declared_cache_key.to_owned(),
                        touched: now,
                    });
                }
            }
            self.store.enforce_ceiling(&mut state);
        }
        self.active = false;
    }
}

impl Drop for ConversationLease<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.store.state.lock().expect("conversation store");
        let remove = state.slots.get_mut(&self.key).is_some_and(|slot| {
            if slot.generation != self.generation || slot.granted != self.granted {
                return false;
            }
            decrement_pending(slot);
            slot.pending == 0 && slot.live.is_none()
        });
        if remove {
            state.slots.remove(&self.key);
        }
        self.active = false;
    }
}

fn decrement_pending(slot: &mut Slot) {
    debug_assert!(
        slot.pending > 0,
        "every lease increments pending exactly once"
    );
    if slot.pending > 0 {
        slot.pending -= 1;
    }
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
/// that would have exceeded it. History is process memory and dies with the process, so there is
/// nothing to flush. Pending-only slots do not count against the conversation ceiling; their number
/// is bounded by the process-wide session admission ceiling.
pub(crate) struct ConversationStore {
    capacity: usize,
    state: Mutex<StoreState>,
}

impl ConversationStore {
    /// Creates a store tracking at most `capacity` committed conversations at once.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(StoreState {
                next_generation: 1,
                slots: HashMap::new(),
            }),
        }
    }

    /// Seeds one session, replacing whatever this message invalidated.
    ///
    /// An entry idle past the route's timeout, or built under a different granted capability set, is
    /// dropped rather than used. Every returned seed carries a lease for the selected generation.
    /// Replacing a generation invalidates all older leases before inference begins.
    pub fn begin(
        &self,
        key: &ConversationKey,
        granted: &[String],
        window: ConversationWindow,
        now: Instant,
    ) -> ConversationSeed<'_> {
        let mut state = self.state.lock().expect("conversation store");
        let stale = state.slots.get(key).and_then(|slot| {
            if slot
                .live
                .as_ref()
                .is_some_and(|conversation| expired(conversation, window.idle_timeout, now))
            {
                Some(EvictionReason::Idle)
            } else if slot.granted != granted {
                Some(EvictionReason::GrantChanged)
            } else {
                None
            }
        });

        if let Some(reason) = stale {
            let had_history = state
                .slots
                .remove(key)
                .is_some_and(|slot| slot.live.is_some());
            if had_history {
                evicted(reason);
            }
        }

        if !state.slots.contains_key(key) {
            let generation = allocate_generation(&mut state);
            state.slots.insert(
                key.clone(),
                Slot {
                    generation,
                    granted: granted.to_vec(),
                    pending: 1,
                    live: None,
                },
            );
            return ConversationSeed {
                history: History::new(window.limits),
                cache_key: cache_key::for_conversation(),
                lease: ConversationLease {
                    store: self,
                    key: key.clone(),
                    generation,
                    granted: granted.to_vec(),
                    active: true,
                },
            };
        }

        let slot = state
            .slots
            .get_mut(key)
            .expect("the conversation slot was checked above");
        slot.pending = slot
            .pending
            .checked_add(1)
            .expect("pending conversations are bounded by session admission");
        let (history, cache_key) = slot.live.as_ref().map_or_else(
            || (History::new(window.limits), cache_key::for_conversation()),
            |conversation| (conversation.history.clone(), conversation.cache_key.clone()),
        );
        ConversationSeed {
            history,
            cache_key,
            lease: ConversationLease {
                store: self,
                key: key.clone(),
                generation: slot.generation,
                granted: granted.to_vec(),
                active: true,
            },
        }
    }

    /// Forgets one selected conversation generation outright.
    ///
    /// This is what an empty grant gets. Removing a pending-only slot also invalidates its leases,
    /// but only removal of remembered history emits an eviction and returns `true`.
    pub fn remove(&self, key: &ConversationKey, reason: EvictionReason) -> bool {
        let removed = self
            .state
            .lock()
            .expect("conversation store")
            .slots
            .remove(key)
            .is_some_and(|slot| slot.live.is_some());
        if removed {
            evicted(reason);
        }
        removed
    }

    /// How many committed conversations are resident, against `sessions.maxConversations`.
    ///
    /// Test-only: nothing in the daemon reads this count, because a store that reported its own
    /// size into telemetry would be one more place a conversation could be described.
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.state
            .lock()
            .expect("conversation store")
            .slots
            .values()
            .filter(|slot| slot.live.is_some())
            .count()
    }

    /// Drops least-recently-used committed conversations until the ceiling holds.
    fn enforce_ceiling(&self, state: &mut StoreState) {
        while state
            .slots
            .values()
            .filter(|slot| slot.live.is_some())
            .count()
            > self.capacity
        {
            let Some(oldest) = state
                .slots
                .iter()
                .filter_map(|(key, slot)| {
                    slot.live
                        .as_ref()
                        .map(|conversation| (key, conversation.touched))
                })
                .min_by_key(|(_, touched)| *touched)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            state.slots.remove(&oldest);
            evicted(EvictionReason::Capacity);
        }
    }
}

fn allocate_generation(state: &mut StoreState) -> u64 {
    let generation = state.next_generation;
    state.next_generation = state
        .next_generation
        .checked_add(1)
        .expect("conversation generation space exhausted");
    generation
}

/// Counts and byte totals, never text or pending keys.
///
/// Written by hand because the derived form would print every remembered exchange and identifier.
impl fmt::Debug for ConversationStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().expect("conversation store");
        let conversations = state
            .slots
            .values()
            .filter_map(|slot| slot.live.as_ref())
            .collect::<Vec<_>>();
        let turns = conversations
            .iter()
            .map(|conversation| conversation.history.len())
            .sum::<usize>();
        let bytes = conversations
            .iter()
            .map(|conversation| conversation.history.bytes())
            .sum::<usize>();
        formatter
            .debug_struct("ConversationStore")
            .field("capacity", &self.capacity)
            .field("conversations", &conversations.len())
            .field("turns", &turns)
            .field("bytes", &bytes)
            .finish()
    }
}

/// Whether an entry has gone untouched for at least as long as its route allows.
fn expired(entry: &Conversation, idle_timeout: Duration, now: Instant) -> bool {
    now.saturating_duration_since(entry.touched) >= idle_timeout
}

/// One lifecycle event per forgotten committed conversation, carrying a reason and nothing else.
fn evicted(reason: EvictionReason) {
    tracing::info!(
        event = "gateway_conversation_evicted",
        reason = reason.label()
    );
}
