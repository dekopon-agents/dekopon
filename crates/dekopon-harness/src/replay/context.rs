//! Portable request revisions are context, not additional model calls or execution receipts.
use std::collections::{BTreeMap, BTreeSet};

use dekopon_model::model::{AssistantTurn, ModelMessage, ModelToolCall, assistant_message};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{RecordedExchange, RecordedSession, RecordedTurn, text, unsigned};
use crate::{context::ContextPolicy, history::History};

/// One exported request fragment. A full revision replaces context; a delta appends to it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordedContext {
    pub turn: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub scope: String,
    pub messages: Vec<RecordedMessage>,
}

/// A portable text/tool message. Attachment summaries remain text, never restored binary assets.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedMessage {
    pub role: String,
    #[serde(
        default,
        deserialize_with = "read_content",
        skip_serializing_if = "Option::is_none"
    )]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

fn read_content<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Content {
        Text(String),
        Parts(Vec<String>),
    }
    Ok(
        Option::<Content>::deserialize(deserializer)?.map(|content| match content {
            Content::Text(text) => text,
            Content::Parts(parts) => parts.join("\n"),
        }),
    )
}
impl RecordedMessage {
    pub(super) fn text(&self) -> String {
        self.content.clone().unwrap_or_default()
    }
    fn model_message(&self) -> ModelMessage {
        match self.role.as_str() {
            "system" => ModelMessage::system(self.text()),
            "assistant" => assistant_message(&AssistantTurn {
                content: self.content.clone(),
                tool_calls: self.tool_calls.clone(),
                usage: None,
                replay_items: Vec::new(),
            }),
            "tool" => ModelMessage::tool(
                self.tool_call_id.as_deref().expect("validated tool"),
                self.text(),
            ),
            _ => ModelMessage::user(self.text()),
        }
    }
}

pub(super) fn decode_prompt(record: &Value, turn: u64) -> Result<RecordedContext, String> {
    let revision = unsigned(record, "context.revision");
    match unsigned(record, "transcript.version") {
        Some(2) if revision.is_some() && text(record, "job.id").is_some() => {}
        None if super::field(record, "transcript.version").is_none() => {}
        _ => {
            return Err(format!(
                "turn {turn} has invalid transcript.version or missing revision/job"
            ));
        }
    }
    if super::field(record, "context.revision").is_some() && revision.is_none() {
        return Err(format!("turn {turn} has invalid context.revision"));
    }
    let encoded =
        text(record, "messages").ok_or_else(|| format!("turn {turn} carries no messages"))?;
    let messages =
        serde_json::from_str(&encoded).map_err(|e| format!("turn {turn} messages: {e}"))?;
    Ok(RecordedContext {
        turn: u32::try_from(turn).map_err(|e| format!("model.turn: {e}"))?,
        revision,
        scope: text(record, "transcript.scope").unwrap_or_default(),
        messages,
    })
}

/// Every distinct problem in one message, in the order they were found; `Ok` when there were none.
fn joined(mut problems: Vec<String>) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    problems.retain(|problem| seen.insert(problem.clone()));
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Reports every conflict in one message rather than stopping at the first: a recording is edited
/// by hand, and one round trip per malformed revision is one round trip too many.
pub(super) fn validate_contexts(contexts: &[RecordedContext]) -> Result<(), String> {
    if contexts.is_empty() || contexts.len() > 128 {
        return Err("request context count must be 1..=128".into());
    }
    let mut problems = Vec::new();
    let mut previous: Option<&RecordedContext> = None;
    let mut messages = Vec::new();
    for context in contexts {
        if context.turn == 0
            || previous.is_some_and(|p| p.turn.checked_add(1) != Some(context.turn))
        {
            problems.push(format!(
                "missing or out-of-order prompt before turn {}",
                context.turn
            ));
        }
        match (context.scope.as_str(), previous) {
            ("full", None) => {}
            ("full", Some(p))
                if p.revision
                    .zip(context.revision)
                    .is_some_and(|(old, new)| new > old) => {}
            ("delta", Some(p)) if p.revision == context.revision => {}
            _ => {
                problems.push(format!(
                    "turn {} has invalid full/delta context revision ordering",
                    context.turn
                ));
            }
        }
        if context.scope == "full" {
            messages.clear();
        }
        messages.extend(context.messages.iter());
        if let Err(e) = validate_messages(&messages) {
            problems.push(format!("turn {}: {e}", context.turn));
        }
        if previous.is_none() && messages.last().is_none_or(|m| m.role != "user") {
            problems.push("first request does not end with the user prompt".to_owned());
        }
        previous = Some(context);
    }
    joined(problems)
}

