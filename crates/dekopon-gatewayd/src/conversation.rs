//! Conversation text is never forwarded to the broker, so it stays out of the privileged process;
//! only the gateway's own journal writes it to disk, and only when an operator configures one.

use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{ConversationTurn, History};
use dekopon_core::{AgentId, ExternalSubject};
use sha2::{Digest, Sha256, digest::Update};

use crate::{
    asset::{AssetAccess, AssetFence},
    cache_key::{self, CacheKeySecret},
    config::MemoryWindow,
    memory::project::Prior,
    routes::BoundRoute,
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
    space: dekopon_core::SpaceKey,
    thread: Option<String>,
    audience: ConversationAudience,
}

impl ConversationKey {
    pub(crate) fn of(
        agent: &AgentId,
        transport: &str,
        conversation: &dekopon_broker_protocol::Conversation,
        subject: Option<&ExternalSubject>,
    ) -> Self {
        Self {
            space: dekopon_core::SpaceKey {
                agent: agent.clone(),
                transport: transport.to_owned().into(),
                workspace: conversation.container.clone().map(Into::into),
                channel: conversation.id.clone().into(),
            },
            thread: conversation.thread.clone(),
            audience: match subject {
                Some(subject) => ConversationAudience::Private(subject.clone()),
                None => ConversationAudience::Shared,
            },
        }
    }

    #[cfg(test)]
    pub fn private(
        agent: &AgentId,
        transport: &str,
        channel: &str,
        subject: &ExternalSubject,
    ) -> Self {
        Self {
            space: dekopon_core::SpaceKey {
                agent: agent.clone(),
                transport: transport.to_owned().into(),
                workspace: None,
                channel: channel.to_owned().into(),
            },
            thread: None,
            audience: ConversationAudience::Private(subject.clone()),
        }
    }

    #[cfg(test)]
    pub fn shared(agent: &AgentId, transport: &str, channel: &str) -> Self {
        Self::for_space(
            dekopon_core::SpaceKey {
                agent: agent.clone(),
                transport: transport.to_owned().into(),
                workspace: None,
                channel: channel.to_owned().into(),
            },
            None,
        )
    }

    pub(crate) fn for_space(space: dekopon_core::SpaceKey, thread: Option<String>) -> Self {
        Self {
            space,
            thread,
            audience: ConversationAudience::Shared,
        }
    }

    pub(crate) fn absorb(&self, digest: &mut impl Update) {
        cache_key::absorb(digest, self.space.agent.as_str().as_bytes());
        cache_key::absorb(digest, self.space.transport.0.as_bytes());
        match &self.space.workspace {
            Some(workspace) => {
                digest.update(&[1]);
                cache_key::absorb(digest, workspace.0.as_bytes());
            }
            None => digest.update(&[0]),
        }
        cache_key::absorb(digest, self.space.channel.0.as_bytes());
        match &self.thread {
            Some(thread) => {
                digest.update(&[1]);
                cache_key::absorb(digest, thread.as_bytes());
            }
            None => digest.update(&[0]),
        }
        match &self.audience {
            ConversationAudience::Private(subject) => {
                digest.update(&[1]);
                cache_key::absorb(digest, subject.canonical().as_bytes());
            }
            ConversationAudience::Shared => digest.update(&[0]),
        }
    }

    pub fn journal_stem(&self) -> String {
        let mut digest = Sha256::new();
        self.absorb(&mut digest);
        let stem: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{}/{stem}", self.space.agent.as_str())
    }
}

/// What a conversation slot replays: a journal-recall route keeps only the previous turn's
/// projection report, and every other route keeps its own window of user and answer text.
#[derive(Clone, Debug)]
pub(crate) enum Window {
    Journal { prior: Vec<Prior> },
    Owned(History),
}

impl Window {
    pub(crate) fn turns(&self) -> usize {
        match self {
            Self::Journal { prior } => prior.len(),
            Self::Owned(history) => history.len(),
        }
    }

    pub(crate) fn bytes(&self) -> usize {
        match self {
            Self::Journal { .. } => 0,
            Self::Owned(history) => history.bytes(),
        }
    }
}

pub(crate) enum Seed {
    Journal,
    Owned(Option<History>),
}

pub(crate) enum Commit {
    Journal {
        prior: Vec<Prior>,
    },
    Owned {
        recalled: Option<ConversationTurn>,
        turn: ConversationTurn,
    },
}

struct Conversation {
    window: Window,
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
}

