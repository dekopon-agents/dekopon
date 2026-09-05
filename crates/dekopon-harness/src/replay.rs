//! Replaying a recorded session against a model, with the operator's changes applied.
//!
//! A session that already happened is the cheapest evaluation an operator can run: the prompt is
//! real, the scripts the model wrote are known, and every script's output was recorded. Replay
//! puts the same prompt to a model again — usually with different standing instructions, a newly
//! mounted skill, or a different model — and answers each script the model writes from the
//! recording, so no capability runs and no effect happens.
//!
//! What replay honestly cannot do is invent tool output. The moment the replayed model writes a
//! script the recorded session never ran, the recording has nothing to answer it with. That point
//! is the *divergence*, and it is reported rather than papered over: by default the replay stops
//! there, and with a live runtime supplied the script runs for real and the report says so. The
//! turns before the divergence are a faithful comparison; the ones after it are a new session.
//!
//! The recording itself is reconstructed from the transcript events `docs/observability.md`
//! defines, which exist only when payload telemetry was enabled for the original session. A
//! session recorded metadata-only has turn counts and token usage but no transcript, and replay
//! says so rather than guessing at the prompt.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

use dekopon_config::Skill;
use dekopon_model::model::{ChatModel, ModelUsage};
use dekopon_shell::{ExitCode, ScriptOutcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

mod context;
pub use context::{RecordedContext, RecordedMessage};

use crate::{
    bootstrap::{self, BootstrapError, CapabilitySnapshot, SessionBootstrap},
    history::{History, HistoryLimits},
    improvement::ImprovementSuggestion,
    runtime::ScriptRuntime,
    session::{CancellationProbe, PromptError, PromptLimits, SessionEngine},
    skills,
    tools::SCRIPT_TOOL_NAME,
};

/// One session as its transcript recorded it.
///
/// This is also the on-disk shape `dekopon-run session show --json` prints and `session replay
/// --from-file` reads back, so a recording can be kept, edited, and replayed without a telemetry
/// backend in the loop.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordedSession {
    /// On-disk shape version, checked before any other field. Files written before this field
    /// existed are read as version 1, which is what this crate has always written.
    #[serde(default = "legacy_recording_version")]
    pub version: u32,
    /// The telemetry trace the records were read from.
    pub trace_id: String,
    /// Leading system messages, in order: standing instructions, then any skills listing.
    #[serde(default)]
    pub system: Vec<String>,
    /// Exchanges replayed ahead of the prompt on a persistent route, oldest first.
    #[serde(default)]
    pub history: Vec<RecordedExchange>,
    /// Portable full/delta request contexts, ordered by model turn. Historical files omit these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<RecordedContext>,
    /// The message this session answered.
    pub prompt: String,
    /// Every model turn, in order.
    #[serde(default)]
    pub turns: Vec<RecordedTurn>,
    /// Independent call accounting, including failed/no-answer calls and images. Absent only in
    /// historical transcript files; an empty current list means unknown, never free inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<Vec<RecordedAccountingCall>>,
    /// The final answer, when the session produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// The only on-disk shape this crate has ever written, and the only one it reads.
pub const RECORDING_VERSION: u32 = 1;

fn legacy_recording_version() -> u32 {
    RECORDING_VERSION
}

impl Default for RecordedSession {
    fn default() -> Self {
        Self {
            version: RECORDING_VERSION,
            trace_id: String::new(),
            system: Vec::new(),
            history: Vec::new(),
            contexts: Vec::new(),
            prompt: String::new(),
            turns: Vec::new(),
            calls: None,
            answer: None,
        }
    }
}

/// A job/call coordinate is independent of any provider assistant/tool ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordedAccountingCall {
    pub job: String,
    pub sequence: u64,
    pub kind: String,
    pub usage: RecordedUsage,
}

/// One remembered exchange a persistent route replayed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedExchange {
    /// What the person asked.
    pub user: String,
    /// What the agent answered, absent when that session produced no answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// One model turn: what the model said, what it called, and what each call was answered with.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTurn {
    /// One-based turn number.
    pub turn: u32,
    /// Assistant text, when the turn carried any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Tool calls the turn requested, with the result each received.
    #[serde(default)]
    pub tool_calls: Vec<RecordedToolCall>,
    /// Provider-reported usage, from the accounting record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<RecordedUsage>,
    /// Model round-trip duration, from the accounting record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
}

/// One tool call and the result the loop handed back.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedToolCall {
    /// The endpoint-assigned call identifier.
    pub id: String,
    /// The tool the model named.
    pub name: String,
    /// The JSON argument text.
    pub arguments: String,
    /// The tool result, absent when the session ended before answering it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl RecordedToolCall {
    /// The script this call ran, when it called the scripting tool with a well-formed argument.
    #[must_use]
    pub fn script(&self) -> Option<String> {
        if self.name != SCRIPT_TOOL_NAME {
            return None;
        }
        serde_json::from_str::<Value>(&self.arguments)
            .ok()?
            .get("script")?
            .as_str()
            .map(str::to_owned)
    }
}

/// Token accounting as the transcript recorded it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedUsage {
    /// Tokens the request consumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// The subset served from the provider's prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Tokens the response produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// The subset spent on reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
    /// Provider-reported total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

impl RecordedUsage {
    fn add(&mut self, other: &Self) {
        fn sum(total: &mut Option<u64>, value: Option<u64>) {
            *total = total.zip(value).and_then(|(a, b)| a.checked_add(b));
        }
        sum(&mut self.input_tokens, other.input_tokens);
        sum(&mut self.cached_input_tokens, other.cached_input_tokens);
        sum(&mut self.output_tokens, other.output_tokens);
        sum(
            &mut self.reasoning_output_tokens,
            other.reasoning_output_tokens,
        );
        sum(&mut self.total_tokens, other.total_tokens);
    }
}

impl From<ModelUsage> for RecordedUsage {
    fn from(usage: ModelUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            total_tokens: usage.total_tokens,
        }
    }
}

/// Why a set of telemetry records did not yield a replayable session.
#[derive(Debug, Error)]
pub enum RecordingError {
    /// Nothing carried the requested trace.
    #[error("no telemetry records were found for trace {trace_id}")]
    NoRecords {
        /// The trace asked for.
        trace_id: String,
    },
    /// The session ran, but its transcript was never exported.
    #[error(
        "trace {trace_id} has {turns} accounted model turn(s) but no transcript; the session was recorded with payload telemetry off, so its prompt and scripts cannot be replayed"
    )]
    NoTranscript {
        /// The trace asked for.
        trace_id: String,
        /// Accounted model turns found.
        turns: usize,
    },
    /// A transcript event carried something the loop never writes.
    #[error("transcript for trace {trace_id} is malformed: {detail}")]
    Malformed {
        /// The trace asked for.
        trace_id: String,
        /// What was wrong, naming the event. Several problems are reported together.
        detail: String,
    },
    /// The recording file declares a shape this build does not read.
    #[error(
        "recording for trace {trace_id} declares version {version}; this build reads version {supported}"
    )]
    UnsupportedVersion {
        /// The trace asked for.
        trace_id: String,
        /// The version the file declared.
        version: u32,
        /// The version this build reads.
        supported: u32,
    },
}

/// Collects every problem a reconstruction found, so one failure names all of them.
///
/// A recording is edited by hand and rebuilt from rows a backend returned in any order; stopping at
/// the first conflict makes fixing one a round trip per problem.
#[derive(Default)]
struct Problems(Vec<String>);

impl Problems {
    fn push(&mut self, detail: impl Into<String>) {
        self.0.push(detail.into());
    }
    fn extend(&mut self, detail: Result<(), String>) {
        if let Err(detail) = detail {
            self.push(detail);
        }
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Every problem in one message, deduplicated: repeated exports repeat their conflicts.
    fn detail(mut self) -> String {
        self.0.sort_unstable();
        self.0.dedup();
        self.0.join("; ")
    }
}

/// Reads one attribute off a flattened telemetry record.
///
/// The loop writes `audit.event`; an OTLP backend may store the attribute under that name or with
/// the dots replaced, so both spellings are read.
fn field<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    if let Some(value) = record.get(name) {
        return Some(value);
    }
    let flattened = name.replace('.', "_");
    record.get(flattened.as_str())
}

