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

use crate::{
    improvement::ImprovementSuggestion,
    prompt::{
        CancellationProbe, History, HistoryLimits, ModelUsageObserver, PromptError, PromptLimits,
        SCRIPT_TOOL_NAME, ScriptRuntime, SessionInputs, run_prompt_session,
    },
    skills,
};

/// One session as its transcript recorded it.
///
/// This is also the on-disk shape `dekopon-run session show --json` prints and `session replay
/// --from-file` reads back, so a recording can be kept, edited, and replayed without a telemetry
/// backend in the loop.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedSession {
    /// The telemetry trace the records were read from.
    pub trace_id: String,
    /// Leading system messages, in order: standing instructions, then any skills listing.
    #[serde(default)]
    pub system: Vec<String>,
    /// Exchanges replayed ahead of the prompt on a persistent route, oldest first.
    #[serde(default)]
    pub history: Vec<RecordedExchange>,
    /// The message this session answered.
    pub prompt: String,
    /// Every model turn, in order.
    #[serde(default)]
    pub turns: Vec<RecordedTurn>,
    /// The final answer, when the session produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
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
            if let Some(value) = value {
                *total = Some(total.unwrap_or(0).saturating_add(value));
            }
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
        /// What was wrong, naming the event.
        detail: String,
    },
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