fn validate_messages(messages: &[&RecordedMessage]) -> Result<(), String> {
    let sizes = messages
        .iter()
        .map(|message| serde_json::to_vec(message).map(|encoded| encoded.len()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let mut problems = Vec::new();
    let size = sizes.iter().sum::<usize>();
    if size > crate::context::MAX_CONTEXT_BYTES {
        problems.push("portable request exceeds context byte limit".to_owned());
    }
    let mut pending = BTreeMap::new();
    let mut group_bytes = 0;
    let mut leading = true;
    let mut asset_result = false;
    for (message, size) in messages.iter().zip(sizes) {
        // Asset dispatch appends a byte-free user attachment summary immediately after its
        // result, even when other calls in the assistant batch are still pending — so it does not
        // close an open group. It does end the *byte* group, exactly as the live enforcer in
        // `context::bound_live` does: that resets on any non-`tool` message, and a validator
        // stricter than the enforcer refuses context the session itself would have accepted.
        let attachment = message.role == "user" && asset_result;
        asset_result = false;
        if message.role != "tool" {
            if !attachment && !pending.is_empty() {
                problems.push("incomplete assistant tool group in request".to_owned());
                pending.clear();
            }
            group_bytes = 0;
        }
        group_bytes += size;
        if group_bytes > crate::context::MAX_GROUP_BYTES {
            problems.push("portable tool group exceeds byte limit".to_owned());
        }
        if message.role != "assistant" && !message.tool_calls.is_empty() {
            problems.push("tool calls outside assistant message".to_owned());
        }
        if message.role != "tool" && message.tool_call_id.is_some() {
            problems.push("tool_call_id outside tool message".to_owned());
        }
        match message.role.as_str() {
            "system" if leading => {}
            "user" => leading = false,
            "assistant" => {
                leading = false;
                for call in &message.tool_calls {
                    if call.id.is_empty()
                        || pending
                            .insert(call.id.as_str(), call.function.name.as_str())
                            .is_some()
                    {
                        problems.push("empty or duplicate assistant tool call ID".to_owned());
                    }
                }
            }
            "tool" => {
                leading = false;
                match message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| pending.remove(id))
                {
                    Some(name) => asset_result = name == crate::tools::ASSET_TOOL_NAME,
                    None => problems.push("orphan or duplicate tool result in request".to_owned()),
                }
            }
            _ => problems.push(format!("unexpected message role {:?}", message.role)),
        }
    }
    if !pending.is_empty() {
        problems.push("incomplete assistant tool group in request".to_owned());
    }
    joined(problems)
}

/// Keep the old text-pair projection only where it is lossless. Portable contexts are authoritative.
pub(super) fn legacy_exchanges(messages: &[RecordedMessage]) -> Vec<RecordedExchange> {
    let mut exchanges = Vec::new();
    let mut cursor = 0;
    while cursor < messages.len() {
        let user = &messages[cursor];
        if user.role != "user" {
            return Vec::new();
        }
        cursor += 1;
        let answer = messages.get(cursor).filter(|m| m.role == "assistant");
        if answer.is_some_and(|m| !m.tool_calls.is_empty()) {
            return Vec::new();
        }
        let answer = answer.map(|m| {
            cursor += 1;
            m.text()
        });
        exchanges.push(RecordedExchange {
            user: user.text(),
            answer,
        });
    }
    exchanges
}

pub(super) fn capture_results(
    contexts: &[RecordedContext],
    turns: &mut [RecordedTurn],
    coordinates: &BTreeMap<u64, (String, u64)>,
    job: Option<&str>,
) -> Result<(), String> {
    let mut problems = Vec::new();
    for turn in turns.iter() {
        let mut ids = BTreeSet::new();
        if turn
            .tool_calls
            .iter()
            .any(|c| c.id.is_empty() || !ids.insert(&c.id))
        {
            problems.push(format!(
                "turn {} has empty or duplicate tool call IDs",
                turn.turn
            ));
        }
    }
    if !problems.is_empty() {
        return joined(problems);
    }
    for context in contexts.iter().skip(1) {
        if context.scope == "delta" {
            match turns.iter().find(|t| t.turn + 1 == context.turn) {
                None => problems.push(format!(
                    "turn {} delta has no preceding answer",
                    context.turn
                )),
                Some(previous) => {
                    let assistants: Vec<_> = context
                        .messages
                        .iter()
                        .filter(|m| m.role == "assistant")
                        .collect();
                    if assistants.len() != 1
                        || assistants[0].content.as_deref().unwrap_or_default()
                            != previous.content.as_deref().unwrap_or_default()
                        || assistants[0].tool_calls.len() != previous.tool_calls.len()
                        || assistants[0]
                            .tool_calls
                            .iter()
                            .zip(&previous.tool_calls)
                            .any(|(a, b)| {
                                a.id != b.id
                                    || a.function.name != b.name
                                    || a.function.arguments != b.arguments
                            })
                    {
                        problems.push(format!(
                            "turn {} delta conflicts with preceding answer",
                            context.turn
                        ));
                    }
                }
            }
        }
        let mut seen = BTreeSet::new();
        let mut batch = None;
        for message in &context.messages {
            if message.role == "assistant" {
                batch = Some(message);
            }
            if message.role != "tool" {
                continue;
            }
            let id = message
                .tool_call_id
                .as_deref()
                .expect("validated tool result");
            let assistant = batch.expect("validated tool batch");
            let source = assistant
                .tool_calls
                .iter()
                .find(|c| c.id == id)
                .expect("validated correlation");
            // The walk is contexts × messages × turns × calls, so the host coordinate this ID
            // carries is parsed once here rather than formatted once per call in the inner loop.
            let claimed = host_coordinate(id);
            for turn in turns.iter_mut().filter(|t| t.turn < context.turn) {
                // Deltas carry only the immediately preceding batch. Full rebuilds normalize IDs
                // using the host job and logical call sequence, not a provider ID or text search.
                let coordinate = coordinates.get(&u64::from(turn.turn));
                for (index, call) in turn.tool_calls.iter_mut().enumerate() {
                    let matches = if context.scope == "delta" {
                        turn.turn + 1 == context.turn && call.id == id
                    } else {
                        coordinate.is_some_and(|(call_job, sequence)| {
                            job == Some(call_job.as_str())
                                && claimed == Some((call_job.as_str(), *sequence, index))
                        })
                    };
                    if !matches {
                        continue;
                    }
                    if source.function.name != call.name
                        || (source.function.arguments != call.arguments
                            && call.name != crate::tools::IMAGE_GENERATION_TOOL_NAME)
                    {
                        problems.push(format!(
                            "turn {} tool call conflicts with request context",
                            turn.turn
                        ));
                        continue;
                    }
                    if !seen.insert((turn.turn, index)) {
                        problems.push(format!(
                            "turn {} repeats a tool group in full context",
                            context.turn
                        ));
                        continue;
                    }
                    // A bounded rebuild can repeat an exact excerpt of an earlier result. It must
                    // not replace that result, count as new execution, or invent a missing exit code.
                    let result = message.text();
                    if let Some(old) = &call.result {
                        let excerpt =
                            crate::history::Excerpt::new(old, crate::history::MAX_EXCERPT_BYTES)
                                .render();
                        if result != *old && result != excerpt {
                            problems
                                .push(format!("turn {} has conflicting tool results", turn.turn));
                        }
                    } else if context.scope == "delta" || !result.contains("\n[excerpt; original ")
                    {
                        call.result = Some(result);
                    }
                }
            }
        }
    }
    joined(problems)
}

/// Splits a host-minted tool call ID back into `(job, call sequence, index)`.
///
/// `context::replay_job` mints these as `{job}-{call}-{index}` with canonical decimals, so parsing
/// is reading back what this crate wrote — and it is one parse per ID rather than one `format!`
/// per tool call of every earlier turn.
fn host_coordinate(id: &str) -> Option<(&str, u64, usize)> {
    fn decimal<T: std::str::FromStr>(text: &str) -> Option<T> {
        // A leading zero is not what the minting site writes, so it is not the same coordinate.
        if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
            return None;
        }
        text.parse().ok()
    }
    let (head, index) = id.rsplit_once('-')?;
    let (job, sequence) = head.rsplit_once('-')?;
    Some((job, decimal(sequence)?, decimal(index)?))
}

pub(super) struct ReplayContext(Vec<ModelMessage>);
impl ReplayContext {
    pub(super) fn new(recorded: &RecordedSession) -> Result<Option<Self>, String> {
        if recorded.contexts.is_empty() {
            return Ok(None);
        }
        validate_contexts(&recorded.contexts)?;
        let first = &recorded.contexts[0].messages;
        if first.last().expect("validated request").text() != recorded.prompt {
            return Err("recorded prompt conflicts with initial context".into());
        }
        Ok(Some(Self(
            first[..first.len() - 1]
                .iter()
                .filter(|m| m.role != "system")
                .map(RecordedMessage::model_message)
                .collect(),
        )))
    }
}
impl ContextPolicy for ReplayContext {
    fn select(&self, _: &History) -> Vec<ModelMessage> {
        self.0.clone()
    }
}