fn text(record: &Value, name: &str) -> Option<String> {
    match field(record, name)? {
        Value::String(value) => Some(value.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn unsigned(record: &Value, name: &str) -> Option<u64> {
    match field(record, name)? {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.trim().parse().ok(),
        _ => None,
    }
}

fn float(record: &Value, name: &str) -> Option<f64> {
    match field(record, name)? {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.trim().parse().ok(),
        _ => None,
    }
}

impl RecordedSession {
    /// Recorded chat calls, or `None` when nothing in the recording says how many there were.
    ///
    /// An empty `calls` list is unknown, never zero: it is what a session whose accounting rows the
    /// receiver did not return looks like, and reporting `0` there reads as free inference. A file
    /// written before independent call accounting existed carries no `calls` at all, and its
    /// answered turns are then the honest count.
    fn model_turns(&self) -> Option<u32> {
        let count = match &self.calls {
            // A call set naming no chat call at all — empty, or image calls only, which is what a
            // truncated or hand-edited recording produces — says nothing about how many chat calls
            // this session made. Zero there would read as free inference.
            Some(calls) => match calls.iter().filter(|call| call.kind == "chat").count() {
                0 => return None,
                chat => chat,
            },
            None if self.turns.is_empty() => return None,
            None => self.turns.len(),
        };
        Some(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// Reconstructs one session from the log records exported under its trace.
    ///
    /// Records may arrive in any order and may include events from other sessions; only the
    /// transcript and accounting events of `trace_id` are read. Full/delta request revisions remain
    /// separate from answered turns and independent calls, so rebuilt or trimmed portable context
    /// neither duplicates work nor loses earlier observed results.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingError`] when no record carries the trace, when the session was recorded
    /// without payload telemetry, or when a transcript event is not the shape the loop writes.
    pub fn from_records(trace_id: &str, records: &[Value]) -> Result<Self, RecordingError> {
        let owned = records
            .iter()
            .filter(|record| text(record, "trace_id").is_some_and(|id| id == trace_id))
            .collect::<Vec<_>>();
        if owned.is_empty() {
            return Err(RecordingError::NoRecords {
                trace_id: trace_id.to_owned(),
            });
        }
        let malformed = |detail: String| RecordingError::Malformed {
            trace_id: trace_id.to_owned(),
            detail,
        };
        let mut problems = Problems::default();

        let mut prompts = BTreeMap::<u64, RecordedContext>::new();
        let mut answers = BTreeMap::<u64, (String, String)>::new();
        let mut accounting = BTreeMap::<u64, (Option<RecordedUsage>, Option<f64>)>::new();
        let mut coordinates = BTreeMap::new();
        let mut job = None;
        let mut calls = BTreeMap::<(String, u64), RecordedAccountingCall>::new();
        for record in owned {
            let Some(event) = text(record, "audit.event") else {
                continue;
            };
            let turn = unsigned(record, "model.turn");
            match event.as_str() {
                "agent.model.prompt" => {
                    let Some(turn) = turn else {
                        problems.push("agent.model.prompt without model.turn");
                        continue;
                    };
                    if let Some(id) = text(record, "job.id") {
                        if job.as_ref().is_some_and(|old| old != &id) {
                            problems.push("conflicting prompt job IDs");
                        }
                        job = Some(id);
                    }
                    let context = match context::decode_prompt(record, turn) {
                        Ok(context) => context,
                        Err(detail) => {
                            problems.push(detail);
                            continue;
                        }
                    };
                    if let Some(old) = prompts.insert(turn, context.clone())
                        && old != context
                    {
                        problems.push(format!("conflicting prompt records for turn {turn}"));
                    }
                }
                "agent.model.answer" => {
                    let Some(turn) = turn else {
                        problems.push("agent.model.answer without model.turn");
                        continue;
                    };
                    let answer = (
                        text(record, "answer").unwrap_or_default(),
                        text(record, "tool_calls").unwrap_or_else(|| "[]".to_owned()),
                    );
                    if let Some(old) = answers.insert(turn, answer.clone())
                        && old != answer
                    {
                        problems.push(format!("conflicting answer records for turn {turn}"));
                    }
                }
                "accounting.model.call" | "accounting.model.turn" => {
                    let usage = RecordedUsage {
                        input_tokens: unsigned(record, "usage.input_tokens"),
                        cached_input_tokens: unsigned(record, "usage.cached_input_tokens"),
                        output_tokens: unsigned(record, "usage.output_tokens"),
                        reasoning_output_tokens: unsigned(record, "usage.reasoning_output_tokens"),
                        total_tokens: unsigned(record, "usage.total_tokens"),
                    };
                    let coordinate = if event == "accounting.model.call" {
                        match (text(record, "job.id"), unsigned(record, "call.sequence")) {
                            (Some(job), Some(sequence)) => Some((job, sequence)),
                            (None, _) => {
                                problems.push("accounting.model.call without job.id");
                                None
                            }
                            (Some(_), None) => {
                                problems.push("accounting.model.call without call.sequence");
                                None
                            }
                        }
                    } else {
                        match turn {
                            Some(turn) => Some((format!("historical-{trace_id}"), turn)),
                            None => {
                                problems.push("accounting.model.turn without model.turn");
                                None
                            }
                        }
                    };
                    let Some((job, sequence)) = coordinate else {
                        continue;
                    };
                    let kind = text(record, "model.kind").unwrap_or_else(|| "chat".into());
                    let call = RecordedAccountingCall {
                        job: job.clone(),
                        sequence,
                        kind: kind.clone(),
                        usage,
                    };
                    if let Some(old) = calls.insert((job, sequence), call.clone())
                        && old != call
                    {
                        problems.push("conflicting accounting call records");
                    }
                    if kind != "image"
                        && let Some(turn) = turn
                    {
                        let usage = (usage != RecordedUsage::default()).then_some(usage);
                        if let Some(old) = coordinates.insert(turn, (call.job.clone(), sequence))
                            && old != (call.job.clone(), sequence)
                        {
                            problems.push(format!("conflicting chat calls for turn {turn}"));
                        }
                        // A duplicate export is idempotent only if it agrees about the round trip
                        // too; a last-wins duration silently picks one of two disagreeing rows.
                        let observation = (usage, float(record, "duration_ms"));
                        if let Some(old) = accounting.insert(turn, observation)
                            && old != observation
                        {
                            problems.push(format!(
                                "conflicting accounting observations for turn {turn}"
                            ));
                        }
                    }
                }
                _ => {}
            }
        }

        if prompts.is_empty() {
            if !problems.is_empty() {
                return Err(malformed(problems.detail()));
            }
            return Err(RecordingError::NoTranscript {
                trace_id: trace_id.to_owned(),
                turns: accounting.len(),
            });
        }
        let contexts: Vec<_> = prompts.into_values().collect();
        problems.extend(context::validate_contexts(&contexts));
        if !problems.is_empty() {
            return Err(malformed(problems.detail()));
        }
        let first = &contexts[0].messages;
        let system_end = first.iter().take_while(|m| m.role == "system").count();
        let system = first[..system_end]
            .iter()
            .map(RecordedMessage::text)
            .collect();
        // The first request ends at the inbound prompt, not an answer-content heuristic: a prior
        // answer may be byte-identical to the new one, and evidence summaries also use user roles.
        let prompt = first.last().expect("validated first request").text();
        let history = context::legacy_exchanges(&first[system_end..first.len() - 1]);
        let mut turns = Vec::new();
        for (number, (answer, encoded)) in answers {
            if !contexts.iter().any(|c| u64::from(c.turn) == number) {
                problems.push(format!("answer without prompt for turn {number}"));
                continue;
            }
            let calls =
                match serde_json::from_str::<Vec<dekopon_model::model::ModelToolCall>>(&encoded) {
                    Ok(calls) => calls,
                    Err(error) => {
                        problems.push(format!("turn {number} tool calls: {error}"));
                        continue;
                    }
                };
            // The writer caps a turn at MAX_TOOL_CALLS_PER_TURN, so a row claiming more is not a
            // transcript this loop wrote. Refuse it before the reconstruction iterates it: the
            // correlation walk is contexts × messages × turns × calls.
            if calls.len() > crate::tools::MAX_TOOL_CALLS_PER_TURN {
                problems.push(format!(
                    "agent.model.answer for turn {number} claims {} tool calls; at most {} are written per turn",
                    calls.len(),
                    crate::tools::MAX_TOOL_CALLS_PER_TURN
                ));
                continue;
            }
            let (usage, duration_ms) = accounting.get(&number).copied().unwrap_or((None, None));
            let turn = match u32::try_from(number) {
                Ok(turn) => turn,
                Err(error) => {
                    problems.push(error.to_string());
                    continue;
                }
            };
            turns.push(RecordedTurn {
                turn,
                content: (!answer.is_empty()).then_some(answer),
                tool_calls: calls
                    .into_iter()
                    .map(|call| RecordedToolCall {
                        id: call.id,
                        name: call.function.name,
                        arguments: call.function.arguments,
                        result: None,
                    })
                    .collect(),
                usage,
                duration_ms,
            });
        }
        if !problems.is_empty() {
            return Err(malformed(problems.detail()));
        }
        problems.extend(context::capture_results(
            &contexts,
            &mut turns,
            &coordinates,
            job.as_deref(),
        ));
        if !problems.is_empty() {
            return Err(malformed(problems.detail()));
        }
        let answer = turns
            .last()
            .filter(|turn| turn.tool_calls.is_empty())
            .and_then(|turn| turn.content.clone())
            .filter(|content| !content.trim().is_empty());

        Ok(Self {
            version: RECORDING_VERSION,
            trace_id: trace_id.to_owned(),
            system,
            history,
            contexts,
            prompt,
            turns,
            // No accounting row is unknown, not zero calls: the `calls` list means "this is every
            // call", and an empty one would claim the session made none.
            calls: (!calls.is_empty()).then(|| calls.into_values().collect()),
            answer,
        })
    }

    /// Checks the declared version, portable context revisions, and independent call coordinates.
    ///
    /// # Errors
    /// Returns [`RecordingError::UnsupportedVersion`] for a shape this build does not read, and
    /// [`RecordingError::Malformed`] for conflicting or incomplete portable context — every
    /// conflict found, in one message.
    pub fn validate(&self) -> Result<(), RecordingError> {
        if self.version != RECORDING_VERSION {
            return Err(RecordingError::UnsupportedVersion {
                trace_id: self.trace_id.clone(),
                version: self.version,
                supported: RECORDING_VERSION,
            });
        }
        let malformed = |detail| RecordingError::Malformed {
            trace_id: self.trace_id.clone(),
            detail,
        };
        let mut problems = Problems::default();
        problems.extend(context::ReplayContext::new(self).map(|_| ()));
        let mut calls = std::collections::BTreeSet::new();
        for call in self.calls.iter().flatten() {
            if !calls.insert((&call.job, call.sequence)) {
                problems.push(format!(
                    "duplicate accounting call {}/{} in recording file",
                    call.job, call.sequence
                ));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(malformed(problems.detail()))
        }
    }

    /// The scripts the recorded model wrote, in order.
    #[must_use]
    pub fn scripts(&self) -> Vec<String> {
        self.turns
            .iter()
            .flat_map(|turn| turn.tool_calls.iter().filter_map(RecordedToolCall::script))
            .collect()
    }

    /// Unknown-aware usage over independent accounting calls, with historical file fallback.
    #[must_use]
    pub fn usage(&self) -> RecordedUsage {
        let observations: Vec<_> = match &self.calls {
            Some(calls) => calls.iter().map(|call| call.usage).collect(),
            None => self
                .turns
                .iter()
                .map(|turn| turn.usage.unwrap_or_default())
                .collect(),
        };
        let mut observations = observations.into_iter();
        let Some(mut total) = observations.next() else {
            return RecordedUsage::default();
        };
        for usage in observations {
            total.add(&usage);
        }
        total
    }
}

/// One session as its accounting records describe it, for listing.
///
/// Accounting records exist for every session, payload telemetry or not, so a listing built from
/// them shows sessions a transcript was never exported for; `RecordedSession::from_records` is
/// what then says whether one can be replayed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListing {
    /// The trace the session ran under.
    pub trace_id: String,
    /// The `service.name` resource attribute the records carried, when they carried one.
    #[serde(default)]
    pub service: Option<String>,
    /// Microseconds since the Unix epoch of the earliest accounted turn.
    pub started_us: i64,
    /// Microseconds since the Unix epoch of the latest accounted turn.
    pub ended_us: i64,
    /// Highest model turn accounted.
    pub model_turns: u32,
    /// Total tokens summed over every turn that reported them.
    #[serde(default)]
    pub total_tokens: Option<u64>,
    /// Whether any turn was accounted as failed.
    pub failed: bool,
    /// Whether the last accounted turn carried an answer.
    pub answered: bool,
}

/// Groups `accounting.model.turn` records by trace, newest session first.
///
/// Records that are not accounting records, or carry no trace, are skipped rather than refused:
/// a listing is a survey of what the receiver holds, and one malformed row must not hide the rest.
#[must_use]
pub fn list_sessions(records: &[Value]) -> Vec<SessionListing> {
    let mut sessions: BTreeMap<String, SessionListing> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for record in records {
        if !matches!(
            text(record, "audit.event").as_deref(),
            Some("accounting.model.call" | "accounting.model.turn")
        ) {
            continue;
        }
        let Some(trace_id) = text(record, "trace_id") else {
            continue;
        };
        // Dedup only what is actually counted. A transcript row carrying the same job/call
        // coordinates used to consume the slot and then be discarded, hiding the accounting row.
        if let (Some(job), Some(call)) = (text(record, "job.id"), unsigned(record, "call.sequence"))
            && !seen.insert((job, call))
        {
            continue;
        }
        let timestamp = field(record, "_timestamp")
            .and_then(Value::as_i64)
            .or_else(|| text(record, "_timestamp").and_then(|value| value.trim().parse().ok()))
            .unwrap_or_default();
        let turn = unsigned(record, "model.turn").and_then(|turn| u32::try_from(turn).ok());
        let tokens = unsigned(record, "usage.total_tokens");
        let failed = matches!(
            text(record, "outcome").as_deref(),
            Some("failed" | "cancelled" | "abandoned")
        );
        let answered = matches!(field(record, "answer.present"), Some(Value::Bool(true)))
            || text(record, "answer.present").as_deref() == Some("true");
        let entry = sessions
            .entry(trace_id.clone())
            .or_insert_with(|| SessionListing {
                trace_id,
                service: text(record, "service_name"),
                started_us: timestamp,
                ended_us: timestamp,
                model_turns: 0,
                total_tokens: Some(0),
                failed: false,
                answered: false,
            });
        entry.started_us = entry.started_us.min(timestamp);
        if timestamp >= entry.ended_us {
            entry.ended_us = timestamp;
        }
        if let Some(turn) = turn
            && turn >= entry.model_turns
        {
            entry.model_turns = turn;
            entry.answered = answered;
        }
        entry.total_tokens = entry
            .total_tokens
            .zip(tokens)
            .and_then(|(a, b)| a.checked_add(b));
        entry.failed |= failed;
    }
    let mut listing = sessions.into_values().collect::<Vec<_>>();
    listing.sort_by(|left, right| {
        right
            .started_us
            .cmp(&left.started_us)
            .then_with(|| left.trace_id.cmp(&right.trace_id))
    });
    listing
}

/// What the replayed session runs under, beyond the model.
pub struct ReplayInputs<'a> {
    /// Host retains this finalizer until its output disposition is known.
    pub accounting: Option<&'a crate::accounting::JobAccounting>,
    /// Exact model name selected by the replay host, replacing any recorded model identity.
    pub selected_model: &'a str,
    /// Replacement standing instructions; `None` replays the recorded ones.
    pub system: Option<&'a str>,
    /// Skills to mount, replacing any listing the recording carried.
    pub skills: &'a [Skill],
    /// Whether to offer `suggest_improvement`.
    pub improvement_suggestions: bool,
    /// Where a script the recording never ran goes: run live, or stop the replay there.
    ///
    /// `Sync` because the replay runtime doubles as the session's cancellation probe and usage
    /// observer, both of which the prompt loop requires to be shareable.
    pub live: Option<&'a (dyn ScriptRuntime + Sync)>,
    /// Bounds on the replayed session.
    pub limits: PromptLimits,
}

/// How the replay handled a script the recording could not answer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DivergenceHandling {
    /// The replay stopped at the first unrecorded script.
    Stopped,
    /// The unrecorded script ran on the live runtime the caller supplied.
    Live,
}

/// The point at which the replayed model left the recorded trajectory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Divergence {
    /// The replayed model turn that wrote the script.
    pub turn: u32,
    /// The script the recording could not answer.
    pub script: String,
    /// Recorded scripts the replay had not consumed when it diverged.
    pub unused_recorded_scripts: Vec<String>,
    /// What happened next.
    pub handling: DivergenceHandling,
}

/// One session, recorded or replayed, at the level the two are compared.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    /// Model turns the session made, or `None` when the recording does not say.
    #[serde(default)]
    pub model_turns: Option<u32>,
    /// Scripts the model wrote, in order.
    pub scripts: Vec<String>,
    /// The final answer, when one was produced.
    #[serde(default)]
    pub answer: Option<String>,
    /// Token usage summed over the session.
    pub usage: RecordedUsage,
}

/// What a replay produced, beside what was recorded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayReport {
    /// The trace the recording came from.
    pub trace_id: String,
    /// The session as recorded.
    pub recorded: SessionSummary,
    /// The session as replayed.
    pub replayed: SessionSummary,
    /// Where the replay left the recording, if it did.
    #[serde(default)]
    pub divergence: Option<Divergence>,
    /// Suggestions the replayed model recorded.
    #[serde(default)]
    pub suggestions: Vec<ImprovementSuggestion>,
    /// Recorded history exchanges the replay could not carry, because retention is bounded.
    ///
    /// A recording whose route remembered more turns than [`History`] retains replays the newest
    /// ones; this says how many older ones the replayed model never saw, so a comparison against
    /// the recorded session is not read as like-for-like when it is not.
    #[serde(default)]
    pub dropped_history_turns: usize,
    /// A session failure that ended the replay, other than a divergence stop.
    #[serde(default)]
    pub error: Option<String>,
}