/// One message as the transcript's redacted rendering carries it.
#[derive(Debug, Deserialize)]
struct TranscriptMessage {
    role: String,
    #[serde(default)]
    content: Option<TranscriptContent>,
    #[serde(default)]
    tool_calls: Vec<TranscriptToolCall>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

/// Text, or the attachment summaries a multimodal message renders to.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TranscriptContent {
    Text(String),
    Parts(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct TranscriptToolCall {
    id: String,
    function: TranscriptFunction,
}

#[derive(Debug, Deserialize)]
struct TranscriptFunction {
    name: String,
    arguments: String,
}

impl RecordedSession {
    /// Reconstructs one session from the log records exported under its trace.
    ///
    /// Records may arrive in any order and may include events from other sessions; only the
    /// transcript and accounting events of `trace_id` are read. The message vector is rebuilt from
    /// the first turn's full prompt plus each later turn's delta, exactly as the loop emitted them,
    /// and the final turn's answer — which no later prompt carries — is taken from its own record.
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

        let mut prompts = BTreeMap::<u64, (String, String)>::new();
        let mut answers = BTreeMap::<u64, (String, String)>::new();
        let mut accounting = BTreeMap::<u64, (Option<RecordedUsage>, Option<f64>)>::new();
        for record in owned {
            let Some(event) = text(record, "audit.event") else {
                continue;
            };
            let turn = unsigned(record, "model.turn");
            match event.as_str() {
                "agent.model.prompt" => {
                    let turn = turn.ok_or_else(|| {
                        malformed("agent.model.prompt without model.turn".to_owned())
                    })?;
                    let scope = text(record, "transcript.scope").unwrap_or_default();
                    let messages = text(record, "messages").ok_or_else(|| {
                        malformed(format!(
                            "agent.model.prompt turn {turn} carries no messages"
                        ))
                    })?;
                    prompts.insert(turn, (scope, messages));
                }
                "agent.model.answer" => {
                    let turn = turn.ok_or_else(|| {
                        malformed("agent.model.answer without model.turn".to_owned())
                    })?;
                    answers.insert(
                        turn,
                        (
                            text(record, "answer").unwrap_or_default(),
                            text(record, "tool_calls").unwrap_or_else(|| "[]".to_owned()),
                        ),
                    );
                }
                "accounting.model.turn" => {
                    let Some(turn) = turn else { continue };
                    let usage = RecordedUsage {
                        input_tokens: unsigned(record, "usage.input_tokens"),
                        cached_input_tokens: unsigned(record, "usage.cached_input_tokens"),
                        output_tokens: unsigned(record, "usage.output_tokens"),
                        reasoning_output_tokens: unsigned(record, "usage.reasoning_output_tokens"),
                        total_tokens: unsigned(record, "usage.total_tokens"),
                    };
                    let usage = (usage != RecordedUsage::default()).then_some(usage);
                    accounting.insert(turn, (usage, float(record, "duration_ms")));
                }
                _ => {}
            }
        }

        if prompts.is_empty() {
            return Err(RecordingError::NoTranscript {
                trace_id: trace_id.to_owned(),
                turns: accounting.len(),
            });
        }
        let (&first_turn, (first_scope, _)) = prompts.iter().next().expect("checked non-empty");
        if first_scope != "full" {
            return Err(malformed(format!(
                "the earliest prompt record (turn {first_turn}) is a {first_scope:?} transcript rather than the full one"
            )));
        }

        // The message vector, exactly as the loop grew it.
        let mut messages = Vec::new();
        for (turn, (_, encoded)) in &prompts {
            let appended = serde_json::from_str::<Vec<TranscriptMessage>>(encoded)
                .map_err(|error| malformed(format!("turn {turn} messages: {error}")))?;
            messages.extend(appended);
        }
        let last_prompt_turn = *prompts.keys().next_back().expect("checked non-empty");
        for (turn, (answer, tool_calls)) in &answers {
            // Turn N's answer rides turn N+1's delta, so only the answer of the last requested
            // turn — the one no later request carried — has to come from its own record.
            if *turn < last_prompt_turn {
                continue;
            }
            let tool_calls = serde_json::from_str::<Vec<TranscriptToolCall>>(tool_calls)
                .map_err(|error| malformed(format!("turn {turn} tool calls: {error}")))?;
            messages.push(TranscriptMessage {
                role: "assistant".to_owned(),
                content: (!answer.is_empty()).then(|| TranscriptContent::Text(answer.clone())),
                tool_calls,
                tool_call_id: None,
            });
        }

        // Leading system messages.
        let mut index = 0;
        let mut system = Vec::new();
        while let Some(message) = messages
            .get(index)
            .filter(|message| message.role == "system")
        {
            system.push(
                message
                    .content
                    .as_ref()
                    .map_or(String::new(), |content| match content {
                        TranscriptContent::Text(text) => text.clone(),
                        TranscriptContent::Parts(parts) => parts.join("\n"),
                    }),
            );
            index += 1;
        }

        // The prompt is the user message just before turn 1's answer; what precedes it is history.
        // Turn 1 is recognized by its own answer record, because a replayed history pair looks
        // exactly like a plain-text turn.
        let first_answer_index = answers
            .get(&first_turn)
            .and_then(|(answer, tool_calls)| {
                let calls = serde_json::from_str::<Vec<TranscriptToolCall>>(tool_calls).ok()?;
                messages
                    .iter()
                    .enumerate()
                    .skip(index)
                    .position(|(_, message)| {
                        message.role == "assistant"
                        && message.content.as_ref().map_or(answer.is_empty(), |content| {
                            matches!(content, TranscriptContent::Text(text) if text == answer)
                        })
                        && message.tool_calls.len() == calls.len()
                        && message
                            .tool_calls
                            .iter()
                            .zip(&calls)
                            .all(|(left, right)| left.id == right.id)
                    })
            })
            .map(|position| position + index)
            .or_else(|| {
                // No answer record for turn 1: the session failed before a model answered, so
                // the prompt is simply the last user message.
                messages
                    .iter()
                    .rposition(|message| message.role == "user")
                    .map(|position| position + 1)
            })
            .ok_or_else(|| {
                malformed("no user message precedes the first model answer".to_owned())
            })?;
        if first_answer_index == 0
            || messages
                .get(first_answer_index - 1)
                .is_none_or(|message| message.role != "user")
        {
            return Err(malformed(
                "the message before the first model answer is not the user prompt".to_owned(),
            ));
        }
        let prompt_index = first_answer_index - 1;
        let mut history = Vec::new();
        let mut cursor = index;
        while cursor < prompt_index {
            let message = &messages[cursor];
            if message.role != "user" {
                return Err(malformed(format!(
                    "history message {cursor} has role {:?} where a user message belongs",
                    message.role
                )));
            }
            let user = content_text(&message.content);
            cursor += 1;
            let answer = messages
                .get(cursor)
                .filter(|next| cursor < prompt_index && next.role == "assistant")
                .map(|next| {
                    cursor += 1;
                    content_text(&next.content)
                });
            history.push(RecordedExchange { user, answer });
        }
        let prompt = content_text(&messages[prompt_index].content);

        // Every assistant message after the prompt is one turn; the tool messages after it are its
        // results, matched by call identifier.
        let mut turns = Vec::<RecordedTurn>::new();
        for message in messages.iter().skip(prompt_index + 1) {
            match message.role.as_str() {
                "assistant" => {
                    let number = u32::try_from(turns.len() + 1).unwrap_or(u32::MAX);
                    let (usage, duration_ms) = accounting
                        .get(&u64::from(number))
                        .copied()
                        .unwrap_or((None, None));
                    turns.push(RecordedTurn {
                        turn: number,
                        content: message.content.as_ref().map(|content| match content {
                            TranscriptContent::Text(text) => text.clone(),
                            TranscriptContent::Parts(parts) => parts.join("\n"),
                        }),
                        tool_calls: message
                            .tool_calls
                            .iter()
                            .map(|call| RecordedToolCall {
                                id: call.id.clone(),
                                name: call.function.name.clone(),
                                arguments: call.function.arguments.clone(),
                                result: None,
                            })
                            .collect(),
                        usage,
                        duration_ms,
                    });
                }
                "tool" => {
                    let Some(id) = message.tool_call_id.as_deref() else {
                        return Err(malformed(
                            "a tool message carries no tool_call_id".to_owned(),
                        ));
                    };
                    let result = content_text(&message.content);
                    let matched = turns
                        .iter_mut()
                        .rev()
                        .find_map(|turn| turn.tool_calls.iter_mut().find(|call| call.id == id));
                    match matched {
                        Some(call) => call.result = Some(result),
                        None => {
                            return Err(malformed(format!(
                                "a tool result answers unknown call {id:?}"
                            )));
                        }
                    }
                }
                // A chat asset's bytes follow its tool result as a user message; nothing to keep.
                _ => {}
            }
        }
        let answer = turns
            .last()
            .filter(|turn| turn.tool_calls.is_empty())
            .and_then(|turn| turn.content.clone())
            .filter(|content| !content.trim().is_empty());

        Ok(Self {
            trace_id: trace_id.to_owned(),
            system,
            history,
            prompt,
            turns,
            answer,
        })
    }

    /// The scripts the recorded model wrote, in order.
    #[must_use]
    pub fn scripts(&self) -> Vec<String> {
        self.turns
            .iter()
            .flat_map(|turn| turn.tool_calls.iter().filter_map(RecordedToolCall::script))
            .collect()
    }

    /// Token usage summed over every accounted turn.
    #[must_use]
    pub fn usage(&self) -> RecordedUsage {
        let mut total = RecordedUsage::default();
        for turn in &self.turns {
            if let Some(usage) = &turn.usage {
                total.add(usage);
            }
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
    for record in records {
        if text(record, "audit.event").as_deref() != Some("accounting.model.turn") {
            continue;
        }
        let Some(trace_id) = text(record, "trace_id") else {
            continue;
        };
        let timestamp = field(record, "_timestamp")
            .and_then(Value::as_i64)
            .or_else(|| text(record, "_timestamp").and_then(|value| value.trim().parse().ok()))
            .unwrap_or_default();
        let turn = unsigned(record, "model.turn").and_then(|turn| u32::try_from(turn).ok());
        let tokens = unsigned(record, "usage.total_tokens");
        let failed = text(record, "outcome").as_deref() == Some("failed");
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
                total_tokens: None,
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
        if let Some(tokens) = tokens {
            entry.total_tokens = Some(
                entry
                    .total_tokens
                    .unwrap_or_default()
                    .saturating_add(tokens),
            );
        }
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

fn content_text(content: &Option<TranscriptContent>) -> String {
    match content {
        Some(TranscriptContent::Text(text)) => text.clone(),
        Some(TranscriptContent::Parts(parts)) => parts.join("\n"),
        None => String::new(),
    }
}

/// What the replayed session runs under, beyond the model.
pub struct ReplayInputs<'a> {
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
    /// Model turns the session made.
    pub model_turns: u32,
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
    /// A session failure that ended the replay, other than a divergence stop.
    #[serde(default)]
    pub error: Option<String>,
}

/// Answers scripts from the recording, and knows when it cannot.
struct ReplayRuntime<'a> {
    recorded: Mutex<VecDeque<(String, ScriptOutcome)>>,
    requested: Mutex<Vec<String>>,
    divergence: Mutex<Option<Divergence>>,
    turns: Mutex<u32>,
    usage: Mutex<RecordedUsage>,
    live: Option<&'a (dyn ScriptRuntime + Sync)>,
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

impl ScriptRuntime for ReplayRuntime<'_> {
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
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
            return outcome;
        }
        let unused = recorded
            .iter()
            .map(|(script, _)| script.clone())
            .collect::<Vec<_>>();
        drop(recorded);
        let turn = *self
            .turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                live.run_script(script, max_capability_calls)
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

    // The tool description is part of the prompt the replayed model is shown, so it has to name
    // the words the live runtime would run; with no live runtime there are none to offer.
    fn command_words(&self) -> Vec<String> {
        self.live.map_or_else(Vec::new, |live| live.command_words())
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

impl ModelUsageObserver for ReplayRuntime<'_> {
    fn observe(&self, usage: Option<ModelUsage>) {
        *self
            .turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        if let Some(usage) = usage {
            self.usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .add(&RecordedUsage::from(usage));
        }
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
        turns: Mutex::new(0),
        usage: Mutex::new(RecordedUsage::default()),
        live: inputs.live,
    };
    let system = match inputs.system {
        Some(system) => Some(system.to_owned()),
        None => {
            let kept = recorded
                .system
                .iter()
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
            Some(answer) => crate::prompt::ConversationTurn::completed(&exchange.user, answer),
            None => crate::prompt::ConversationTurn::unanswered(&exchange.user),
        });
    }
    let mut session = SessionInputs::new(&recorded.prompt, inputs.limits)
        .with_system(system.as_deref())
        .with_skills(inputs.skills)
        .with_usage_observer(&runtime)
        .with_cancellation(&runtime);
    if inputs.improvement_suggestions {
        session = session.with_improvement_suggestions();
    }
    let outcome = run_prompt_session(model, &runtime, session, &mut history);
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
            model_turns: u32::try_from(recorded.turns.len()).unwrap_or(u32::MAX),
            scripts: recorded.scripts(),
            answer: recorded.answer.clone(),
            usage: recorded.usage(),
        },
        replayed: SessionSummary {
            model_turns: *runtime
                .turns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            scripts: runtime
                .requested
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            answer,
            usage: *runtime
                .usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        },
        divergence,
        suggestions,
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
        DivergenceHandling, RecordedSession, RecordingError, ReplayInputs, SessionListing,
        list_sessions, outcome_from_result, replay,
    };
    use crate::prompt::{PromptLimits, SCRIPT_TOOL_NAME};

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
                    total_tokens: Some(7),
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
        ) -> Result<AssistantTurn, ModelError> {
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
        assert_eq!(report.replayed.model_turns, 2);
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
        assert_eq!(systems[0], vec!["Be terse.".to_owned()]);
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
        assert_eq!(report.replayed.model_turns, 1);
        // Without an override the recorded instructions were replayed.
        let systems = model.systems.lock().expect("systems");
        assert_eq!(systems[0], vec!["Be brief.".to_owned()]);
    }
}
