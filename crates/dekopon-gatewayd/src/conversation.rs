//! Conversation text is never forwarded to the broker, so it stays out of the privileged process;
//! only the gateway's own journal writes it to disk, and only when an operator configures one.

use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{
    ArenaHeadroom, ArenaRefusal, ArenaSnapshot, ConversationTurn, History, PackedArena,
};
use dekopon_core::{AgentId, ExternalSubject};
use sha2::{Digest, Sha256, digest::Update};

use crate::{
    asset::{AssetAccess, AssetFence},
    cache_key::{self, CacheKeySecret},
    config::MemoryWindow,
    routes::BoundRoute,
};

#[derive(Clone, Eq, Hash, PartialEq)]
enum ConversationAudience {
    Private(ExternalSubject),
    Shared,
    Space(Box<(dekopon_core::SpaceKey, Option<String>)>),
}

/// The transport, agent, and audience together stop two agents or installations from sharing
/// history; the key has no Debug so a subject or native conversation id can't leak into a log span.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct ConversationKey {
    agent: AgentId,
    transport: String,
    conversation: String,
    audience: ConversationAudience,
}

impl ConversationKey {
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

    pub fn shared(agent: &AgentId, transport: &str, conversation: &str) -> Self {
        Self {
            agent: agent.clone(),
            transport: transport.to_owned(),
            conversation: conversation.to_owned(),
            audience: ConversationAudience::Shared,
        }
    }

    pub(crate) fn for_space(space: dekopon_core::SpaceKey, thread: Option<String>) -> Self {
        Self {
            agent: space.agent.clone(),
            transport: String::new(),
            conversation: String::new(),
            audience: ConversationAudience::Space(Box::new((space, thread))),
        }
    }

    pub(crate) fn absorb(&self, digest: &mut impl Update) {
        match &self.audience {
            ConversationAudience::Private(_) | ConversationAudience::Shared => {
                for part in [self.agent.as_str(), &self.transport, &self.conversation] {
                    cache_key::absorb(digest, part.as_bytes());
                }
            }
            ConversationAudience::Space(_) => {}
        }
        match &self.audience {
            ConversationAudience::Private(subject) => {
                digest.update(&[1]);
                cache_key::absorb(digest, subject.canonical().as_bytes());
            }
            ConversationAudience::Shared => digest.update(&[0]),
            ConversationAudience::Space(binding) => {
                let (space, thread) = binding.as_ref();
                digest.update(&space.encode());
                match thread {
                    Some(thread) => {
                        digest.update(&[1]);
                        cache_key::absorb(digest, thread.as_bytes());
                    }
                    None => digest.update(&[0]),
                }
                digest.update(&[0]);
            }
        }
    }

    /// A digest, so any subject or native conversation id encodes to a safe file name.
    pub fn journal_stem(&self) -> String {
        let mut digest = Sha256::new();
        self.absorb(&mut digest);
        let stem: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{}/{stem}", self.agent.as_str())
    }
}

