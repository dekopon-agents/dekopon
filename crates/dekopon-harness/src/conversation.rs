//! Reusable scoped in-memory history ownership. Fresh admission always precedes a lease.
use crate::history::{History, HistoryLimits, JobRecord};
use std::{
    collections::HashMap,
    fmt,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationWindow {
    pub idle_timeout: Duration,
    pub limits: HistoryLimits,
}

/// Routing-only coordinates. No Debug/Serialize: sender and conversation are private metadata.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct ConversationKey {
    agent: String,
    route: String,
    transport: String,
    channel: String,
    conversation: String,
    subject: String,
}
impl ConversationKey {
    pub fn scoped(
        agent: &str,
        route: &str,
        transport: &str,
        channel: &str,
        conversation: &str,
        subject: &str,
    ) -> Self {
        Self {
            agent: agent.to_owned(),
            route: route.to_owned(),
            transport: transport.to_owned(),
            channel: channel.to_owned(),
            conversation: conversation.to_owned(),
            subject: subject.to_owned(),
        }
    }
    /// Scope commitment for checkpoint comparison; never sent to a model or used as a cache key.
    pub fn commitment(&self) -> String {
        crate::history::digest(
            &serde_json::to_vec(&[
                &self.agent,
                &self.route,
                &self.transport,
                &self.channel,
                &self.conversation,
                &self.subject,
            ])
            .expect("scope serializes"),
        )
    }
}
/// Prefix of the minted prompt-cache key that names one remembered conversation.
///
/// Public so the gateway's own cache-key lanes can be pinned against it: the key is a routing
/// hint, and two lanes that accidentally shared a prefix would be one lane. One definition.
pub const CONVERSATION_CACHE_PREFIX: &str = "dekopond-conversation";

/// Retained bytes across every conversation, independent of per-route context windows.
const MAX_STORE_BYTES: usize = 64 * 1024 * 1024;

struct Conversation {
    history: History,
    surface: Vec<String>,
    cache_key: String,
    touched: Instant,
    /// This entry's contribution to the store's retained-byte total.
    ///
    /// Recomputed only where the entry changes, so the ceiling check is arithmetic on a running
    /// total rather than a JSON encoding of every retained conversation under the store mutex.
    bytes: usize,
}
impl Conversation {
    fn footprint(&self, key: &ConversationKey) -> usize {
        self.history.bytes()
            + self.surface.iter().map(String::len).sum::<usize>()
            + self.cache_key.len()
            + key.agent.len()
            + key.route.len()
            + key.transport.len()
            + key.channel.len()
            + key.conversation.len()
            + key.subject.len()
    }
}