/// Answers scripts from the recording, and knows when it cannot.
struct ReplayRuntime<'a> {
    recorded: Mutex<VecDeque<(String, ScriptOutcome)>>,
    requested: Mutex<Vec<String>>,
    divergence: Mutex<Option<Divergence>>,
    /// Assistant batches in which at least one script was dispatched live, by call sequence.
    ///
    /// `ToolGroup::provenance` labels the whole batch, and the label a later turn shows the model
    /// says "no new capability execution is claimed". That is only true of a batch every one of
    /// whose results came out of the recording, so a batch is disqualified the moment one script
    /// reaches the live runtime — before or after a recorded sibling in the same batch.
    live_dispatched_groups: Mutex<std::collections::BTreeSet<u32>>,
    live: Option<&'a (dyn ScriptRuntime + Sync)>,
    recorded_system: &'a [String],
}

impl ReplayRuntime<'_> {
    fn recorded_outcomes(recorded: &RecordedSession) -> VecDeque<(String, ScriptOutcome)> {
        recorded
            .turns
            .iter()
            .flat_map(|turn| turn.tool_calls.iter())
            .filter_map(|call| {
                let script = call.script()?;
                let result = call.result.as_deref()?;
                Some((script, outcome_from_result(result)))
            })
            .collect()
    }
}

