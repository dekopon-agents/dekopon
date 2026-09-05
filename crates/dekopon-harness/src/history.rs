//! Bounded portable execution evidence. Retention is independent of model context selection.

use dekopon_model::model::{ModelMessage, ModelToolCall};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const DEFAULT_MAX_TURNS: usize = 12;
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;
pub const MAX_EXCERPT_BYTES: usize = 4096;
pub(crate) const MAX_EXECUTIONS: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryLimits {
    pub max_turns: usize,
    pub max_bytes: usize,
}
impl HistoryLimits {
    /// Hard turn ceiling [`History::new`] clamps any configured window to.
    ///
    /// Public because a reader that reconstructs a recorded history has to know how many turns the
    /// clamp can silently drop before it reports the reconstruction as complete.
    pub const MAX_TURNS: usize = 128;
    /// Hard retained-byte ceiling [`History::new`] clamps any configured window to.
    pub const MAX_BYTES: usize = 1024 * 1024;
}
impl Default for HistoryLimits {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Transport acceptance is distinct from generation; only Accepted can carry a memory receipt.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum DeliveryDisposition {
    #[default]
    Pending,
    Accepted {
        text: String,
    },
    Suppressed,
    Cancelled,
    Failed,
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionProvenance {
    DirectReadOnly,
    BrokerObserved,
    RecordedReplay,
}

/// Failed work may have effects. Unknown is not a failure that may safely be retried.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionOutcome {
    NotExecuted,
    Denied,
    Succeeded,
    Failed,
    Unknown,
}

/// A bounded text projection and a commitment to the original bytes, never binary assets.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Excerpt {
    pub text: String,
    pub original_bytes: usize,
    pub digest: String,
    pub truncated: bool,
}
impl Excerpt {
    pub(crate) fn render(&self) -> String {
        if self.truncated {
            format!(
                "{}\n[excerpt; original {} bytes, sha256 {}]",
                self.text, self.original_bytes, self.digest
            )
        } else {
            self.text.clone()
        }
    }
    pub(crate) fn new(text: &str, maximum: usize) -> Self {
        let mut end = text.len().min(maximum);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            text: text[..end].to_owned(),
            original_bytes: text.len(),
            digest: digest(text.as_bytes()),
            truncated: end != text.len(),
        }
    }
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    pub job: String,
    pub call: u32,
    pub tool: String,
    pub sequence: u32,
    pub capability: String,
    pub provenance: ExecutionProvenance,
    pub invocation: Option<String>,
    pub evidence: Vec<String>,
    pub outcome: ExecutionOutcome,
    pub result: Option<Excerpt>,
}

/// One assistant batch and its complete results, or an explicitly incomplete portable summary.
/// Provider continuation and intermediate assistant reasoning have no field here.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolGroup {
    pub call: u32,
    pub calls: Vec<ModelToolCall>,
    pub results: Vec<ToolResult>,
    pub omitted: bool,
    pub provenance: Option<ExecutionProvenance>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub id: String,
    pub result: Excerpt,
}
impl ToolGroup {
    pub(crate) fn complete(&self) -> bool {
        !self.omitted
            && self.calls.len() == self.results.len()
            && self
                .calls
                .iter()
                .all(|call| self.results.iter().filter(|r| r.id == call.id).count() == 1)
    }
    pub(crate) fn capture_results(&mut self, messages: &[ModelMessage]) {
        // The host appends one assistant batch before dispatch. Never search older batches by
        // an untrusted provider ID: IDs are only unique within that batch.
        let Some(start) = messages.iter().rposition(|m| m.role() == "assistant") else {
            return;
        };
        for message in &messages[start + 1..] {
            if message.role() != "tool" {
                continue;
            }
            let encoded = serde_json::to_value(message).expect("message serializes");
            if let Some(id) = encoded
                .get("tool_call_id")
                .and_then(serde_json::Value::as_str)
                && self.calls.iter().any(|call| call.id == id)
                && !self.results.iter().any(|r| r.id == id)
            {
                self.results.push(ToolResult {
                    id: id.to_owned(),
                    result: Excerpt::new(
                        message.content().unwrap_or("[non-text result omitted]"),
                        MAX_EXCERPT_BYTES,
                    ),
                });
            }
        }
    }
}