struct Conversation {
    history: History,
    arena: Arc<PackedArena>,
    record_ids: Vec<u64>,
    next_record_id: u64,
    cache_key: String,
    touched: Instant,
    watermark: Option<Watermark>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Watermark {
    pub after: String,
    pub taken: Vec<String>,
}

#[derive(Default)]
pub(crate) struct TakenIn {
    pub newest: Option<String>,
    pub steers: Vec<String>,
    pub recalled: Option<ConversationTurn>,
}

impl TakenIn {
    fn advance(self, watermark: Option<Watermark>) -> Option<Watermark> {
        let mut watermark = match (self.newest, watermark) {
            (Some(after), Some(mut watermark)) => {
                if later(&after, &watermark.after) {
                    watermark.after = after;
                }
                watermark
            }
            (Some(after), None) => Watermark {
                after,
                taken: Vec::new(),
            },
            (None, Some(watermark)) => watermark,
            (None, None) => return None,
        };
        watermark.taken.extend(self.steers);
        let Watermark { after, taken } = &mut watermark;
        taken.retain(|id| later(id, after));
        Some(watermark)
    }
}

// Slack timestamps and Discord snowflakes are both fixed-alphabet numerals, so a longer id is newer.
fn later(id: &str, than: &str) -> bool {
    (id.len(), id) > (than.len(), than)
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Residency {
    Cold,
    Resident,
    Since(Watermark),
}

struct Slot {
    generation: u64,
    asset_fence: Arc<AssetFence>,
    granted: Vec<String>,
    pending: usize,
    live: Option<Conversation>,
}

struct StoreState {
    next_generation: u64,
    slots: HashMap<ConversationKey, Slot>,
    capacity_retired: HashMap<ConversationKey, (u64, Vec<String>, Weak<AssetFence>)>,
}

/// The lease and asset token only bound this generation's history and assets; neither authorizes
/// running a capability, and both go stale once a fresh grant, idle check, or eviction replaces the
/// generation.
pub(crate) struct ConversationSeed<'a> {
    pub history: History,
    pub cache_key: String,
    pub assets: AssetAccess,
    pub lease: ConversationLease<'a>,
    /// True only for the call that created this generation, which alone restores recalled assets.
    pub created: bool,
}

pub(crate) struct ConversationLease<'a> {
    store: &'a ConversationStore,
    key: ConversationKey,
    generation: u64,
    granted: Vec<String>,
    active: bool,
}

impl ConversationLease<'_> {
    pub fn commit(
        mut self,
        window: MemoryWindow,
        turn: ConversationTurn,
        mut taken: TakenIn,
        declared_cache_key: &str,
        now: Instant,
    ) -> bool {
        let mut state = self.store.state.lock();
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
            let recalled = taken.recalled.take();
            match slot.live.as_mut() {
                Some(existing) => {
                    if let Some(recalled) = recalled {
                        existing.record(recalled);
                    }
                    existing.record(turn);
                    existing.touched = now;
                    existing.watermark = taken.advance(existing.watermark.take());
                }
                None => {
                    let mut conversation = Conversation::new(
                        History::new(window.limits),
                        &self.store.headroom,
                        declared_cache_key.to_owned(),
                        now,
                    );
                    if let Some(recalled) = recalled {
                        conversation.record(recalled);
                    }
                    conversation.record(turn);
                    conversation.watermark = taken.advance(None);
                    slot.live = Some(conversation);
                }
            }
            self.store.enforce_ceiling(&mut state);
        }
        self.active = false;
        current
    }
}