/// The map plus its running retained-byte total; the two are only ever changed together.
#[derive(Default)]
struct Entries {
    map: HashMap<ConversationKey, Conversation>,
    bytes: usize,
}
impl Entries {
    fn insert(&mut self, key: ConversationKey, conversation: Conversation) {
        self.bytes += conversation.bytes;
        if let Some(replaced) = self.map.insert(key, conversation) {
            self.bytes -= replaced.bytes;
        }
    }
    fn remove(&mut self, key: &ConversationKey) -> bool {
        match self.map.remove(key) {
            Some(removed) => {
                self.bytes -= removed.bytes;
                true
            }
            None => false,
        }
    }
}
/// cache_key is also the random generation lease. Late appends cannot resurrect evicted state.
pub struct ConversationSeed {
    pub history: History,
    pub cache_key: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvictionReason {
    Idle,
    Capacity,
    GrantChanged,
}
impl EvictionReason {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Capacity => "capacity",
            Self::GrantChanged => "grant-changed",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("conversation generation was invalidated; late append refused")]
pub struct StaleLease;

/// A global entry AND retained-byte ceiling, independent of per-route context windows.
pub struct BoundedConversationStore {
    capacity: usize,
    entries: Mutex<Entries>,
}
impl BoundedConversationStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.min(4096),
            entries: Mutex::new(Entries::default()),
        }
    }
    /// `surface` commits full scoped metadata plus the broker startup epoch when available. It is
    /// an invalidation comparison only, never an authorization snapshot usable for execution.
    pub fn begin(
        &self,
        key: &ConversationKey,
        surface: &[String],
        window: ConversationWindow,
        now: Instant,
    ) -> ConversationSeed {
        let entries = &mut *self.entries.lock().expect("conversation store");
        if let Some(reason) = entries.map.get(key).and_then(|entry| {
            if now.saturating_duration_since(entry.touched) >= window.idle_timeout {
                Some(EvictionReason::Idle)
            } else if entry.surface != surface {
                Some(EvictionReason::GrantChanged)
            } else {
                None
            }
        }) {
            entries.remove(key);
            evicted(reason);
        }
        if !entries.map.contains_key(key) {
            let mut seeded = Conversation {
                history: History::new(window.limits),
                surface: surface.to_vec(),
                cache_key: format!(
                    "{CONVERSATION_CACHE_PREFIX}-{}",
                    crate::checkpoint::opaque_id()
                ),
                touched: now,
                bytes: 0,
            };
            seeded.bytes = seeded.footprint(key);
            entries.insert(key.clone(), seeded);
        }
        let entry = entries.map.get_mut(key).expect("seeded conversation");
        // Seeding a turn is using the conversation. Without this the entry the caller is about to
        // answer under is the least recently touched one, so the ceiling evicts the conversation
        // this call just seeded and the delivered answer's `commit` is refused as a stale lease.
        entry.touched = now;
        let seed = ConversationSeed {
            history: entry.history.clone(),
            cache_key: entry.cache_key.clone(),
        };
        self.enforce_ceiling(entries, Some(key));
        seed
    }
    /// Append only the completing job under its live generation, never a cloned history window.
    /// A→B→A gets three random generations even if metadata returns to its original value.
    pub fn commit(
        &self,
        key: &ConversationKey,
        surface: &[String],
        window: ConversationWindow,
        turn: JobRecord,
        generation: &str,
        now: Instant,
    ) -> Result<(), StaleLease> {
        let entries = &mut *self.entries.lock().expect("conversation store");
        let entry = entries.map.get_mut(key).ok_or(StaleLease)?;
        if entry.cache_key != generation
            || entry.surface != surface
            || now.saturating_duration_since(entry.touched) >= window.idle_timeout
        {
            return Err(StaleLease);
        }
        entry.history.record(turn);
        entry.touched = now;
        let updated = entry.footprint(key);
        let previous = std::mem::replace(&mut entry.bytes, updated);
        entries.bytes = entries.bytes + updated - previous;
        self.enforce_ceiling(entries, Some(key));
        Ok(())
    }
    pub fn remove(&self, key: &ConversationKey, reason: EvictionReason) -> bool {
        let removed = self.entries.lock().expect("conversation store").remove(key);
        if removed {
            evicted(reason);
        }
        removed
    }
    /// Drops least-recently-touched conversations until both ceilings hold.
    ///
    /// `active` is the conversation the calling session is answering under. It is never the
    /// victim: evicting it would rotate the generation the caller already holds, so the answer it
    /// is about to deliver could not be appended to the history it was built from.
    fn enforce_ceiling(&self, entries: &mut Entries, active: Option<&ConversationKey>) {
        while entries.map.len() > self.capacity || entries.bytes > MAX_STORE_BYTES {
            let Some(oldest) = entries
                .map
                .iter()
                .filter(|(key, _)| active != Some(*key))
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            entries.remove(&oldest);
            evicted(EvictionReason::Capacity);
        }
    }
    /// How many conversations are resident, against `sessions.maxConversations`.
    ///
    /// Nothing in the daemon reads this count: a store that reported its own size into telemetry
    /// would be one more place a conversation could be described. Eviction is observable through
    /// `gateway_conversation_evicted`, which is what an operator watching a ceiling set too low
    /// needs. It is `pub` only because the gateway's residency tests live in another crate.
    pub fn tracked(&self) -> usize {
        self.entries.lock().expect("conversation store").map.len()
    }
}
/// Counts and byte totals, never text: [`History`] and [`JobRecord`] both derive `Debug`, so a
/// derived form would put whole conversations into the log stream outside the payload gate.
impl fmt::Debug for BoundedConversationStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.lock().expect("conversation store");
        f.debug_struct("BoundedConversationStore")
            .field("capacity", &self.capacity)
            .field("conversations", &entries.map.len())
            .field(
                "turns",
                &entries.map.values().map(|e| e.history.len()).sum::<usize>(),
            )
            .field(
                "bytes",
                &entries
                    .map
                    .values()
                    .map(|e| e.history.bytes())
                    .sum::<usize>(),
            )
            .finish()
    }
}
fn evicted(reason: EvictionReason) {
    tracing::info!(
        event = "gateway_conversation_evicted",
        reason = reason.label()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{HistoryLimits, JobRecord};

    fn window() -> ConversationWindow {
        ConversationWindow {
            idle_timeout: Duration::from_secs(900),
            limits: HistoryLimits {
                max_turns: 8,
                max_bytes: 64 * 1024,
            },
        }
    }
    fn key(conversation: &str) -> ConversationKey {
        ConversationKey::scoped("agent", "route", "dev", "channel", conversation, "subject")
    }
    fn surface() -> Vec<String> {
        vec!["fingerprint".to_owned()]
    }
    fn total(store: &BoundedConversationStore) -> usize {
        store.entries.lock().expect("store").bytes
    }
    fn recomputed(store: &BoundedConversationStore) -> usize {
        let entries = store.entries.lock().expect("store");
        entries
            .map
            .iter()
            .map(|(key, entry)| entry.footprint(key))
            .sum()
    }

    /// Seeding a conversation is using it, so the next session's ceiling evicts somebody else.
    ///
    /// Without this, a returning sender's own message left their entry the least recently touched
    /// one in the store: a concurrent session arriving a moment later evicted the conversation
    /// this session was answering, and the delivered answer's append came back `StaleLease`.
    #[test]
    fn seeding_a_conversation_protects_it_from_a_concurrent_session_s_eviction() {
        let store = BoundedConversationStore::new(2);
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let seeded = store.begin(&key("x"), &surface(), window(), start);
        store.begin(&key("y"), &surface(), window(), at(1));

        // The returning sender's next message reads the same generation back.
        let reread = store.begin(&key("x"), &surface(), window(), at(2));
        assert_eq!(reread.cache_key, seeded.cache_key);

        // A third conversation arrives while that session is still running and the ceiling binds.
        store.begin(&key("z"), &surface(), window(), at(3));
        assert_eq!(store.tracked(), 2, "the ceiling holds");

        store
            .commit(
                &key("x"),
                &surface(),
                window(),
                JobRecord::completed("what broke?", "two things"),
                &seeded.cache_key,
                at(4),
            )
            .expect("the conversation this session answered is not the eviction victim");
        assert!(
            store
                .begin(&key("y"), &surface(), window(), at(5))
                .history
                .is_empty(),
            "the idle conversation was the victim instead"
        );
    }

    /// The store's ceiling reads a running total, and the total is the one a full walk produces.
    ///
    /// The ceiling used to be evaluated by encoding every retained conversation to JSON under the
    /// store mutex, on every begin and every commit. A running total is only better than that if
    /// it is exact, so this pins it against a recomputation after seeding, appending and removal.
    #[test]
    fn the_running_byte_total_matches_a_full_recomputation_after_every_change() {
        let store = BoundedConversationStore::new(8);
        let start = Instant::now();
        assert_eq!(total(&store), 0);
        for index in 0..4 {
            let key = key(&format!("c{index}"));
            let seeded = store.begin(&key, &surface(), window(), start);
            assert_eq!(total(&store), recomputed(&store), "after seeding c{index}");
            store
                .commit(
                    &key,
                    &surface(),
                    window(),
                    JobRecord::completed("what broke?", "two things"),
                    &seeded.cache_key,
                    start,
                )
                .expect("live lease");
            assert_eq!(
                total(&store),
                recomputed(&store),
                "after appending c{index}"
            );
        }
        assert!(total(&store) > 0);
        assert!(store.remove(&key("c0"), EvictionReason::GrantChanged));
        assert_eq!(total(&store), recomputed(&store), "after removal");

        // A changed grant drops the entry and its bytes with it.
        store.begin(&key("c1"), &["moved".to_owned()], window(), start);
        assert_eq!(total(&store), recomputed(&store), "after a grant change");
    }
}