/// A job's user text, generated text, actual delivery, and independently observed executions.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobRecord {
    pub job: String,
    pub user: String,
    pub generated: Option<String>,
    pub delivery: DeliveryDisposition,
    pub executions: Vec<ExecutionRecord>,
    pub groups: Vec<ToolGroup>,
}
impl JobRecord {
    pub(crate) fn new(job: String, user: &str) -> Self {
        Self {
            job,
            user: user.to_owned(),
            generated: None,
            delivery: DeliveryDisposition::Pending,
            executions: Vec::new(),
            groups: Vec::new(),
        }
    }
    /// Imports recorded narrative, never evidence that a capability executed or text was delivered.
    pub fn completed(user: impl Into<String>, answer: impl Into<String>) -> Self {
        let mut record = Self::unanswered(user);
        record.generated = Some(answer.into());
        record
    }
    pub fn unanswered(user: impl Into<String>) -> Self {
        Self::new(crate::checkpoint::opaque_id(), &user.into())
    }
    pub fn user(&self) -> &str {
        &self.user
    }
    pub fn answer(&self) -> Option<&str> {
        self.generated.as_deref()
    }
    pub fn is_answered(&self) -> bool {
        self.generated.is_some()
    }
    pub fn bytes(&self) -> usize {
        // Include structure/IDs, not just text: short execution records also consume memory.
        serde_json::to_vec(self)
            .expect("portable record serializes")
            .len()
    }
    pub fn has_unknown_work(&self) -> bool {
        self.executions
            .iter()
            .any(|r| r.outcome == ExecutionOutcome::Unknown)
    }
}

/// The wire shape of a [`History`]. The cached byte totals are derived, never transported.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryFields {
    turns: Vec<JobRecord>,
    limits: HistoryLimits,
    unresolved: bool,
}
impl From<HistoryFields> for History {
    fn from(fields: HistoryFields) -> Self {
        let sizes = fields
            .turns
            .iter()
            .map(JobRecord::bytes)
            .collect::<Vec<_>>();
        Self {
            bytes: sizes.iter().sum(),
            sizes,
            turns: fields.turns,
            limits: fields.limits,
            unresolved: fields.unresolved,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(from = "HistoryFields")]
pub struct History {
    turns: Vec<JobRecord>,
    limits: HistoryLimits,
    // Never erase unresolved-effect warnings merely because the evidence window was trimmed.
    unresolved: bool,
    /// Encoded size of each retained turn, in `turns` order.
    ///
    /// Maintained where turns are appended and dropped so that [`History::bytes`] — which a store
    /// ceiling evaluates on every append and on every eviction step — never serializes the corpus.
    #[serde(skip)]
    sizes: Vec<usize>,
    /// Running total of `sizes`.
    #[serde(skip)]
    bytes: usize,
}
impl History {
    pub fn new(limits: HistoryLimits) -> Self {
        Self {
            limits: HistoryLimits {
                max_turns: limits.max_turns.min(HistoryLimits::MAX_TURNS),
                max_bytes: limits.max_bytes.min(HistoryLimits::MAX_BYTES),
            },
            ..Self::default()
        }
    }
    #[cfg(test)]
    pub(crate) fn from_turns(
        limits: HistoryLimits,
        turns: impl IntoIterator<Item = JobRecord>,
    ) -> Self {
        let mut history = Self::new(limits);
        for turn in turns {
            history.record(turn);
        }
        history
    }
    pub fn limits(&self) -> HistoryLimits {
        self.limits
    }
    pub fn turns(&self) -> &[JobRecord] {
        &self.turns
    }
    pub fn len(&self) -> usize {
        self.turns.len()
    }
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }
    /// Retained bytes, read from the running total rather than by encoding the corpus.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn has_unknown_work(&self) -> bool {
        self.unresolved || self.turns.iter().any(JobRecord::has_unknown_work)
    }
    pub(crate) fn checkpoint_seed(&self) -> Self {
        let mut seed = self.clone();
        while seed.bytes > 256 * 1024 && !seed.turns.is_empty() {
            seed.drop_oldest();
        }
        seed
    }
    pub fn record(&mut self, mut turn: JobRecord) {
        self.unresolved |= turn.has_unknown_work();
        // Drop whole tool groups first, not result halves. Executions retain their independent
        // provenance/digests even when model-facing call text no longer fits the retention lane.
        let mut size = turn.bytes();
        while size > self.limits.max_bytes && !turn.groups.is_empty() {
            turn.groups.remove(0);
            size = turn.bytes();
        }
        if size > self.limits.max_bytes {
            for execution in &mut turn.executions {
                if let Some(excerpt) = &mut execution.result {
                    excerpt.text.clear();
                    excerpt.truncated = excerpt.original_bytes > 0;
                }
            }
            size = turn.bytes();
        }
        self.turns.push(turn);
        self.sizes.push(size);
        self.bytes += size;
        while self.turns.len() > self.limits.max_turns || self.bytes > self.limits.max_bytes {
            if self.turns.is_empty() {
                break;
            }
            self.drop_oldest();
        }
    }
    /// Drops the oldest retained turn, keeping the running total exact.
    fn drop_oldest(&mut self) {
        self.turns.remove(0);
        self.bytes -= self.sizes.remove(0);
    }
    #[cfg(test)]
    pub(crate) fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        crate::context::WindowContext.replay(self, messages);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(user: &str, answer: &str) -> JobRecord {
        JobRecord::completed(user, answer)
    }