impl Drop for ConversationLease<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.store.state.lock();
        let remove = state.slots.get_mut(&self.key).is_some_and(|slot| {
            if slot.generation != self.generation || slot.granted != self.granted {
                return false;
            }
            decrement_pending(slot);
            slot.pending == 0 && slot.live.is_none()
        });
        if remove && let Some(slot) = state.slots.remove(&self.key) {
            slot.asset_fence.deactivate();
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvictionReason {
    Idle,
    Capacity,
    GrantChanged,
    Sealed,
}

impl EvictionReason {
    const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Capacity => "capacity",
            Self::GrantChanged => "grant-changed",
            Self::Sealed => "sealed",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SealedConversation {
    pub at: std::time::SystemTime,
}

pub(crate) struct ConversationStore {
    capacity: usize,
    cache_keys: CacheKeySecret,
    headroom: Arc<ArenaHeadroom>,
    state: Mutex<StoreState>,
}

impl Conversation {
    fn new(
        history: History,
        headroom: &Arc<ArenaHeadroom>,
        cache_key: String,
        now: Instant,
    ) -> Self {
        let arena = Arc::new(PackedArena::new(Arc::clone(headroom)));
        let record_ids = (1..=history.len() as u64).collect();
        let next_record_id = history.len() as u64 + 1;
        let conversation = Self {
            history,
            arena,
            record_ids,
            next_record_id,
            cache_key,
            touched: now,
            watermark: None,
        };
        conversation.repack();
        conversation
    }

    fn record(&mut self, turn: ConversationTurn) {
        let prior = self.record_ids.len();
        self.history.record(turn);
        self.record_ids.push(self.next_record_id);
        self.next_record_id += 1;
        let dropped = prior + 1 - self.history.len();
        self.record_ids.drain(..dropped);
        self.repack();
    }

    fn repack(&self) {
        let records: Vec<_> = self
            .record_ids
            .iter()
            .zip(self.history.turns())
            .map(|(id, turn)| (*id, turn))
            .collect();
        if let Err(reason) = self.arena.repack(&records) {
            tracing::warn!(
                event = "gateway_memory_retention_failed",
                reason = match reason {
                    ArenaRefusal::TransientHeadroom => "arena-headroom",
                    ArenaRefusal::DuplicateRecord => "duplicate-record",
                },
                "gateway.memory.retention.failed"
            );
        }
    }
}

impl ConversationStore {
    #[cfg(test)]
    pub fn new(capacity: usize) -> Self {
        Self::with_arena_limit(capacity, 128 * 1024 * 1024)
    }

    #[cfg(test)]
    pub fn with_arena_limit(capacity: usize, arena_limit: usize) -> Self {
        Self::with_cache_keys(capacity, arena_limit, CacheKeySecret::from_bytes([7; 32]))
    }

    pub fn with_cache_keys(
        capacity: usize,
        arena_limit: usize,
        cache_keys: CacheKeySecret,
    ) -> Self {
        Self {
            capacity,
            cache_keys,
            headroom: ArenaHeadroom::new(arena_limit),
            state: Mutex::new(StoreState {
                next_generation: 1,
                slots: HashMap::new(),
                capacity_retired: HashMap::new(),
            }),
        }
    }

    pub(crate) fn route_cache_key(&self, route: &BoundRoute) -> String {
        self.cache_keys
            .for_route(&route.agent, &route.transport, route.route_index)
    }

    /// Whether `begin` would continue a window already in memory, making a recall pointless.
    pub fn resident(
        &self,
        key: &ConversationKey,
        granted: &[String],
        window: MemoryWindow,
        now: Instant,
    ) -> Residency {
        let state = self.state.lock();
        let live = state
            .slots
            .get(key)
            .filter(|slot| slot.granted == granted)
            .and_then(|slot| slot.live.as_ref())
            .filter(|conversation| !expired(conversation, window.idle_timeout, now));
        match live {
            None => Residency::Cold,
            Some(conversation) => conversation
                .watermark
                .clone()
                .map_or(Residency::Resident, Residency::Since),
        }
    }

    pub fn begin(
        &self,
        key: &ConversationKey,
        granted: &[String],
        window: MemoryWindow,
        recalled: Option<History>,
        now: Instant,
    ) -> ConversationSeed<'_> {
        let mut state = self.state.lock();
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

        if let Some(reason) = stale
            && let Some(slot) = state.slots.remove(key)
        {
            let had_history = slot.live.is_some();
            slot.asset_fence.deactivate();
            if had_history {
                evicted(reason);
            }
        }

        let recalled = recalled.filter(|history| !history.is_empty());
        if !state.slots.contains_key(key) {
            state.capacity_retired.remove(key);
            let generation = allocate_generation(&mut state);
            let asset_fence = Arc::new(
                AssetFence::new().with_grant(crate::memory::journal::grant_digest(granted)),
            );
            let cache_key = self.cache_keys.for_conversation(key);
            let live = recalled
                .map(|history| Conversation::new(history, &self.headroom, cache_key.clone(), now));
            let history = live.as_ref().map_or_else(
                || History::new(window.limits),
                |conversation| conversation.history.clone(),
            );
            let adopted = live.is_some();
            state.slots.insert(
                key.clone(),
                Slot {
                    generation,
                    asset_fence: Arc::clone(&asset_fence),
                    granted: granted.to_vec(),
                    pending: 1,
                    live,
                },
            );
            if adopted {
                self.enforce_ceiling(&mut state);
            }
            return ConversationSeed {
                created: true,
                history,
                cache_key,
                assets: AssetAccess::persistent(key.clone(), generation, asset_fence),
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
        if slot.live.is_none()
            && let Some(history) = recalled
        {
            slot.live = Some(Conversation::new(
                history,
                &self.headroom,
                self.cache_keys.for_conversation(key),
                now,
            ));
        }
        let (history, cache_key) = slot.live.as_ref().map_or_else(
            || {
                (
                    History::new(window.limits),
                    self.cache_keys.for_conversation(key),
                )
            },
            |conversation| (conversation.history.clone(), conversation.cache_key.clone()),
        );
        ConversationSeed {
            created: false,
            history,
            cache_key,
            assets: AssetAccess::persistent(
                key.clone(),
                slot.generation,
                Arc::clone(&slot.asset_fence),
            ),
            lease: ConversationLease {
                store: self,
                key: key.clone(),
                generation: slot.generation,
                granted: granted.to_vec(),
                active: true,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn generation_for_test(&self, key: &ConversationKey) -> Option<u64> {
        self.state.lock().slots.get(key).map(|slot| slot.generation)
    }

    pub(crate) fn publication_eligible(
        &self,
        key: &ConversationKey,
        granted: &[String],
        generation: u64,
    ) -> bool {
        let state = self.state.lock();
        if let Some(slot) = state.slots.get(key) {
            return slot.generation == generation && slot.granted == granted;
        }
        state
            .capacity_retired
            .get(key)
            .is_some_and(|(id, expected, fence)| {
                *id == generation && expected == granted && fence.strong_count() > 0
            })
    }

    pub fn snapshot(
        &self,
        key: &ConversationKey,
        granted: &[String],
    ) -> Option<Arc<ArenaSnapshot>> {
        let state = self.state.lock();
        let slot = state.slots.get(key)?;
        if slot.granted != granted {
            return None;
        }
        slot.live
            .as_ref()
            .map(|conversation| conversation.arena.snapshot())
    }

    pub fn remove(&self, key: &ConversationKey, reason: EvictionReason) -> bool {
        let mut state = self.state.lock();
        let removed = state.slots.remove(key);
        state.capacity_retired.remove(key);
        let had_history = removed.as_ref().is_some_and(|slot| slot.live.is_some());
        if let Some(slot) = removed {
            slot.asset_fence.deactivate();
        }
        drop(state);
        if had_history {
            evicted(reason);
        }
        had_history
    }

    /// Keep this test-only: exposing the count in production telemetry would give the conversation
    /// store one more way to be observed.
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.state
            .lock()
            .slots
            .values()
            .filter(|slot| slot.live.is_some())
            .count()
    }

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
            if let Some(slot) = state.slots.remove(&oldest) {
                slot.asset_fence.deactivate();
                state
                    .capacity_retired
                    .retain(|_, (_, _, fence)| fence.strong_count() > 0);
                state.capacity_retired.insert(
                    oldest,
                    (
                        slot.generation,
                        slot.granted,
                        Arc::downgrade(&slot.asset_fence),
                    ),
                );
            }
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

/// Debug is hand-written so it prints only counts and byte totals, never the conversation text,
/// pending keys, or identifiers a derived implementation would print.
impl fmt::Debug for ConversationStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
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

fn expired(entry: &Conversation, idle_timeout: Duration, now: Instant) -> bool {
    now.saturating_duration_since(entry.touched) >= idle_timeout
}

fn evicted(reason: EvictionReason) {
    tracing::info!(
        event = "gateway_conversation_evicted",
        reason = reason.label(),
        "gateway.conversation.evicted"
    );
}

#[cfg(test)]
mod space_tests {
    use super::*;

    #[test]
    fn space_replay_stem_is_frozen_and_threads_remain_separate() {
        let space = dekopon_core::SpaceKey {
            agent: "reviewer".parse().expect("agent"),
            transport: "team-slack".to_owned().into(),
            workspace: Some("t1".to_owned().into()),
            channel: "c1".to_owned().into(),
        };
        let first = ConversationKey::for_space(space.clone(), Some("thread-1".to_owned()));
        assert_eq!(
            first.journal_stem(),
            "reviewer/5f6ae83559748e2d54cdf794bd028ae8e3338f30f3f0f6623ff9bd193105a2fe"
        );
        assert_ne!(
            first.journal_stem(),
            ConversationKey::for_space(space.clone(), Some("thread-2".to_owned())).journal_stem()
        );
        assert_ne!(
            first.journal_stem(),
            ConversationKey::for_space(space, None).journal_stem()
        );
    }
}