/// Rebuilds a script outcome from the tool result the loop rendered.
///
/// The trailer is the loop's own `[exit code: N]`, so parsing it is reading back what this crate
/// wrote. Capability counts and steps were not recorded; a replayed script spends none of either.
fn outcome_from_result(result: &str) -> ScriptOutcome {
    let (output, exit_code) = match result.rsplit_once("[exit code: ") {
        Some((head, tail)) => {
            let code = tail
                .trim_end_matches(']')
                .trim()
                .parse::<u8>()
                .map_or(ExitCode::FAILURE, ExitCode::from);
            (head.strip_suffix('\n').unwrap_or(head).to_owned(), code)
        }
        None => (result.to_owned(), ExitCode::SUCCESS),
    };
    ScriptOutcome {
        output,
        exit_code,
        truncated: false,
        capability_calls: 0,
        steps: 0,
    }
}

impl ReplayRuntime<'_> {
    /// Marks the current assistant batch as one a live dispatch entered, and takes back any
    /// `RecordedReplay` label an earlier recorded sibling in the same batch already stamped.
    fn disqualify_group(&self, journal: &crate::checkpoint::ExecutionJournal) {
        let Some(call) = journal
            .snapshot()
            .record
            .groups
            .last()
            .map(|group| group.call)
        else {
            return;
        };
        self.live_dispatched_groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call);
        if let Err(error) = journal.update(|c| {
            if let Some(group) = c.record.groups.last_mut()
                && group.provenance == Some(crate::history::ExecutionProvenance::RecordedReplay)
            {
                group.provenance = None;
            }
        }) {
            journal.failure(error);
        }
    }

    fn run_script_inner(
        &self,
        script: &str,
        max_capability_calls: u32,
        journal: Option<&crate::checkpoint::ExecutionJournal>,
    ) -> ScriptOutcome {
        self.requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(script.to_owned());
        let mut recorded = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The first unconsumed recording of this exact script, wherever it sits: a replayed model
        // may reorder two independent scripts and still be on the recorded trajectory.
        if let Some(position) = recorded.iter().position(|(recorded, _)| recorded == script) {
            let (_, outcome) = recorded.remove(position).expect("position is in range");
            if let Some(journal) = journal {
                let live_dispatched = self
                    .live_dispatched_groups
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Err(error) = journal.update(|c| {
                    if let Some(group) = c.record.groups.last_mut()
                        && !live_dispatched.contains(&group.call)
                    {
                        group.provenance =
                            Some(crate::history::ExecutionProvenance::RecordedReplay);
                    }
                }) {
                    drop(live_dispatched);
                    journal.failure(error);
                }
            }
            return outcome;
        }
        let unused = recorded
            .iter()
            .map(|(script, _)| script.clone())
            .collect::<Vec<_>>();
        drop(recorded);
        let turn = journal.map_or(0, |journal| journal.snapshot().state.spent.model_calls);
        let mut divergence = self
            .divergence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.live {
            Some(live) => {
                divergence.get_or_insert_with(|| Divergence {
                    turn,
                    script: script.to_owned(),
                    unused_recorded_scripts: unused,
                    handling: DivergenceHandling::Live,
                });
                drop(divergence);
                match journal {
                    Some(journal) => {
                        self.disqualify_group(journal);
                        live.run_script_observed(script, max_capability_calls, journal)
                    }
                    None => live.run_script(script, max_capability_calls),
                }
            }
            None => {
                *divergence = Some(Divergence {
                    turn,
                    script: script.to_owned(),
                    unused_recorded_scripts: unused,
                    handling: DivergenceHandling::Stopped,
                });
                ScriptOutcome {
                    output: "[replay stopped: the recorded session never ran this script and no \
                             live providers were supplied to run it]"
                        .to_owned(),
                    exit_code: ExitCode::SYNTAX,
                    truncated: false,
                    capability_calls: 0,
                    steps: 0,
                }
            }
        }
    }
}
impl ScriptRuntime for ReplayRuntime<'_> {
    fn observes_executions(&self) -> bool {
        self.live.is_some_and(ScriptRuntime::observes_executions)
    }
    fn run_script(&self, script: &str, maximum: u32) -> ScriptOutcome {
        self.run_script_inner(script, maximum, None)
    }
    fn run_script_observed(
        &self,
        script: &str,
        maximum: u32,
        journal: &crate::checkpoint::ExecutionJournal,
    ) -> ScriptOutcome {
        self.run_script_inner(script, maximum, Some(journal))
    }
    // The tool description is part of the prompt the replayed model is shown, so it has to name
    // the words the live runtime would run; with no live runtime there are none to offer.
    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
        match self.live {
            Some(live) => live.capability_snapshot(),
            None => CapabilitySnapshot::from_recording(self.recorded_system),
        }
    }
}

