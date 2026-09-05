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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    turns: Vec<JobRecord>,
    limits: HistoryLimits,
    // Never erase unresolved-effect warnings merely because the evidence window was trimmed.
    unresolved: bool,
}
impl History {
    pub fn new(limits: HistoryLimits) -> Self {
        Self {
            turns: Vec::new(),
            limits: HistoryLimits {
                max_turns: limits.max_turns.min(128),
                max_bytes: limits.max_bytes.min(1024 * 1024),
            },
            unresolved: false,
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
    pub fn bytes(&self) -> usize {
        self.turns.iter().map(JobRecord::bytes).sum()
    }
    pub fn has_unknown_work(&self) -> bool {
        self.unresolved || self.turns.iter().any(JobRecord::has_unknown_work)
    }
    pub(crate) fn checkpoint_seed(&self) -> Self {
        let mut seed = self.clone();
        while seed.bytes() > 256 * 1024 && !seed.turns.is_empty() {
            seed.turns.remove(0);
        }
        seed
    }
    pub fn record(&mut self, mut turn: JobRecord) {
        self.unresolved |= turn.has_unknown_work();
        // Drop whole tool groups first, not result halves. Executions retain their independent
        // provenance/digests even when model-facing call text no longer fits the retention lane.
        while turn.bytes() > self.limits.max_bytes && !turn.groups.is_empty() {
            turn.groups.remove(0);
        }
        if turn.bytes() > self.limits.max_bytes {
            for execution in &mut turn.executions {
                if let Some(excerpt) = &mut execution.result {
                    excerpt.text.clear();
                    excerpt.truncated = excerpt.original_bytes > 0;
                }
            }
        }
        self.turns.push(turn);
        while self.turns.len() > self.limits.max_turns || self.bytes() > self.limits.max_bytes {
            if self.turns.is_empty() {
                break;
            }
            self.turns.remove(0);
        }
    }
    #[cfg(test)]
    pub(crate) fn replay_into(&self, messages: &mut Vec<ModelMessage>) {
        crate::context::WindowContext.replay(self, messages);
    }
}