/// The lease and asset token only bound this generation's history and assets; neither authorizes
/// running a capability, and both go stale once a fresh grant, idle check, or eviction replaces the
/// generation.
pub(crate) struct ConversationSeed<'a> {
    pub window: Window,
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
        commit: Commit,
        taken: TakenIn,
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
            let conversation = slot.live.get_or_insert_with(|| Conversation {
                window: match commit {
                    Commit::Journal { .. } => Window::Journal { prior: Vec::new() },
                    Commit::Owned { .. } => Window::Owned(History::new(window.limits)),
                },
                cache_key: declared_cache_key.to_owned(),
                touched: now,
                watermark: None,
            });
            match (commit, &mut conversation.window) {
                (Commit::Journal { prior }, Window::Journal { prior: kept }) => *kept = prior,
                (Commit::Owned { recalled, turn }, Window::Owned(history)) => {
                    if let Some(recalled) = recalled {
                        history.record(recalled);
                    }
                    history.record(turn);
                }
                (Commit::Journal { prior }, kept @ Window::Owned(_)) => {
                    *kept = Window::Journal { prior };
                }
                (Commit::Owned { recalled, turn }, kept @ Window::Journal { .. }) => {
                    let mut history = History::new(window.limits);
                    if let Some(recalled) = recalled {
                        history.record(recalled);
                    }
                    history.record(turn);
                    *kept = Window::Owned(history);
                }
            }
            conversation.touched = now;
            conversation.watermark = taken.advance(conversation.watermark.take());
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
    state: Mutex<StoreState>,
}

impl ConversationStore {
    #[cfg(test)]
    pub fn new(capacity: usize) -> Self {
        Self::with_cache_keys(capacity, CacheKeySecret::from_bytes([7; 32]))
    }

    pub fn with_cache_keys(capacity: usize, cache_keys: CacheKeySecret) -> Self {
        Self {
            capacity,
            cache_keys,
            state: Mutex::new(StoreState {
                next_generation: 1,
                slots: HashMap::new(),
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
        seed: Seed,
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

        let (recalled, empty) = match seed {
            Seed::Journal => (None, Window::Journal { prior: Vec::new() }),
            Seed::Owned(history) => (
                history.filter(|history| !history.is_empty()),
                Window::Owned(History::new(window.limits)),
            ),
        };
        let created = !state.slots.contains_key(key);
        if created {
            let generation = allocate_generation(&mut state);
            let asset_fence = Arc::new(
                AssetFence::new().with_grant(crate::memory::journal::grant_digest(granted)),
            );
            state.slots.insert(
                key.clone(),
                Slot {
                    generation,
                    asset_fence,
                    granted: granted.to_vec(),
                    pending: 0,
                    live: None,
                },
            );
        }
        let cache_key = self.cache_keys.for_conversation(key);
        let slot = state
            .slots
            .get_mut(key)
            .expect("the conversation slot was inserted above");
        slot.pending = slot
            .pending
            .checked_add(1)
            .expect("pending conversations are bounded by session admission");
        let adopted = slot.live.is_none() && recalled.is_some();
        if slot.live.is_none()
            && let Some(history) = recalled
        {
            slot.live = Some(Conversation {
                window: Window::Owned(history),
                cache_key: cache_key.clone(),
                touched: now,
                watermark: None,
            });
        }
        let (window_seed, cache_key) = slot.live.as_ref().map_or_else(
            || (empty, cache_key.clone()),
            |conversation| (conversation.window.clone(), conversation.cache_key.clone()),
        );
        let seeded = ConversationSeed {
            created,
            window: window_seed,
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
        };
        if adopted {
            self.enforce_ceiling(&mut state);
        }
        seeded
    }

    pub fn remove(&self, key: &ConversationKey, reason: EvictionReason) -> bool {
        let mut state = self.state.lock();
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
        let state = self.state.lock();
        let conversations = state
            .slots
            .values()
            .filter_map(|slot| slot.live.as_ref())
            .collect::<Vec<_>>();
        let turns = conversations
            .iter()
            .map(|conversation| conversation.window.turns())
            .sum::<usize>();
        let bytes = conversations
            .iter()
            .map(|conversation| conversation.window.bytes())
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
    fn colliding_joined_coordinates_have_distinct_memory_and_cache_keys() {
        use dekopon_broker_protocol::{Conversation, ConversationKind};
        let agent = "reviewer".parse().unwrap();
        let secret = CacheKeySecret::from_bytes([7; 32]);
        let coordinates =
            |workspace: Option<&str>, channel: &str, thread: Option<&str>| Conversation {
                kind: ConversationKind::Thread,
                container: workspace.map(str::to_owned),
                id: channel.to_owned(),
                thread: thread.map(str::to_owned),
            };
        let first = coordinates(Some("workspace"), "a:b", Some("c"));
        let second = coordinates(Some("workspace"), "a", Some("b:c"));
        assert_eq!(first.key(), second.key());
        let keys = [
            ConversationKey::of(&agent, "slack", &first, None),
            ConversationKey::of(&agent, "slack", &second, None),
            ConversationKey::of(&agent, "slack", &coordinates(None, "a:b", Some("c")), None),
            ConversationKey::of(
                &agent,
                "slack",
                &coordinates(Some(""), "a:b", Some("c")),
                None,
            ),
            ConversationKey::of(
                &agent,
                "slack",
                &coordinates(Some("workspace"), "a:b", None),
                None,
            ),
        ];
        for (index, key) in keys.iter().enumerate() {
            for other in &keys[index + 1..] {
                assert_ne!(key.journal_stem(), other.journal_stem());
                assert_ne!(secret.for_conversation(key), secret.for_conversation(other));
            }
        }
    }

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