impl CancellationProbe for ReplayRuntime<'_> {
    fn is_cancelled(&self) -> bool {
        self.divergence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|divergence| divergence.handling == DivergenceHandling::Stopped)
    }
}

/// Puts the recorded prompt to `model` again under `inputs`, answering scripts from the recording.
///
/// The recorded system messages are replayed unless `inputs.system` replaces them; a recorded
/// skills listing is dropped whenever `inputs.skills` mounts anything, so the replay lists exactly
/// the skills it can actually read. No capability runs unless a live runtime is supplied and the
/// model diverges onto it.
pub fn replay<M>(model: &M, recorded: &RecordedSession, inputs: ReplayInputs<'_>) -> ReplayReport
where
    M: ChatModel + ?Sized,
{
    let runtime = ReplayRuntime {
        recorded: Mutex::new(ReplayRuntime::recorded_outcomes(recorded)),
        requested: Mutex::new(Vec::new()),
        divergence: Mutex::new(None),
        live_dispatched_groups: Mutex::new(std::collections::BTreeSet::new()),
        live: inputs.live,
        recorded_system: &recorded.system,
    };
    let system = match inputs.system {
        Some(system) => Some(system.to_owned()),
        None => {
            let kept = recorded
                .system
                .iter()
                .filter(|message| !bootstrap::is_prompt_block(message))
                .filter(|message| inputs.skills.is_empty() || !skills::is_prompt_block(message))
                .cloned()
                .collect::<Vec<_>>();
            (!kept.is_empty()).then(|| kept.join("\n\n"))
        }
    };
    let mut history = History::new(HistoryLimits {
        max_turns: recorded.history.len().max(1),
        max_bytes: usize::MAX,
    });
    for exchange in &recorded.history {
        history.record(match &exchange.answer {
            Some(answer) => crate::history::JobRecord::completed(&exchange.user, answer),
            None => crate::history::JobRecord::unanswered(&exchange.user),
        });
    }
    // `History::new` clamps the retention window it is asked for, and `record` trims to it, so a
    // recording with more remembered exchanges than the harness retains loses the oldest ones. The
    // count is measured rather than derived, so it stays right whatever the clamp is.
    let dropped_history_turns = recorded.history.len().saturating_sub(history.len());
    let fallback_accounting = crate::accounting::JobAccounting::default();
    let accounting = inputs.accounting.unwrap_or(&fallback_accounting);
    let mut session = SessionBootstrap::new(&recorded.prompt, inputs.limits, inputs.selected_model)
        .with_system(system.as_deref())
        .with_skills(inputs.skills)
        .with_accounting(accounting)
        .with_cancellation(&runtime);
    if inputs.improvement_suggestions {
        session = session.with_improvement_suggestions();
    }
    let portable = recorded
        .validate()
        .map_err(|error| error.to_string())
        .and_then(|()| context::ReplayContext::new(recorded));
    let outcome = match portable {
        Ok(Some(ref policy)) => SessionEngine::new(model, &runtime)
            .run(session.with_context_policy(policy), &mut history),
        Ok(None) => SessionEngine::new(model, &runtime).run(session, &mut history),
        Err(error) => {
            return ReplayReport {
                trace_id: recorded.trace_id.clone(),
                recorded: SessionSummary {
                    model_turns: recorded.model_turns(),
                    scripts: recorded.scripts(),
                    answer: recorded.answer.clone(),
                    usage: recorded.usage(),
                },
                replayed: SessionSummary::default(),
                divergence: None,
                suggestions: Vec::new(),
                dropped_history_turns,
                error: Some(error),
            };
        }
    };
    let tracked = accounting.snapshot();
    let divergence = runtime
        .divergence
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let (answer, suggestions, error) = match outcome {
        Ok(outcome) => (
            (!outcome.answer.is_empty()).then_some(outcome.answer),
            outcome.suggestions,
            None,
        ),
        Err(PromptError::Cancelled)
            if divergence
                .as_ref()
                .is_some_and(|divergence| divergence.handling == DivergenceHandling::Stopped) =>
        {
            (None, Vec::new(), None)
        }
        Err(error) => (None, Vec::new(), Some(error.to_string())),
    };
    ReplayReport {
        trace_id: recorded.trace_id.clone(),
        recorded: SessionSummary {
            model_turns: recorded.model_turns(),
            scripts: recorded.scripts(),
            answer: recorded.answer.clone(),
            usage: recorded.usage(),
        },
        replayed: SessionSummary {
            model_turns: Some(
                u32::try_from(
                    tracked
                        .calls
                        .iter()
                        .filter(|c| c.kind == crate::accounting::CallKind::Chat)
                        .count(),
                )
                .unwrap_or(u32::MAX),
            ),
            scripts: runtime
                .requested
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            answer,
            usage: RecordedUsage::from(tracked.totals().cumulative.usage()),
        },
        divergence,
        suggestions,
        dropped_history_turns,
        error,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use dekopon_model::model::{
        AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
        ModelToolCall,
    };
    use serde_json::{Value, json};

    use super::{
        DivergenceHandling, RECORDING_VERSION, RecordedSession, RecordingError, ReplayInputs,
        SessionListing, list_sessions, outcome_from_result, replay,
    };
    use crate::{
        bootstrap::{BootstrapError, CapabilitySnapshot},
        checkpoint::ExecutionJournal,
        history::{ExecutionProvenance, JobRecord},
        runtime::ScriptRuntime,
        session::PromptLimits,
        tools::{MAX_TOOL_CALLS_PER_TURN, SCRIPT_TOOL_NAME},
    };
    use dekopon_shell::{ExitCode, ScriptOutcome};

    #[test]
    fn independent_accounting_includes_failed_calls_images_and_unknown_fields() {
        let prompt = json!({"trace_id":"usage", "audit_event":"agent.model.prompt", "model_turn":1,"transcript_scope":"full", "messages":json!([{"role":"user","content":"request"}]).to_string()});
        let failed = json!({"trace_id":"usage", "audit_event":"accounting.model.call", "job_id":"job", "call_sequence":1, "model_turn":1,"model_kind":"chat", "usage_input_tokens":17, "outcome":"failed"});
        let records = vec![prompt.clone(), failed.clone(), failed.clone()];
        let recorded = RecordedSession::from_records("usage", &records).unwrap();
        assert!(recorded.turns.is_empty());
        assert_eq!(recorded.calls.as_ref().unwrap().len(), 1);
        assert_eq!(recorded.usage().input_tokens, Some(17));
        assert_eq!(recorded.usage().output_tokens, None);
        assert_eq!(recorded.usage().total_tokens, None);
        let image = json!({"trace_id":"usage", "audit_event":"accounting.model.call", "job_id":"job", "call_sequence":2, "model_turn":1,"model_kind":"image", "usage_input_tokens":5,"usage_output_tokens":6});
        let recorded = RecordedSession::from_records("usage", &[prompt, failed, image]).unwrap();
        assert_eq!(recorded.calls.as_ref().unwrap().len(), 2);
        assert_eq!(recorded.usage().input_tokens, Some(22));
        assert_eq!(recorded.usage().output_tokens, None);
        assert_eq!(
            RecordedSession::default().usage(),
            super::RecordedUsage::default()
        );
    }

    #[test]
    fn replay_summary_counts_independent_chat_calls_including_failed_inference() {
        let mut rows = records();
        rows.retain(|row| row["audit_event"] != "accounting.model.turn");
        rows.retain(|row| !(row["audit_event"] == "agent.model.answer" && row["model_turn"] == 2));
        for (sequence, kind, total) in [(1, "chat", 12), (2, "chat", 7), (3, "image", 20)] {
            let row = json!({"trace_id":"t1", "audit_event":"accounting.model.call", "job_id":"job", "call_sequence":sequence, "model_turn":sequence.min(2), "model_kind":kind, "usage_total_tokens":total});
            rows.extend([row.clone(), row]);
        }
        let recorded = RecordedSession::from_records("t1", &rows).unwrap();
        assert_eq!(recorded.turns.len(), 1);
        assert_eq!(recorded.calls.as_ref().unwrap().len(), 3);
        let report = replay(
            &ScriptedModel::new([answer("done")]),
            &recorded,
            inputs(None),
        );
        assert_eq!(report.error, None);
        assert_eq!(report.recorded.model_turns, Some(2));
        assert_eq!(report.recorded.usage.total_tokens, Some(39));
        let mut malformed = recorded.clone();
        malformed.contexts[0].scope = "invalid".into();
        let report = replay(&ScriptedModel::new([]), &malformed, inputs(None));
        assert!(report.error.unwrap().contains("revision ordering"));
        assert_eq!(report.recorded.model_turns, Some(2));
        assert_eq!(report.recorded.usage.total_tokens, Some(39));
        let mut historical = recorded;
        historical.calls = None;
        assert_eq!(historical.model_turns(), Some(1));
    }

    /// A listing is built from accounting alone, so it covers sessions with no transcript.
    #[test]
    fn sessions_are_listed_from_accounting_records_newest_first() {
        let records = vec![
            json!({"trace_id": "old", "audit_event": "accounting.model.turn", "model_turn": 1, "_timestamp": 1_000, "usage_total_tokens": 10, "answer_present": false, "outcome": "succeeded", "service_name": "dekopond"}),
            json!({"trace_id": "new", "audit_event": "accounting.model.turn", "model_turn": 1, "_timestamp": 5_000, "usage_total_tokens": "7", "answer_present": "false", "outcome": "succeeded"}),
            json!({"trace_id": "old", "audit_event": "accounting.model.turn", "model_turn": 2, "_timestamp": 2_000, "usage_total_tokens": 15, "answer_present": true, "outcome": "succeeded", "service_name": "dekopond"}),
            json!({"trace_id": "new", "audit_event": "accounting.model.turn", "model_turn": 2, "_timestamp": 6_000, "outcome": "failed"}),
            json!({"trace_id": "new", "audit_event": "agent.model.answer", "model_turn": 1, "_timestamp": 5_500}),
            json!({"audit_event": "accounting.model.turn", "model_turn": 1, "_timestamp": 9_000}),
        ];

        let listing = list_sessions(&records);

        assert_eq!(
            listing,
            vec![
                SessionListing {
                    trace_id: "new".to_owned(),
                    service: None,
                    started_us: 5_000,
                    ended_us: 6_000,
                    model_turns: 2,
                    total_tokens: None,
                    failed: true,
                    answered: false,
                },
                SessionListing {
                    trace_id: "old".to_owned(),
                    service: Some("dekopond".to_owned()),
                    started_us: 1_000,
                    ended_us: 2_000,
                    model_turns: 2,
                    total_tokens: Some(25),
                    failed: false,
                    answered: true,
                },
            ]
        );
    }

    /// The transcript one two-turn session exports, as flattened OpenObserve rows would carry it.
    fn records() -> Vec<Value> {
        let first_prompt = json!([
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "earlier question"},
            {"role": "assistant", "content": "earlier answer"},
            {"role": "user", "content": "How many posts?"}
        ]);
        let script = "posts.count | jq .n";
        let call = json!([{"id": "call-1", "type": "function", "function": {"name": "bash", "arguments": json!({"script": script}).to_string()}}]);
        let delta = json!([
            {"role": "assistant", "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "bash", "arguments": json!({"script": script}).to_string()}}]},
            {"role": "tool", "content": "42\n[exit code: 0]", "tool_call_id": "call-1"}
        ]);
        vec![
            json!({"trace_id": "t1", "audit_event": "agent.model.prompt", "model_turn": 1, "transcript_scope": "full", "messages": first_prompt.to_string()}),
            json!({"trace_id": "t1", "audit_event": "accounting.model.turn", "model_turn": 1, "duration_ms": 12.5, "usage_input_tokens": 100, "usage_output_tokens": 10}),
            json!({"trace_id": "t1", "audit_event": "agent.model.answer", "model_turn": 1, "answer": "", "tool_calls": call.to_string()}),
            json!({"trace_id": "t1", "audit_event": "agent.tool.script", "model_turn": 1, "tool_call_index": 1, "script": script}),
            json!({"trace_id": "t1", "audit_event": "agent.tool.output", "model_turn": 1, "tool_call_index": 1, "output": "42\n[exit code: 0]"}),
            // Out of order on purpose: a backend returns rows sorted its own way.
            json!({"trace_id": "t1", "audit_event": "agent.model.answer", "model_turn": 2, "answer": "There are 42 posts.", "tool_calls": "[]"}),
            json!({"trace_id": "t1", "audit_event": "agent.model.prompt", "model_turn": 2, "transcript_scope": "delta", "messages": delta.to_string()}),
            json!({"trace_id": "t1", "audit_event": "accounting.model.turn", "model_turn": 2, "duration_ms": "7", "usage_input_tokens": "150", "usage_cached_input_tokens": 90, "usage_output_tokens": 20}),
            json!({"trace_id": "other", "audit_event": "agent.model.prompt", "model_turn": 1, "transcript_scope": "full", "messages": "[]"}),
        ]
    }

    #[test]
    fn a_transcript_is_reconstructed_from_full_and_delta_prompt_events() {
        let session = RecordedSession::from_records("t1", &records()).expect("transcript loads");

        assert_eq!(session.system, vec!["Be brief.".to_owned()]);
        assert_eq!(session.history.len(), 1);
        assert_eq!(session.history[0].user, "earlier question");
        assert_eq!(session.history[0].answer.as_deref(), Some("earlier answer"));
        assert_eq!(session.prompt, "How many posts?");
        assert_eq!(session.turns.len(), 2);
        assert_eq!(session.turns[0].tool_calls.len(), 1);
        assert_eq!(
            session.turns[0].tool_calls[0].script().as_deref(),
            Some("posts.count | jq .n")
        );
        assert_eq!(
            session.turns[0].tool_calls[0].result.as_deref(),
            Some("42\n[exit code: 0]")
        );
        assert_eq!(session.turns[0].duration_ms, Some(12.5));
        assert_eq!(
            session.turns[1]
                .usage
                .and_then(|usage| usage.cached_input_tokens),
            Some(90)
        );
        assert_eq!(session.answer.as_deref(), Some("There are 42 posts."));
        assert_eq!(session.usage().input_tokens, Some(250));
        assert_eq!(session.scripts(), vec!["posts.count | jq .n".to_owned()]);

        // The on-disk shape round-trips, which is what `--from-file` depends on.
        let encoded = serde_json::to_string(&session).expect("serializes");
        let decoded: RecordedSession = serde_json::from_str(&encoded).expect("deserializes");
        assert_eq!(decoded, session);
    }

    #[test]
    fn a_metadata_only_recording_says_why_it_cannot_be_replayed() {
        let records = vec![
            json!({"trace_id": "t2", "audit_event": "accounting.model.turn", "model_turn": 1}),
            json!({"trace_id": "t2", "audit_event": "accounting.model.turn", "model_turn": 2}),
        ];
        let error = RecordedSession::from_records("t2", &records).expect_err("no transcript");
        assert!(
            matches!(&error, RecordingError::NoTranscript { turns: 2, .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("payload telemetry off"),
            "{error}"
        );

        let error = RecordedSession::from_records("absent", &records).expect_err("no records");
        assert!(matches!(error, RecordingError::NoRecords { .. }), "{error}");
    }

    #[test]
    fn a_tool_result_trailer_rebuilds_the_outcome() {
        let outcome = outcome_from_result("hi\n[exit code: 126]");
        assert_eq!(outcome.output, "hi");
        assert_eq!(outcome.exit_code.get(), 126);
        let empty = outcome_from_result("[exit code: 0]");
        assert_eq!(empty.output, "");
        assert_eq!(empty.exit_code.get(), 0);
    }

    /// A model whose turns are fixed, recording the system prompt it saw.
    struct ScriptedModel {
        turns: Mutex<VecDeque<AssistantTurn>>,
        systems: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedModel {
        fn new(turns: impl IntoIterator<Item = AssistantTurn>) -> Self {
            Self {
                turns: Mutex::new(turns.into_iter().collect()),
                systems: Mutex::new(Vec::new()),
            }
        }
    }

    impl ChatModel for ScriptedModel {
        fn complete(
            &self,
            messages: &[ModelMessage],
            _tools: &[ModelTool],
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let result: Result<AssistantTurn, ModelError> = {
                self.systems.lock().expect("systems").push(
                    messages
                        .iter()
                        .filter(|message| message.role() == "system")
                        .filter_map(|message| message.content().map(str::to_owned))
                        .collect(),
                );
                self.turns
                    .lock()
                    .expect("turns")
                    .pop_front()
                    .ok_or(ModelError::NoChoices)
            };
            if let Ok(turn) = &result
                && let Some(usage) = turn.usage
            {
                recorder.observe(
                    attempt,
                    dekopon_model::usage::UsageObservation {
                        usage,
                        invalid: [false; 5],
                    },
                )?;
            }
            result
        }
    }

    fn script_call(script: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "replay-call".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({"script": script}).to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        }
    }

    fn answer(text: &str) -> AssistantTurn {
        AssistantTurn {
            content: Some(text.to_owned()),
            tool_calls: Vec::new(),
            usage: None,
            replay_items: Vec::new(),
        }
    }

    fn inputs<'a>(system: Option<&'a str>) -> ReplayInputs<'a> {
        ReplayInputs {
            accounting: None,
            selected_model: "replay-model",
            system,
            skills: &[],
            improvement_suggestions: false,
            live: None,
            limits: PromptLimits {
                max_steps: 4,
                max_capability_calls: 8,
            },
        }
    }

    #[test]
    fn a_replay_on_the_recorded_trajectory_is_answered_from_the_recording() {
        let recorded = RecordedSession::from_records("t1", &records()).expect("transcript loads");
        let model = ScriptedModel::new([
            script_call("posts.count | jq .n"),
            answer("Forty-two posts."),
        ]);

        let report = replay(&model, &recorded, inputs(Some("Be terse.")));

        assert!(report.divergence.is_none(), "{report:?}");
        assert_eq!(report.error, None);
        assert_eq!(report.replayed.answer.as_deref(), Some("Forty-two posts."));
        assert_eq!(report.replayed.model_turns, Some(2));
        assert_eq!(
            report.replayed.scripts,
            vec!["posts.count | jq .n".to_owned()]
        );
        assert_eq!(
            report.recorded.answer.as_deref(),
            Some("There are 42 posts.")
        );
        // The override replaced the recorded instructions, and the history was replayed.
        let systems = model.systems.lock().expect("systems");
        assert_eq!(systems[0].len(), 2);
        assert_eq!(systems[0][0], "Be terse.");
        assert!(crate::bootstrap::is_prompt_block(&systems[0][1]));
        assert!(systems[0][1].contains("replay-model"));
    }

    /// A live runtime for the scripts the recording cannot answer, which reports what the
    /// checkpoint said about each assistant batch at the moment it was asked to run one.
    struct LiveScripts {
        records: Mutex<Vec<JobRecord>>,
    }

    impl LiveScripts {
        fn new() -> Self {
            Self {
                records: Mutex::new(Vec::new()),
            }
        }
        fn record(&self, index: usize) -> JobRecord {
            self.records.lock().expect("observed records")[index].clone()
        }
    }

    impl ScriptRuntime for LiveScripts {
        fn run_script(&self, script: &str, _maximum: u32) -> ScriptOutcome {
            ScriptOutcome {
                output: format!("live {script}"),
                exit_code: ExitCode::SUCCESS,
                truncated: false,
                capability_calls: 0,
                steps: 0,
            }
        }
        fn run_script_observed(
            &self,
            script: &str,
            maximum: u32,
            journal: &ExecutionJournal,
        ) -> ScriptOutcome {
            self.records
                .lock()
                .expect("observed records")
                .push(journal.snapshot().record);
            self.run_script(script, maximum)
        }
        fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
            Ok(CapabilitySnapshot::empty())
        }
    }

    fn live_inputs(live: &LiveScripts) -> ReplayInputs<'_> {
        ReplayInputs {
            live: Some(live),
            limits: PromptLimits {
                max_steps: 6,
                max_capability_calls: 8,
            },
            ..inputs(None)
        }
    }

    fn banner_shown(record: &JobRecord) -> bool {
        let mut messages = Vec::new();
        crate::context::replay_job(record, &mut messages);
        messages
            .iter()
            .filter_map(ModelMessage::content)
            .any(|text| text.contains("no new capability execution is claimed"))
    }

    /// The banner speaks for the whole assistant batch, so one live dispatch in it makes the
    /// claim false, whichever order the recorded and unrecorded scripts ran in.
    #[test]
    fn a_batch_with_one_live_script_claims_no_recorded_replay_for_the_group() {
        let recorded = RecordedSession::from_records("t1", &records()).expect("transcript loads");
        let mixed_first = AssistantTurn {
            content: None,
            tool_calls: ["posts.count | jq .n", "posts.list | jq length"]
                .iter()
                .enumerate()
                .map(|(index, script)| {
                    let mut call = script_call(script).tool_calls.remove(0);
                    call.id = format!("replay-call-{index}");
                    call
                })
                .collect(),
            usage: None,
            replay_items: Vec::new(),
        };
        let live = LiveScripts::new();
        let report = replay(
            &ScriptedModel::new([
                mixed_first,
                script_call("posts.tail | jq .n"),
                answer("mixed"),
            ]),
            &recorded,
            live_inputs(&live),
        );
        assert_eq!(report.error, None, "{report:?}");
        // Observed while the second batch's live script ran, so the first batch is settled.
        let settled = live.record(1);
        assert_eq!(
            settled.groups[0].provenance, None,
            "a batch a live script entered is not a recorded replay"
        );
        assert!(!banner_shown(&settled), "{settled:?}");

        // The control: a batch every one of whose results came from the recording keeps the label.
        let live = LiveScripts::new();
        let report = replay(
            &ScriptedModel::new([
                script_call("posts.count | jq .n"),
                script_call("posts.tail | jq .n"),
                answer("all recorded first"),
            ]),
            &recorded,
            live_inputs(&live),
        );
        assert_eq!(report.error, None, "{report:?}");
        let settled = live.record(0);
        assert_eq!(
            settled.groups[0].provenance,
            Some(ExecutionProvenance::RecordedReplay)
        );
        assert!(banner_shown(&settled), "{settled:?}");
    }

    /// The writer caps a turn at ten calls, so the reconstruction refuses more before iterating.
    #[test]
    fn an_answer_row_claiming_more_tool_calls_than_a_turn_can_hold_is_refused() {
        let calls = (0..=MAX_TOOL_CALLS_PER_TURN)
            .map(|index| {
                json!({"id": format!("call-{index}"), "type": "function",
                       "function": {"name": "bash", "arguments": json!({"script": "x"}).to_string()}})
            })
            .collect::<Vec<_>>();
        let mut rows = records();
        rows.retain(|row| row["audit_event"] != "agent.model.answer" || row["model_turn"] != 1);
        rows.push(
            json!({"trace_id": "t1", "audit_event": "agent.model.answer", "model_turn": 1,
                   "answer": "", "tool_calls": json!(calls).to_string()}),
        );
        let error = RecordedSession::from_records("t1", &rows)
            .expect_err("eleven tool calls are not a transcript this loop wrote")
            .to_string();
        assert!(error.contains("claims 11 tool calls"), "{error}");
        assert!(
            error.contains(&format!(
                "at most {MAX_TOOL_CALLS_PER_TURN} are written per turn"
            )),
            "{error}"
        );
        assert!(error.contains("turn 1"), "{error}");
    }

    /// No accounting row is unknown, not zero calls.
    #[test]
    fn a_recording_with_no_accounting_rows_reports_unknown_model_turns_not_zero() {
        let mut rows = records();
        rows.retain(|row| {
            !row["audit_event"]
                .as_str()
                .unwrap_or_default()
                .starts_with("accounting")
        });
        let recorded = RecordedSession::from_records("t1", &rows).expect("transcript loads");
        assert_eq!(
            recorded.calls, None,
            "no rows is not an empty list of calls"
        );
        // Answered turns are still an honest count when nothing else says otherwise.
        assert_eq!(recorded.model_turns(), Some(2));

        let mut emptied = recorded.clone();
        emptied.calls = Some(Vec::new());
        assert_eq!(
            emptied.model_turns(),
            None,
            "an empty call list is unknown, never zero"
        );

        // A list that names only image calls is the same kind of unknown: a truncated page set or
        // a hand-edited recording can produce one, and "0 turn(s)" there reads as free inference.
        let mut image_only = recorded.clone();
        image_only.calls = Some(vec![super::RecordedAccountingCall {
            job: "job".to_owned(),
            sequence: 1,
            kind: "image".to_owned(),
            usage: super::RecordedUsage::default(),
        }]);
        assert_eq!(
            image_only.model_turns(),
            None,
            "a call set naming no chat call says nothing about chat calls"
        );
        let report = replay(&ScriptedModel::new([]), &emptied, inputs(None));
        assert_eq!(report.recorded.model_turns, None);
        let encoded = serde_json::to_value(&report).expect("report serializes");
        assert_eq!(encoded["recorded"]["modelTurns"], Value::Null);
    }

    /// The dedup slot belongs to the rows the listing counts, not to the ones it discards.
    #[test]
    fn a_transcript_row_sharing_a_call_coordinate_does_not_hide_its_accounting_row() {
        let records = vec![
            json!({"trace_id": "t", "audit_event": "agent.model.answer", "job_id": "job",
                   "call_sequence": 1, "model_turn": 1, "_timestamp": 1_000}),
            json!({"trace_id": "t", "audit_event": "accounting.model.call", "job_id": "job",
                   "call_sequence": 1, "model_turn": 1, "_timestamp": 2_000,
                   "usage_total_tokens": 11, "answer_present": true, "outcome": "succeeded"}),
            // The same accounting row exported twice still counts once.
            json!({"trace_id": "t", "audit_event": "accounting.model.call", "job_id": "job",
                   "call_sequence": 1, "model_turn": 1, "_timestamp": 2_000,
                   "usage_total_tokens": 11, "answer_present": true, "outcome": "succeeded"}),
            json!({"trace_id": "t", "audit_event": "accounting.model.call", "job_id": "job",
                   "call_sequence": 2, "model_turn": 2, "_timestamp": 3_000,
                   "usage_total_tokens": 7, "answer_present": true, "outcome": "cancelled"}),
        ];

        let listing = list_sessions(&records);

        assert_eq!(
            listing,
            vec![SessionListing {
                trace_id: "t".to_owned(),
                service: None,
                started_us: 2_000,
                ended_us: 3_000,
                model_turns: 2,
                total_tokens: Some(18),
                failed: true,
                answered: true,
            }]
        );
    }

    /// A reconstruction reports every conflict it found, not the first one it hit.
    #[test]
    fn two_simultaneous_reconstruction_conflicts_are_both_reported() {
        let mut rows = records();
        let mut answer = rows[2].clone();
        answer["answer"] = json!("a different answer");
        let mut accounting = rows[1].clone();
        accounting["duration_ms"] = json!(99.5);
        rows.extend([answer, accounting]);

        let error = RecordedSession::from_records("t1", &rows)
            .expect_err("two conflicts")
            .to_string();

        assert!(
            error.contains("conflicting answer records for turn 1"),
            "{error}"
        );
        assert!(
            error.contains("conflicting accounting observations for turn 1"),
            "{error}"
        );
    }

    /// Two duplicate coordinates in an edited file are both named.
    #[test]
    fn every_duplicate_accounting_call_in_a_recording_file_is_named() {
        let mut recorded = RecordedSession::from_records("t1", &records()).expect("loads");
        let calls = recorded.calls.get_or_insert_with(Vec::new);
        let first = calls[0].clone();
        let mut second = first.clone();
        second.sequence = first.sequence.checked_add(1).expect("fits");
        calls.push(second.clone());
        calls.push(first.clone());
        calls.push(second);

        let error = recorded.validate().expect_err("two duplicates").to_string();

        assert!(
            error.contains(&format!("{}/{}", first.job, first.sequence)),
            "{error}"
        );
        assert!(
            error.contains(&format!("{}/{}", first.job, first.sequence + 1)),
            "{error}"
        );
    }

    /// The declared shape is checked before anything else, and an unknown key is not a recording.
    #[test]
    fn a_recording_file_declares_a_version_and_refuses_unknown_top_level_keys() {
        let recorded = RecordedSession::from_records("t1", &records()).expect("loads");
        assert_eq!(recorded.version, RECORDING_VERSION);
        let encoded = serde_json::to_value(&recorded).expect("serializes");
        assert_eq!(encoded["version"], json!(RECORDING_VERSION));

        let mut legacy = encoded.clone();
        legacy.as_object_mut().expect("object").remove("version");
        let decoded: RecordedSession =
            serde_json::from_value(legacy).expect("a file written before the field reads as 1");
        assert_eq!(decoded.version, RECORDING_VERSION);
        decoded.validate().expect("a version-1 recording validates");

        let mut unknown = encoded;
        unknown["surprise"] = json!(true);
        let error = serde_json::from_value::<RecordedSession>(unknown)
            .expect_err("an unknown top-level key is not this shape")
            .to_string();
        assert!(error.contains("surprise"), "{error}");

        let mut future = recorded;
        future.version = 99;
        let error = future.validate().expect_err("version 99 is not read");
        assert!(
            matches!(
                error,
                RecordingError::UnsupportedVersion { version: 99, .. }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("99"), "{error}");
    }

    /// A route that remembered more than the harness retains replays fewer turns, and says so.
    #[test]
    fn history_the_replay_could_not_carry_is_counted_in_the_report() {
        let mut recorded = RecordedSession::from_records("t1", &records()).expect("loads");
        recorded.contexts.clear();
        recorded.history = (0..200)
            .map(|index| super::RecordedExchange {
                user: format!("question {index}"),
                answer: Some(format!("answer {index}")),
            })
            .collect();

        let report = replay(
            &ScriptedModel::new([answer("done")]),
            &recorded,
            inputs(None),
        );

        assert_eq!(report.error, None, "{report:?}");
        assert_eq!(report.dropped_history_turns, 200 - 128);
        let encoded = serde_json::to_value(&report).expect("report serializes");
        assert_eq!(encoded["droppedHistoryTurns"], json!(72));

        let short = RecordedSession::from_records("t1", &records()).expect("loads");
        let report = replay(&ScriptedModel::new([answer("done")]), &short, inputs(None));
        assert_eq!(report.dropped_history_turns, 0);
    }

    #[test]
    fn a_script_the_recording_never_ran_stops_the_replay_and_is_reported() {
        let recorded = RecordedSession::from_records("t1", &records()).expect("transcript loads");
        let model = ScriptedModel::new([
            script_call("posts.list | jq length"),
            answer("never reached"),
        ]);

        let report = replay(&model, &recorded, inputs(None));

        let divergence = report.divergence.expect("the replay diverged");
        assert_eq!(divergence.turn, 1);
        assert_eq!(divergence.script, "posts.list | jq length");
        assert_eq!(divergence.handling, DivergenceHandling::Stopped);
        assert_eq!(
            divergence.unused_recorded_scripts,
            vec!["posts.count | jq .n".to_owned()]
        );
        assert_eq!(report.replayed.answer, None);
        assert_eq!(report.error, None, "a divergence stop is not a failure");
        assert_eq!(report.replayed.model_turns, Some(1));
        // Without an override the recorded instructions were replayed.
        let systems = model.systems.lock().expect("systems");
        assert_eq!(systems[0].len(), 2);
        assert_eq!(systems[0][0], "Be brief.");
        assert!(crate::bootstrap::is_prompt_block(&systems[0][1]));
        assert!(systems[0][1].contains("replay-model"));
    }
}

#[cfg(test)]
mod portable_tests;