    fn exact(history: &History) -> usize {
        history.turns().iter().map(JobRecord::bytes).sum()
    }

    /// The running total is the number a full re-encoding would produce, at every step.
    ///
    /// `bytes()` is read on every conversation-store append and on every eviction step, so it is
    /// answered from a counter rather than by serializing the corpus. A counter that drifts from
    /// the encoding is worse than the O(corpus) call it replaced, so this pins them equal across
    /// an append, a byte-bound trim, a turn-bound eviction, and a decode.
    #[test]
    fn the_cached_byte_total_equals_the_encoded_size_after_record_trim_and_eviction() {
        let mut history = History::new(HistoryLimits {
            max_turns: 3,
            max_bytes: 64 * 1024,
        });
        assert_eq!(history.bytes(), 0);
        for index in 0..3 {
            history.record(turn(&format!("question {index}"), "answer"));
            assert_eq!(history.bytes(), exact(&history), "after append {index}");
        }
        history.record(turn("question 3", "answer"));
        assert_eq!(history.len(), 3, "the turn bound evicted the oldest");
        assert_eq!(
            history.bytes(),
            exact(&history),
            "after turn-bound eviction"
        );

        let mut narrow = History::new(HistoryLimits {
            max_turns: 8,
            max_bytes: 512,
        });
        for index in 0..8 {
            narrow.record(turn(&"q".repeat(200), &format!("answer {index}")));
            assert_eq!(narrow.bytes(), exact(&narrow), "narrow append {index}");
        }

        let encoded = serde_json::to_string(&history).expect("history serializes");
        let decoded: History = serde_json::from_str(&encoded).expect("history decodes");
        assert_eq!(decoded.bytes(), history.bytes());
        assert_eq!(decoded.bytes(), exact(&decoded));
        assert_eq!(decoded, history);
    }

    /// The checkpoint seed drops whole oldest turns and keeps its own total exact.
    #[test]
    fn the_checkpoint_seed_keeps_the_running_total_exact_while_it_drops_turns() {
        let mut history = History::new(HistoryLimits {
            max_turns: 128,
            max_bytes: HistoryLimits::MAX_BYTES,
        });
        for index in 0..64 {
            history.record(turn(&"q".repeat(8 * 1024), &format!("answer {index}")));
        }
        let seed = history.checkpoint_seed();
        assert!(seed.bytes() <= 256 * 1024, "{}", seed.bytes());
        assert!(seed.len() < history.len());
        assert_eq!(seed.bytes(), exact(&seed));
    }

    /// The clamp is the published constant, so a reader can report what it dropped.
    #[test]
    fn configured_windows_are_clamped_to_the_published_ceilings() {
        let history = History::new(HistoryLimits {
            max_turns: usize::MAX,
            max_bytes: usize::MAX,
        });
        assert_eq!(history.limits().max_turns, HistoryLimits::MAX_TURNS);
        assert_eq!(history.limits().max_bytes, HistoryLimits::MAX_BYTES);
    }
}
