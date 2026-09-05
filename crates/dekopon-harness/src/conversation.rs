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
struct Conversation {
    history: History,
    surface: Vec<String>,
    cache_key: String,
    touched: Instant,
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
    entries: Mutex<HashMap<ConversationKey, Conversation>>,
}
impl BoundedConversationStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.min(4096),
            entries: Mutex::new(HashMap::new()),
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
        let mut entries = self.entries.lock().expect("conversation store");
        if let Some(reason) = entries.get(key).and_then(|entry| {
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
        let entry = entries.entry(key.clone()).or_insert_with(|| Conversation {
            history: History::new(window.limits),
            surface: surface.to_vec(),
            cache_key: format!("dekopond-conversation-{}", crate::checkpoint::opaque_id()),
            touched: now,
        });
        let seed = ConversationSeed {
            history: entry.history.clone(),
            cache_key: entry.cache_key.clone(),
        };
        self.enforce_ceiling(&mut entries);
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
        let mut entries = self.entries.lock().expect("conversation store");
        let entry = entries.get_mut(key).ok_or(StaleLease)?;
        if entry.cache_key != generation
            || entry.surface != surface
            || now.saturating_duration_since(entry.touched) >= window.idle_timeout
        {
            return Err(StaleLease);
        }
        entry.history.record(turn);
        entry.touched = now;
        self.enforce_ceiling(&mut entries);
        Ok(())
    }
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
    fn enforce_ceiling(&self, entries: &mut HashMap<ConversationKey, Conversation>) {
        while entries.len() > self.capacity
            || entries
                .iter()
                .map(|(k, e)| {
                    e.history.bytes()
                        + e.surface.iter().map(String::len).sum::<usize>()
                        + e.cache_key.len()
                        + k.agent.len()
                        + k.route.len()
                        + k.transport.len()
                        + k.channel.len()
                        + k.conversation.len()
                        + k.subject.len()
                })
                .sum::<usize>()
                > 64 * 1024 * 1024
        {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            entries.remove(&oldest);
            evicted(EvictionReason::Capacity);
        }
    }
    pub fn tracked(&self) -> usize {
        self.entries.lock().expect("conversation store").len()
    }
}
impl fmt::Debug for BoundedConversationStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.lock().expect("conversation store");
        f.debug_struct("BoundedConversationStore")
            .field("capacity", &self.capacity)
            .field("conversations", &entries.len())
            .field(
                "turns",
                &entries.values().map(|e| e.history.len()).sum::<usize>(),
            )
            .field(
                "bytes",
                &entries.values().map(|e| e.history.bytes()).sum::<usize>(),
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
