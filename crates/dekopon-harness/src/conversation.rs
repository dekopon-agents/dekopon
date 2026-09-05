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
    /// When the generation a session is still answering under was handed out, if there is one.
    ///
    /// Eviction prefers a conversation nobody is answering: evicting one a *concurrent* session
    /// holds the generation for rotates its cache key, so the answer that session is about to
    /// deliver is refused as a stale lease and a person hears nothing. The `commit` that ends the
    /// generation clears it, and a mark older than the idle timeout is ignored, so a session that
    /// never committed cannot protect an entry forever and the ceiling always has a victim.
    generation_started: Option<Instant>,
}
impl Conversation {
    /// Whether a session is still answering under this conversation's current generation.
    fn answering(&self, now: Instant, idle_timeout: Duration) -> bool {
        self.generation_started
            .is_some_and(|started| now.saturating_duration_since(started) < idle_timeout)
    }
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
                generation_started: None,
            };
            seeded.bytes = seeded.footprint(key);
            entries.insert(key.clone(), seeded);
        }
        let entry = entries.map.get_mut(key).expect("seeded conversation");
        // Seeding a turn is using the conversation. Without this the entry the caller is about to
        // answer under is the least recently touched one, so the ceiling evicts the conversation
        // this call just seeded and the delivered answer's `commit` is refused as a stale lease.
        entry.touched = now;
        // The caller is about to answer under this generation, and stays that way until it
        // commits: from here it is a conversation somebody is in the middle of, not a resident one.
        entry.generation_started = Some(now);
        let seed = ConversationSeed {
            history: entry.history.clone(),
            cache_key: entry.cache_key.clone(),
        };
        self.enforce_ceiling(entries, Some(key), now, window.idle_timeout);
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
        // The generation this append completes is over; the conversation is resident again.
        entry.generation_started = None;
        let updated = entry.footprint(key);
        let previous = std::mem::replace(&mut entry.bytes, updated);
        entries.bytes = entries.bytes + updated - previous;
        self.enforce_ceiling(entries, Some(key), now, window.idle_timeout);
        Ok(())
    }
    pub fn remove(&self, key: &ConversationKey, reason: EvictionReason) -> bool {
        let removed = self.entries.lock().expect("conversation store").remove(key);
        if removed {
            evicted(reason);
        }
        removed
    }
    /// Drops conversations until both ceilings hold, least recently touched first.
    ///
    /// `active` is the conversation the calling session is answering under. It is never the
    /// victim: evicting it would rotate the generation the caller already holds, so the answer it
    /// is about to deliver could not be appended to the history it was built from. A conversation
    /// a *concurrent* session is still answering under loses the same answer the same way, so one
    /// nobody is answering is always taken first; when every candidate has a session in flight the
    /// least recently touched still goes, because the ceiling is a bound before it is a courtesy.
    fn enforce_ceiling(
        &self,
        entries: &mut Entries,
        active: Option<&ConversationKey>,
        now: Instant,
        idle_timeout: Duration,
    ) {
        while entries.map.len() > self.capacity || entries.bytes > MAX_STORE_BYTES {
            let Some(oldest) = entries
                .map
                .iter()
                .filter(|(key, _)| active != Some(*key))
                .min_by_key(|(_, e)| (e.answering(now, idle_timeout), e.touched))
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            entries.remove(&oldest);
            evicted(EvictionReason::Capacity);
        }
    }
    /// How many conversations are resident, against the capacity this store was built with.
    ///
    /// The only thing this store will say about what it holds: never a key, a surface, a
    /// generation or any text. `dekopond` reads it exactly once per run, on the `gateway_stopped`
    /// record at exit, so an operator sizing `sessions.maxConversations` has the denominator for
    /// the `gateway_conversation_evicted` churn. Deliberately not reported per message: a size
    /// published that often would be one more place a live conversation could be described.
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

    /// A conversation another session is still answering is not the ceiling's victim either.
    ///
    /// Excluding only the *calling* session's conversation left the concurrent case open: a third
    /// sender's `begin` evicted the conversation an in-flight session was answering, rotating its
    /// generation, and that session's delivered answer came back `StaleLease` with nothing but a
    /// warn. Eviction now takes a conversation nobody is answering first.
    #[test]
    fn a_conversation_another_session_is_answering_is_not_the_eviction_victim() {
        let store = BoundedConversationStore::new(2);
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);

        // One session begins and keeps running; a second begins, answers, and is done.
        let in_flight = store.begin(&key("x"), &surface(), window(), start);
        let finished = store.begin(&key("y"), &surface(), window(), at(1));
        store
            .commit(
                &key("y"),
                &surface(),
                window(),
                JobRecord::completed("what broke?", "one thing"),
                &finished.cache_key,
                at(2),
            )
            .expect("live lease");

        // A third conversation arrives at the ceiling. `y` is more recently touched than `x`, but
        // nobody is answering under it, so it is the one that goes.
        store.begin(&key("z"), &surface(), window(), at(3));
        assert_eq!(store.tracked(), 2, "the ceiling holds");
        store
            .commit(
                &key("x"),
                &surface(),
                window(),
                JobRecord::completed("what broke?", "two things"),
                &in_flight.cache_key,
                at(4),
            )
            .expect("the in-flight session's answer is still appendable");
        assert!(
            store
                .begin(&key("y"), &surface(), window(), at(5))
                .history
                .is_empty(),
            "the conversation nobody was answering was the victim instead"
        );
    }

    /// The byte ceiling picks its victim the same way the entry ceiling does.
    ///
    /// `enforce_ceiling` is one loop over two ceilings, and every other test here drives the entry
    /// count. A store that reached `MAX_STORE_BYTES` first — the ceiling a deployment with a large
    /// `maxConversations` and long windows actually reaches — took the same victim search, but
    /// nothing said so, and the entry-count tests would still pass if the byte arm had been
    /// written to evict the arriving conversation.
    #[test]
    fn the_retained_byte_ceiling_never_evicts_the_arriving_conversation() {
        let store = BoundedConversationStore::new(4096);
        let now = Instant::now();
        // The widest window a route can configure, so the store reaches 64 MiB in a few dozen
        // conversations rather than a thousand.
        let wide = ConversationWindow {
            idle_timeout: Duration::from_secs(900),
            limits: HistoryLimits {
                max_turns: 8,
                max_bytes: HistoryLimits::MAX_BYTES,
            },
        };
        let bulky = || {
            JobRecord::completed(
                "what broke?".to_owned(),
                "b".repeat(HistoryLimits::MAX_BYTES / 2),
            )
        };
        // Sized from one conversation's measured footprint rather than from the window, because
        // what the ceiling counts is the retained encoding, not the configured bound.
        let probe = key("probe");
        let seeded = store.begin(&probe, &surface(), wide, now);
        store
            .commit(&probe, &surface(), wide, bulky(), &seeded.cache_key, now)
            .expect("live lease");
        let filling = 2 + MAX_STORE_BYTES / total(&store);
        assert!(filling < 256, "the fixture stays small: {filling}");
        for index in 0..filling {
            let key = key(&format!("c{index}"));
            let seeded = store.begin(&key, &surface(), wide, now);
            store
                .commit(&key, &surface(), wide, bulky(), &seeded.cache_key, now)
                .expect("live lease");
        }
        assert!(
            store.tracked() < filling,
            "the byte ceiling bound before the entry ceiling did: {} of {filling} retained",
            store.tracked()
        );
        assert!(total(&store) <= MAX_STORE_BYTES, "{} bytes", total(&store));

        // A returning sender arrives with the store already at the byte ceiling. Its own seeding
        // pushes the total over, so eviction runs inside its own `begin`.
        let returning = key("returning");
        let seeded = store.begin(&returning, &surface(), wide, now);
        assert!(total(&store) <= MAX_STORE_BYTES);
        store
            .commit(
                &returning,
                &surface(),
                wide,
                bulky(),
                &seeded.cache_key,
                now,
            )
            .expect("the arriving conversation is never its own eviction victim");
        assert_eq!(total(&store), recomputed(&store), "the total stays exact");
    }

    /// A generation nobody ever committed stops protecting its conversation once it goes idle.
    ///
    /// Otherwise a session that failed without appending would leave its conversation permanently
    /// unevictable, and a store of those has no victim at all — a ceiling that cannot be enforced.
    #[test]
    fn an_abandoned_generation_stops_protecting_its_conversation() {
        let store = BoundedConversationStore::new(1);
        let start = Instant::now();
        store.begin(&key("abandoned"), &surface(), window(), start);
        // Past the idle timeout the mark means nothing, so the ceiling evicts as it always did.
        store.begin(
            &key("later"),
            &surface(),
            window(),
            start + window().idle_timeout + Duration::from_secs(1),
        );
        assert_eq!(store.tracked(), 1, "the ceiling still has a victim");
        assert!(
            store
                .begin(
                    &key("abandoned"),
                    &surface(),
                    window(),
                    start + window().idle_timeout + Duration::from_secs(2)
                )
                .history
                .is_empty(),
            "the abandoned conversation was evicted"
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
