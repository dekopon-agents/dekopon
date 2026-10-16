use std::{
    fmt,
    io::{self, Write},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use arc_swap::ArcSwap;
use dekopon_model::model::{AssistantTurn, ModelMessage, assistant_message};
use serde::{Deserialize, Serialize};

pub use dekopon_context::{Exchange, Representation};
use dekopon_context::{eviction_count, fit_projected};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Pin {
    pub record_id: u64,
    pub representation_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReadRef {
    pub source_record_id: u64,
    pub representation_id: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<(u64, u64)>,
    pub outcome: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Selection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<String>,
    pub available: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    pub authority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub struct ArenaHeadroom {
    limit: usize,
    charged: AtomicUsize,
}

impl ArenaHeadroom {
    #[must_use]
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            charged: AtomicUsize::new(0),
        })
    }

    #[must_use]
    pub fn charged(&self) -> usize {
        self.charged.load(Ordering::Acquire)
    }

    fn charge(self: &Arc<Self>, bytes: usize) -> Result<Charge, ArenaRefusal> {
        self.charged
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |charged| {
                charged
                    .checked_add(bytes)
                    .filter(|total| *total <= self.limit)
            })
            .map_err(|_exhausted| ArenaRefusal::TransientHeadroom)?;
        Ok(Charge {
            headroom: Arc::clone(self),
            bytes,
        })
    }
}

struct Charge {
    headroom: Arc<ArenaHeadroom>,
    bytes: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.headroom
            .charged
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

pub struct ArenaSnapshot {
    bytes: Vec<u8>,
    records: Vec<(u64, Range<usize>)>,
    _charge: Charge,
}

impl fmt::Debug for ArenaSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArenaSnapshot")
            .field("bytes", &self.bytes.len())
            .field("records", &self.records.len())
            .finish()
    }
}

impl ArenaSnapshot {
    #[must_use]
    pub fn read(&self, record_id: u64) -> Option<&str> {
        let (_, range) = self.records.iter().find(|(id, _)| *id == record_id)?;
        std::str::from_utf8(&self.bytes[range.clone()]).ok()
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArenaRefusal {
    TransientHeadroom,
    DuplicateRecord,
}

struct Measured(usize);

impl Write for Measured {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(input.len())
            .ok_or_else(|| io::Error::other("arena size overflow"))?;
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedBytes {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("arena transient headroom exceeded"));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize)]
struct PackedTurn<'a> {
    user: &'a str,
    answer: Option<&'a str>,
    exchanges: &'a [Exchange],
}

impl<'a> PackedTurn<'a> {
    fn of(turn: &'a ConversationTurn) -> Self {
        Self {
            user: turn.user(),
            answer: turn.answer(),
            exchanges: turn.exchanges(),
        }
    }
}

pub struct PackedArena {
    current: ArcSwap<ArenaSnapshot>,
    headroom: Arc<ArenaHeadroom>,
}

