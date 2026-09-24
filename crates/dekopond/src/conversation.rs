//! This history lives only in the gateway's memory, never persisted or forwarded to the broker, so
//! the most sensitive conversation text never reaches the broker's more privileged process.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{ConversationTurn, History};
use dekopon_core::{AgentId, ExternalSubject};

use crate::{
    asset::{AssetAccess, AssetFence},
    cache_key,
    config::MemoryWindow,
};

#[derive(Clone, Eq, Hash, PartialEq)]
enum ConversationAudience {
    Private(ExternalSubject),
    Shared,
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
}

struct Conversation {
    history: History,
    /// The cache lane must be minted separately and never derived from the conversation key, since
    /// that key may reveal who is asking.
    cache_key: String,
    touched: Instant,
}

struct Slot {
    generation: u64,
    input_revision: u64,
    seed_revision: u64,
    gateway_notice: Option<&'static str>,
    asset_fence: Arc<AssetFence>,
    granted: Vec<String>,
    pending: usize,
    live: Option<Conversation>,
}

struct StoreState {
    next_generation: u64,
    slots: HashMap<ConversationKey, Slot>,
}

/// The lease and asset token only bound this generation's history and assets; neither authorizes
/// running a capability, and both go stale once a fresh grant, idle check, or eviction replaces the
/// generation.
pub(crate) struct ConversationSeed<'a> {
    pub history: History,
    pub cache_key: String,
    pub assets: AssetAccess,
    pub lease: ConversationLease<'a>,
    pub input: ConversationInput,
    pub gateway_notice: Option<&'static str>,
}

/// This input becomes invalid after any later normal request, even within the same generation, so
/// don't reuse a stale one.
#[derive(Clone)]
pub(crate) struct ConversationInput {
    key: ConversationKey,
    generation: u64,
    revision: u64,
    seed_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LateAssetRefusal {
    Missing,
    StaleInput,
    GrantChanged,
    Expired,
    InventoryUnavailable,
}

impl LateAssetRefusal {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing-generation",
            Self::StaleInput => "stale-input",
            Self::GrantChanged => "grant-changed",
            Self::Expired => "expired",
            Self::InventoryUnavailable => "inventory-unavailable",
        }
    }
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
}

impl EvictionReason {
    const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Capacity => "capacity",
            Self::GrantChanged => "grant-changed",
        }
    }
}

pub(crate) struct ConversationStore {
    capacity: usize,
    state: Mutex<StoreState>,
}

impl ConversationStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(StoreState {
                next_generation: 1,
                slots: HashMap::new(),
            }),
        }
    }

    pub fn begin(
        &self,
        key: &ConversationKey,
        granted: &[String],
        window: MemoryWindow,
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

        if let Some(reason) = stale
            && let Some(slot) = state.slots.remove(key)
        {
            let had_history = slot.live.is_some();
            slot.asset_fence.deactivate();
            if had_history {
                evicted(reason);
            }
        }

        if !state.slots.contains_key(key) {
            let generation = allocate_generation(&mut state);
            let asset_fence = Arc::new(AssetFence::new());
            state.slots.insert(
                key.clone(),
                Slot {
                    generation,
                    input_revision: 1,
                    seed_revision: 1,
                    gateway_notice: None,
                    asset_fence: Arc::clone(&asset_fence),
                    granted: granted.to_vec(),
                    pending: 1,
                    live: None,
                },
            );
            return ConversationSeed {
                gateway_notice: None,
                input: ConversationInput {
                    key: key.clone(),
                    generation,
                    revision: 1,
                    seed_revision: 1,
                },
                history: History::new(window.limits),
                cache_key: cache_key::for_conversation(),
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
        slot.input_revision = slot
            .input_revision
            .checked_add(1)
            .expect("conversation input revision space exhausted");
        slot.seed_revision = slot
            .seed_revision
            .checked_add(1)
            .expect("conversation seed revision space exhausted");
        slot.pending = slot
            .pending
            .checked_add(1)
            .expect("pending conversations are bounded by session admission");
        let (history, cache_key) = slot.live.as_ref().map_or_else(
            || (History::new(window.limits), cache_key::for_conversation()),
            |conversation| (conversation.history.clone(), conversation.cache_key.clone()),
        );
        ConversationSeed {
            gateway_notice: slot.gateway_notice.take(),
            input: ConversationInput {
                key: key.clone(),
                generation: slot.generation,
                revision: slot.input_revision,
                seed_revision: slot.seed_revision,
            },
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

    pub fn invalidate_late_input(&self, key: &ConversationKey) {
        let mut state = self.state.lock().expect("conversation store");
        if let Some(slot) = state.slots.get_mut(key) {
            slot.input_revision = slot
                .input_revision
                .checked_add(1)
                .expect("conversation input revision space exhausted");
        }
    }

    pub fn remember_gateway_notice(&self, input: &ConversationInput, notice: &'static str) {
        let mut state = self.state.lock().expect("conversation store");
        if let Some(slot) = state.slots.get_mut(&input.key)
            && slot.generation == input.generation
            && slot.seed_revision == input.seed_revision
        {
            slot.gateway_notice = Some(notice);
        }
    }

    pub fn retain_late_assets<T>(
        &self,
        input: &ConversationInput,
        granted: &[String],
        window: MemoryWindow,
        cache_key: &str,
        register: impl FnOnce() -> Option<T>,
    ) -> Result<T, LateAssetRefusal> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("conversation store");
        let slot = state
            .slots
            .get(&input.key)
            .ok_or(LateAssetRefusal::Missing)?;
        if slot.generation != input.generation || slot.input_revision != input.revision {
            return Err(LateAssetRefusal::StaleInput);
        }
        let reason = if slot.granted != granted || granted.is_empty() {
            Some(EvictionReason::GrantChanged)
        } else if slot
            .live
            .as_ref()
            .is_some_and(|live| expired(live, window.idle_timeout, now))
        {
            Some(EvictionReason::Idle)
        } else {
            None
        };
        if let Some(reason) = reason {
            if let Some(slot) = state.slots.remove(&input.key) {
                slot.asset_fence.deactivate();
                evicted(reason);
            }
            return Err(match reason {
                EvictionReason::GrantChanged => LateAssetRefusal::GrantChanged,
                EvictionReason::Idle => LateAssetRefusal::Expired,
                EvictionReason::Capacity => LateAssetRefusal::Missing,
            });
        }
        let result = register().ok_or(LateAssetRefusal::InventoryUnavailable)?;
        let slot = state
            .slots
            .get_mut(&input.key)
            .ok_or(LateAssetRefusal::Missing)?;
        let live = slot.live.get_or_insert_with(|| Conversation {
            history: History::new(window.limits),
            cache_key: cache_key.to_owned(),
            touched: now,
        });
        live.touched = now;
        self.enforce_ceiling(&mut state);
        if !state.slots.contains_key(&input.key) {
            return Err(LateAssetRefusal::Missing);
        }
        Ok(result)
    }

    pub fn remove(&self, key: &ConversationKey, reason: EvictionReason) -> bool {
        let mut state = self.state.lock().expect("conversation store");
        let removed = state.slots.remove(key);
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
            .expect("conversation store")
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

fn expired(entry: &Conversation, idle_timeout: Duration, now: Instant) -> bool {
    now.saturating_duration_since(entry.touched) >= idle_timeout
}

fn evicted(reason: EvictionReason) {
    tracing::info!(
        event = "gateway_conversation_evicted",
        reason = reason.label()
    );
}
