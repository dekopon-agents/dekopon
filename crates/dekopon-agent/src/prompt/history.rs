use std::{
    fmt,
    io::{self, Write},
    ops::Range,
    sync::{Arc, Weak},
};

use arc_swap::ArcSwap;
use dekopon_model::model::{AssistantTurn, ModelMessage, assistant_message};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Representation {
    pub routine_id: String,
    pub version: String,
    pub source_record_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    pub kind: String,
    pub captured_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Exchange {
    pub tool: String,
    pub request: Vec<Representation>,
    pub result: Vec<Representation>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

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

#[derive(Clone, Default, Eq, PartialEq)]
pub struct ArenaSnapshot {
    bytes: Vec<u8>,
    records: Vec<(u64, Range<usize>)>,
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

    fn allocated_bytes(&self) -> usize {
        self.bytes.capacity().saturating_add(
            self.records
                .capacity()
                .saturating_mul(std::mem::size_of::<(u64, Range<usize>)>()),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArenaRefusal {
    TransientHeadroom,
    DuplicateRecord,
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
        if self.bytes.capacity() > self.limit {
            return Err(io::Error::other("arena transient headroom exceeded"));
        }
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

pub struct PackedArena {
    current: ArcSwap<ArenaSnapshot>,
    publication: Mutex<Vec<Weak<ArenaSnapshot>>>,
    max_transient_bytes: usize,
}

impl PackedArena {
    #[must_use]
    pub fn new(max_transient_bytes: usize) -> Self {
        Self {
            current: ArcSwap::from_pointee(ArenaSnapshot::default()),
            publication: Mutex::new(Vec::new()),
            max_transient_bytes,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<ArenaSnapshot> {
        self.current.load_full()
    }

    pub fn repack(&self, records: &[(u64, &ConversationTurn)]) -> Result<(), ArenaRefusal> {
        let mut live = self.publication.lock();
        live.retain(|snapshot| snapshot.strong_count() != 0);
        let current = self.current.load_full();
        let resident = live
            .iter()
            .filter_map(Weak::upgrade)
            .fold(current.allocated_bytes(), |sum, snapshot| {
                sum.saturating_add(snapshot.allocated_bytes())
            });
        let handles_bytes = records
            .len()
            .checked_mul(std::mem::size_of::<(u64, Range<usize>)>())
            .ok_or(ArenaRefusal::TransientHeadroom)?;
        if handles_bytes > self.max_transient_bytes.saturating_sub(resident) {
            return Err(ArenaRefusal::TransientHeadroom);
        }
        let mut bytes = BoundedBytes {
            bytes: Vec::new(),
            limit: self.max_transient_bytes - resident - handles_bytes,
        };
        let mut handles = Vec::with_capacity(records.len());
        let mut ids = std::collections::HashSet::new();
        for (id, turn) in records {
            if !ids.insert(*id) {
                return Err(ArenaRefusal::DuplicateRecord);
            }
            let start = bytes.bytes.len();
            serde_json::to_writer(
                &mut bytes,
                &PackedTurn {
                    user: turn.user(),
                    answer: turn.answer(),
                    exchanges: turn.exchanges(),
                },
            )
            .map_err(|_serialize| ArenaRefusal::TransientHeadroom)?;
            handles.push((*id, start..bytes.bytes.len()));
        }
        let next = Arc::new(ArenaSnapshot {
            bytes: bytes.bytes,
            records: handles,
        });
        let previous = self.current.swap(next);
        if Arc::strong_count(&previous) > 1 {
            live.push(Arc::downgrade(&previous));
        }
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

impl Representation {
    #[must_use]
    pub fn original(text: &str, version: &str) -> Self {
        Self {
            routine_id: "original".into(),
            version: version.into(),
            source_record_id: 0,
            part: None,
            kind: "original".into(),
            captured_bytes: text.len() as u64,
            text: Some(text.into()),
        }
    }

    #[must_use]
    pub fn prefix(text: &str, max_bytes: usize, routine_id: &str, version: &str) -> Self {
        let cut = text.floor_char_boundary(max_bytes.min(text.len()));
        Self {
            routine_id: routine_id.into(),
            version: version.into(),
            source_record_id: 0,
            part: None,
            kind: "prefix".into(),
            captured_bytes: text.len() as u64,
            text: Some(text[..cut].into()),
        }
    }

    #[must_use]
    pub fn omitted_bytes(&self) -> u64 {
        self.captured_bytes
            .saturating_sub(self.text.as_ref().map_or(0, |text| text.len() as u64))
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
}

impl ConversationTurn {
    #[must_use]
    pub fn completed(user: impl Into<String>, answer: impl Into<RawAnswer>) -> Self {
        Self {
            user: user.into(),
            answer: Some(answer.into()),
            exchanges: Vec::new(),
        }
    }

    #[must_use]
    pub fn unanswered(user: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            answer: None,
            exchanges: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_exchanges(mut self, exchanges: Vec<Exchange>) -> Self {
        self.exchanges = exchanges;
        self
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
        if self.turns.len() > self.limits.max_turns {
            let excess = self.turns.len() - self.limits.max_turns;
            self.turns.drain(..excess);
        }

        let mut bytes = self.bytes();
        let mut dropped = 0;
        for turn in &self.turns {
            if bytes <= self.limits.max_bytes {
                break;
            }
            bytes = bytes.saturating_sub(turn.bytes());
            dropped += 1;
        }
        self.turns.drain(..dropped);
    }
}

#[cfg(test)]
mod arena_tests {
    use super::{ArenaRefusal, ConversationTurn, Exchange, PackedArena, Representation};

    #[test]
    fn a_reader_survives_publication_with_stable_record_handles() {
        let arena = PackedArena::new(4096);
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
        let arena = PackedArena::new(270);
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