impl PackedArena {
    #[must_use]
    pub fn new(headroom: Arc<ArenaHeadroom>) -> Self {
        let empty = ArenaSnapshot {
            bytes: Vec::new(),
            records: Vec::new(),
            _charge: Charge {
                headroom: Arc::clone(&headroom),
                bytes: 0,
            },
        };
        Self {
            current: ArcSwap::from_pointee(empty),
            headroom,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<ArenaSnapshot> {
        self.current.load_full()
    }

    /// Callers serialize repacks of one arena; the shared headroom itself is safe to charge from
    /// any number of arenas at once.
    pub fn repack(&self, records: &[(u64, &ConversationTurn)]) -> Result<(), ArenaRefusal> {
        let mut ids = std::collections::HashSet::new();
        let mut measured = Measured(0);
        for (id, turn) in records {
            if !ids.insert(*id) {
                return Err(ArenaRefusal::DuplicateRecord);
            }
            serde_json::to_writer(&mut measured, &PackedTurn::of(turn))
                .map_err(|_overflow| ArenaRefusal::TransientHeadroom)?;
        }
        let handles_bytes = records
            .len()
            .checked_mul(std::mem::size_of::<(u64, Range<usize>)>())
            .ok_or(ArenaRefusal::TransientHeadroom)?;
        let charge = self.headroom.charge(
            measured
                .0
                .checked_add(handles_bytes)
                .ok_or(ArenaRefusal::TransientHeadroom)?,
        )?;
        let mut bytes = BoundedBytes {
            bytes: Vec::with_capacity(measured.0),
            limit: measured.0,
        };
        let mut handles = Vec::with_capacity(records.len());
        for (id, turn) in records {
            let start = bytes.bytes.len();
            serde_json::to_writer(&mut bytes, &PackedTurn::of(turn))
                .map_err(|_exceeded| ArenaRefusal::TransientHeadroom)?;
            handles.push((*id, start..bytes.bytes.len()));
        }
        self.current.store(Arc::new(ArenaSnapshot {
            bytes: bytes.bytes,
            records: handles,
            _charge: charge,
        }));
        Ok(())
    }
}

pub const DEFAULT_MAX_TURNS: usize = 16;

pub const DEFAULT_MAX_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryLimits {
    pub max_turns: usize,
    pub max_bytes: usize,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawAnswer(String);

impl RawAnswer {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for RawAnswer {
    fn from(answer: String) -> Self {
        Self(answer)
    }
}

impl From<&str> for RawAnswer {
    fn from(answer: &str) -> Self {
        Self(answer.to_owned())
    }
}

/// Stored as plain text, not ModelMessages, because a remembered system message, an orphaned tool
/// result, or provider-specific replay state would each corrupt a later request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationTurn {
    user: String,
    answer: Option<RawAnswer>,
    exchanges: Vec<Exchange>,
    memory_id: Option<u64>,
}

impl ConversationTurn {
    #[must_use]
    pub fn completed(user: impl Into<String>, answer: impl Into<RawAnswer>) -> Self {
        Self {
            user: user.into(),
            answer: Some(answer.into()),
            exchanges: Vec::new(),
            memory_id: None,
        }
    }

    #[must_use]
    pub fn unanswered(user: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            answer: None,
            exchanges: Vec::new(),
            memory_id: None,
        }
    }

    #[must_use]
    pub fn with_exchanges(mut self, exchanges: Vec<Exchange>) -> Self {
        self.exchanges = exchanges;
        self
    }

    #[must_use]
    pub fn with_memory_id(mut self, id: u64) -> Self {
        self.memory_id = Some(id);
        self
    }

    #[must_use]
    pub fn memory_id(&self) -> Option<u64> {
        self.memory_id
    }

    #[must_use]
    pub fn exchanges(&self) -> &[Exchange] {
        &self.exchanges
    }

    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    #[must_use]
    pub fn answer(&self) -> Option<&str> {
        self.answer.as_ref().map(RawAnswer::as_str)
    }

    #[must_use]
    pub fn is_answered(&self) -> bool {
        self.answer.is_some()
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.user
            .len()
            .saturating_add(self.answer.as_ref().map_or(0, |answer| answer.0.len()))
    }

    fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        messages.push(ModelMessage::user(&self.user));
        if let Some(answer) = &self.answer {
            messages.push(assistant_message(&AssistantTurn::new(
                Some(answer.0.clone()),
                Vec::new(),
                None,
            )));
        }
    }
}

/// Kept transport-neutral as text because provider-specific replay state, like ChatGPT's encrypted
/// reasoning, silently disappears with no error when replayed against a different backend.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct History {
    turns: Vec<ConversationTurn>,
    limits: HistoryLimits,
}

impl History {
    #[must_use]
    pub fn new(limits: HistoryLimits) -> Self {
        Self {
            turns: Vec::new(),
            limits,
        }
    }

    #[must_use]
    pub fn from_turns(
        limits: HistoryLimits,
        turns: impl IntoIterator<Item = ConversationTurn>,
    ) -> Self {
        let mut history = Self::new(limits);
        for turn in turns {
            history.record(turn);
        }
        history
    }

    #[must_use]
    pub fn limits(&self) -> HistoryLimits {
        self.limits
    }

    #[must_use]
    pub fn turns(&self) -> &[ConversationTurn] {
        &self.turns
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.turns
            .iter()
            .map(ConversationTurn::bytes)
            .fold(0, usize::saturating_add)
    }

    pub fn record_projected(&mut self, turn: ConversationTurn) -> bool {
        if self.limits.max_turns == 0 {
            if turn.bytes() > self.limits.max_bytes {
                return false;
            }
            self.turns.clear();
            return true;
        }
        let Some(dropped) = fit_projected(
            self.turns.iter().map(ConversationTurn::bytes),
            turn.bytes(),
            self.limits.max_turns,
            self.limits.max_bytes,
        ) else {
            return false;
        };
        self.turns.drain(..dropped);
        self.turns.push(turn);
        true
    }

    pub fn record(&mut self, mut turn: ConversationTurn) {
        const MARKER: &str = "[…]";
        if turn.bytes() > self.limits.max_bytes
            && let Some(prefix_bytes) = self
                .limits
                .max_bytes
                .checked_sub(turn.answer.as_ref().map_or(0, |answer| answer.0.len()))
                .and_then(|remaining| remaining.checked_sub(MARKER.len()))
        {
            turn.user
                .truncate(turn.user.floor_char_boundary(prefix_bytes));
            turn.user.push_str(MARKER);
        }
        self.turns.push(turn);
        self.trim();
    }

    pub(super) fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        for turn in &self.turns {
            turn.replay_into(messages);
        }
    }

    fn trim(&mut self) {
        let dropped = eviction_count(
            self.turns.iter().map(ConversationTurn::bytes),
            self.limits.max_turns,
            self.limits.max_bytes,
        );
        self.turns.drain(..dropped);
    }
}

#[cfg(test)]
mod projection_tests {
    use super::{ConversationTurn, History, HistoryLimits};

    #[test]
    fn zero_turn_limit_records_nothing_without_panicking() {
        let mut history = History::new(HistoryLimits {
            max_turns: 0,
            max_bytes: 10,
        });
        assert!(history.record_projected(ConversationTurn::unanswered("one")));
        assert!(history.is_empty());
    }

    #[test]
    fn fitting_drops_oldest_whole_turns_without_truncating_or_downgrading() {
        let mut history = History::new(HistoryLimits {
            max_turns: 3,
            max_bytes: 10,
        });
        assert!(history.record_projected(ConversationTurn::unanswered("11111")));
        assert!(history.record_projected(ConversationTurn::unanswered("22222")));
        assert!(!history.record_projected(ConversationTurn::unanswered("oversized original")));
        assert!(history.record_projected(ConversationTurn::unanswered("33333")));
        assert_eq!(
            history
                .turns()
                .iter()
                .map(ConversationTurn::user)
                .collect::<Vec<_>>(),
            ["22222", "33333"]
        );
        assert_eq!(history.bytes(), 10);
    }
}

#[cfg(test)]
mod arena_tests {
    use super::{
        ArenaHeadroom, ArenaRefusal, ConversationTurn, Exchange, PackedArena, Representation,
    };

    #[test]
    fn a_reader_survives_publication_with_stable_record_handles() {
        let arena = PackedArena::new(ArenaHeadroom::new(4096));
        let first =
            ConversationTurn::completed("one 🟠", "answered").with_exchanges(vec![Exchange {
                tool: "bash".into(),
                request: vec![Representation::original("echo hi", "v")],
                result: vec![Representation::original("failed", "v")],
                outcome: "failed".into(),
                exit_status: Some(1),
            }]);
        let two = ConversationTurn::completed("two", "answer two");
        let changed = ConversationTurn::completed("new two", "new answer");
        let three = ConversationTurn::completed("three", "answer three");
        arena.repack(&[(11, &first), (12, &two)]).unwrap();
        let old = arena.snapshot();
        arena.repack(&[(12, &changed), (13, &three)]).unwrap();
        let new = arena.snapshot();
        let before: serde_json::Value = serde_json::from_str(old.read(11).unwrap()).unwrap();
        assert_eq!(before["user"], "one 🟠");
        assert_eq!(before["answer"], "answered");
        assert_eq!(before["exchanges"][0]["request"][0]["text"], "echo hi");
        assert_eq!(before["exchanges"][0]["result"][0]["text"], "failed");
        assert_eq!(before["exchanges"][0]["exitStatus"], 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(old.read(12).unwrap()).unwrap()["user"],
            "two"
        );
        assert_eq!(new.read(11), None);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(new.read(12).unwrap()).unwrap()["answer"],
            "new answer"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(new.read(13).unwrap()).unwrap()["user"],
            "three"
        );
    }

    #[test]
    fn held_readers_count_against_headroom_and_refusal_does_not_publish() {
        let arena = PackedArena::new(ArenaHeadroom::new(200));
        let first = ConversationTurn::completed("old", "ok");
        let second = ConversationTurn::completed("next", "ok");
        let third = ConversationTurn::completed("a".repeat(35), "ok");
        arena.repack(&[(1, &first)]).unwrap();
        let old = arena.snapshot();
        arena.repack(&[(2, &second)]).unwrap();
        let new = arena.snapshot();
        assert_eq!(
            arena.repack(&[(3, &third)]),
            Err(ArenaRefusal::TransientHeadroom)
        );
        assert_eq!(
            old.read(1),
            Some(r#"{"user":"old","answer":"ok","exchanges":[]}"#)
        );
        assert_eq!(
            new.read(2),
            Some(r#"{"user":"next","answer":"ok","exchanges":[]}"#)
        );
        assert_eq!(arena.snapshot().read(2), new.read(2));
        drop(old);
        drop(new);
        arena.repack(&[(3, &third)]).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(arena.snapshot().read(3).unwrap()).unwrap()["user"],
            "a".repeat(35)
        );
    }
}
