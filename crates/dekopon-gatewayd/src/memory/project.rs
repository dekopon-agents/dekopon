use std::{fs::File, num::NonZeroU8, sync::Arc};

use dekopon_agent::memory_tools::Rung;
use dekopon_model::{
    body::OpaqueSource,
    model::{ModelKey, ModelMessage},
    replay::{ReplayCall, ReplayMessage, ReplayReasoning},
    text::Text,
};

use super::{
    journal::{
        JournalError,
        records::{Field, Head, NamedTool, decode_escaped, escaped_text},
        tools::envelope,
    },
    record::{RecordId, Slot},
};

/// One record's place in the previous turn's projection, kept in the conversation slot so the
/// next turn changes only what entered or left.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Prior {
    pub record: RecordId,
    pub rung: Option<Rung>,
    pub pin: Option<Rung>,
    pub hot: bool,
}

pub(crate) struct ProjectionInput<'a> {
    pub file: &'a Arc<File>,
    pub records: &'a [Head],
    pub prior: &'a [Prior],
    pub hot_turns: usize,
    pub max_turns: usize,
    pub max_bytes: usize,
    pub model: &'a ModelKey,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RungCounts {
    pub native: u32,
    pub summary: u32,
    pub metadata: u32,
}

pub(crate) struct Projected {
    pub messages: Vec<ModelMessage>,
    pub prior: Vec<Prior>,
    pub omitted: Vec<RecordId>,
    pub opaque_dropped: u32,
    pub repacked: bool,
    pub rungs: RungCounts,
    pub bytes: u64,
}

const EARLIER: &str = "[gateway: earlier conversation]";

/// The rungs a record can replay at, most detailed first: native, each generated summary from the
/// deepest part down, then metadata.
fn ladder(head: &Head) -> Vec<Rung> {
    let mut rungs = vec![Rung::Native];
    rungs.extend(generated(head).into_iter().rev().map(Rung::Summary));
    rungs.push(Rung::Metadata);
    rungs
}

fn generated(head: &Head) -> Vec<NonZeroU8> {
    head.header
        .summaries
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot, Slot::Generated(_)))
        .filter_map(|(index, _)| u8::try_from(index + 1).ok().and_then(NonZeroU8::new))
        .collect()
}

fn ladder_start(head: &Head) -> Rung {
    if let Some(pin) = head.header.pin.filter(|pin| ladder(head).contains(pin)) {
        return pin;
    }
    if let Some(choice) = head.header.choice.filter(|choice| ladder(head).contains(choice)) {
        return choice;
    }
    generated(head)
        .last()
        .copied()
        .map_or(Rung::Native, Rung::Summary)
}

fn step_down(head: &Head, rung: Option<Rung>) -> Option<Rung> {
    let rung = rung?;
    let ladder = ladder(head);
    let at = ladder.iter().position(|candidate| *candidate == rung)?;
    ladder.get(at + 1).copied()
}

fn summary_text(head: &Head, part: NonZeroU8) -> String {
    head.header.summaries[..usize::from(part.get())]
        .iter()
        .filter_map(|slot| match slot {
            Slot::Generated(text) => Some(text.as_str()),
            Slot::Queued | Slot::Running | Slot::Error(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_lines(head: &Head) -> String {
    let mut lines = String::new();
    for span in &head.header.spans {
        if let Field::Result { outcome, .. } = span.field {
            let tool = head
                .header
                .spans
                .iter()
                .find_map(|other| match other.field {
                    Field::Arguments { tool, .. } if other.item == span.item => Some(tool),
                    _ => None,
                })
                .unwrap_or(NamedTool::Rejected);
            let outcome = match outcome {
                dekopon_agent::memory::Outcome::Exit(code) => format!("exit {code}"),
                dekopon_agent::memory::Outcome::Refused => "refused".to_owned(),
                dekopon_agent::memory::Outcome::Failed => "failed".to_owned(),
            };
            lines.push_str(&format!("\n[tool: {}, {outcome}]", tool_label(tool)));
        }
    }
    lines
}

const fn tool_label(tool: NamedTool) -> &'static str {
    match tool {
        NamedTool::Bash => "bash",
        NamedTool::ReadSkill => "read_skill",
        NamedTool::FetchChatAsset => "fetch_chat_asset",
        NamedTool::ReadMemoryRecord => "read_memory_record",
        NamedTool::SetMemoryPin => "set_memory_pin",
        NamedTool::ClearMemoryPin => "clear_memory_pin",
        NamedTool::InspectAgentConfig => "inspect_agent_config",
        NamedTool::SuggestImprovement => "suggest_improvement",
        NamedTool::Wake => "wake",
        NamedTool::Rejected => "rejected",
    }
}

fn envelope_text(head: &Head, rung: Rung) -> String {
    let mut text = envelope(head, rung);
    if let Rung::Summary(part) = rung {
        text.push('\n');
        text.push_str(&summary_text(head, part));
    }
    text.push_str(&tool_lines(head));
    text
}

struct Native {
    messages: Vec<ReplayMessage>,
    bytes: u64,
    opaque_dropped: u32,
}

#[derive(Default)]
struct Step {
    content: Option<Text>,
    calls: Vec<ReplayCall>,
    reasoning: Vec<ReplayReasoning>,
    results: Vec<ReplayMessage>,
}

impl Step {
    fn flush(&mut self, out: &mut Vec<ReplayMessage>, model: &ModelKey) {
        let step = std::mem::take(self);
        let message = ReplayMessage::Assistant {
            content: step.content,
            calls: step.calls,
            reasoning: step.reasoning,
        };
        if !message.is_empty_for(model) {
            out.push(message);
        }
        out.extend(step.results);
    }

    fn started(&self) -> bool {
        self.content.is_some() || !self.calls.is_empty()
    }
}

fn slice(file: &Arc<File>, head: &Head, rel_offset: u64, len: u64) -> Result<Text, JournalError> {
    Ok(Text::File {
        file: Arc::clone(file),
        offset: head
            .body_at
            .checked_add(rel_offset)
            .ok_or(JournalError::Corrupt)?,
        len,
    })
}

fn is_object(file: &File, offset: u64, len: u64) -> Result<bool, JournalError> {
    let mut decoded = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    let mut buffer = vec![0; 64 * 1024];
    decode_escaped(file, offset, len, &mut buffer, &mut |bytes| {
        decoded.extend_from_slice(bytes);
    })?;
    Ok(serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&decoded).is_ok())
}

/// Verbatim steps within the record's share: text items first, then exchanges, cutting only
/// result text; an argument that does not fit, or is not an object, becomes `{"cut":N}`.
fn native(
    file: &Arc<File>,
    head: &Head,
    share: u64,
    model: &ModelKey,
) -> Result<Native, JournalError> {
    let spans = &head.header.spans;
    let text_bytes: u64 = spans
        .iter()
        .filter(|span| {
            matches!(
                span.field,
                Field::User { .. } | Field::Steer { .. } | Field::Assistant { .. }
            )
        })
        .map(|span| span.len)
        .sum();
    let mut left = share.saturating_sub(text_bytes);
    let mut out = Vec::new();
    let mut step = Step::default();
    let mut bytes = text_bytes;
    let mut opaque_dropped = 0;
    let mut index = 0;
    while index < spans.len() {
        let item = spans[index].item;
        let end = spans[index..]
            .iter()
            .position(|span| span.item != item)
            .map_or(spans.len(), |count| index + count);
        let group = &spans[index..end];
        index = end;
        let first = &group[0];
        match &first.field {
            Field::User { .. } | Field::Steer { .. } => {
                step.flush(&mut out, model);
                out.push(ReplayMessage::User(vec![slice(
                    file,
                    head,
                    first.rel_offset,
                    first.len,
                )?]));
            }
            Field::Assistant { .. } => {
                if step.started() {
                    step.flush(&mut out, model);
                }
                step.content = Some(slice(file, head, first.rel_offset, first.len)?);
            }
            Field::Plain { .. } => {
                if step.started() {
                    step.flush(&mut out, model);
                }
            }
            Field::Opaque { model: stored } => {
                if step.started() {
                    step.flush(&mut out, model);
                }
                if stored == model {
                    bytes = bytes.saturating_add(first.len);
                    step.reasoning.push(ReplayReasoning {
                        model: stored.clone(),
                        value: OpaqueSource::File {
                            file: Arc::clone(file),
                            offset: head
                                .body_at
                                .checked_add(first.rel_offset)
                                .ok_or(JournalError::Corrupt)?,
                            len: first.len,
                        },
                    });
                } else {
                    opaque_dropped += 1;
                }
            }
            Field::CallId | Field::RejectedTool | Field::Arguments { .. } | Field::Result { .. } => {
                let mut id = None;
                let mut name = None;
                let mut arguments = None;
                let mut result = None;
                for span in group {
                    let offset = head
                        .body_at
                        .checked_add(span.rel_offset)
                        .ok_or(JournalError::Corrupt)?;
                    match &span.field {
                        Field::CallId => id = Some(escaped_text(file, offset, span.len, 1024)?),
                        Field::RejectedTool => {
                            name = Some(escaped_text(file, offset, span.len, 1024)?);
                        }
                        Field::Arguments {
                            tool,
                            original_bytes,
                        } => {
                            if name.is_none() {
                                name = Some(tool_label(*tool).to_owned());
                            }
                            arguments = Some(if span.len <= left && is_object(file, offset, span.len)? {
                                left -= span.len;
                                bytes = bytes.saturating_add(span.len);
                                slice(file, head, span.rel_offset, span.len)?
                            } else {
                                let cut = serde_json::json!({ "cut": original_bytes }).to_string();
                                bytes = bytes.saturating_add(cut.len() as u64);
                                Text::Owned(cut)
                            });
                        }
                        Field::Result { .. } => {
                            let text = slice(file, head, span.rel_offset, span.len)?
                                .prefix(left)
                                .map_err(JournalError::from)?;
                            let used = match &text {
                                Text::File { len, .. } => *len,
                                Text::Owned(text) => text.len() as u64,
                                Text::Opaque(_) => 0,
                            };
                            left = left.saturating_sub(used);
                            bytes = bytes.saturating_add(used);
                            result = Some(text);
                        }
                        Field::User { .. }
                        | Field::Steer { .. }
                        | Field::Assistant { .. }
                        | Field::Plain { .. }
                        | Field::Opaque { .. } => return Err(JournalError::Corrupt),
                    }
                }
                let (Some(id), Some(name), Some(arguments), Some(text)) =
                    (id, name, arguments, result)
                else {
                    return Err(JournalError::Corrupt);
                };
                let id: dekopon_model::model::ToolCallId = id.into();
                step.calls.push(ReplayCall {
                    id: id.clone(),
                    name,
                    arguments,
                });
                step.results.push(ReplayMessage::Tool { id, text });
            }
        }
    }
    step.flush(&mut out, model);
    Ok(Native {
        messages: out,
        bytes,
        opaque_dropped,
    })
}

fn cost(head: &Head, rung: Option<Rung>, native_bytes: u64) -> u64 {
    match rung {
        None => 0,
        Some(Rung::Native) => native_bytes,
        Some(rung @ (Rung::Summary(_) | Rung::Metadata)) => envelope_text(head, rung).len() as u64 + 1,
    }
}

pub(crate) fn project(input: &ProjectionInput<'_>) -> Result<Projected, JournalError> {
    let records = input.records;
    let hot_turns = input.hot_turns.max(1);
    let hot_from = records.len().saturating_sub(hot_turns);
    let share = (input.max_bytes / hot_turns) as u64;
    let mut natives = Vec::with_capacity(records.len());
    for head in records {
        natives.push(native(input.file, head, share, input.model)?);
    }
    let prior_of = |record: RecordId| input.prior.iter().find(|prior| prior.record == record);
    let fresh = |index: usize, head: &Head| -> Option<Rung> {
        if index >= hot_from {
            Some(Rung::Native)
        } else {
            Some(ladder_start(head))
        }
    };
    let mut rungs: Vec<Option<Rung>> = records
        .iter()
        .enumerate()
        .map(|(index, head)| {
            let hot = index >= hot_from;
            match prior_of(head.header.id) {
                Some(prior) if prior.hot == hot && prior.pin == head.header.pin => {
                    prior.rung.filter(|rung| ladder(head).contains(rung))
                }
                Some(_) | None => fresh(index, head),
            }
        })
        .collect();
    let total = |rungs: &[Option<Rung>]| -> u64 {
        rungs
            .iter()
            .zip(records)
            .zip(&natives)
            .map(|((rung, head), native)| cost(head, *rung, native.bytes))
            .sum()
    };
    let budget = input.max_bytes as u64;
    let shown = rungs.iter().filter(|rung| rung.is_some()).count();
    let mut repacked = false;
    if total(&rungs) > budget || shown > input.max_turns.saturating_mul(2) || records.len() >= input.max_turns.saturating_mul(2) {
        repacked = true;
        rungs = records
            .iter()
            .enumerate()
            .map(|(index, head)| fresh(index, head))
            .collect();
        let keep_from = records.len().saturating_sub(input.max_turns);
        for rung in rungs.iter_mut().take(keep_from) {
            *rung = None;
        }
        let target = budget.saturating_mul(3) / 4;
        while total(&rungs) > target {
            let older = (0..hot_from).find(|index| rungs[*index].is_some());
            if let Some(index) = older {
                rungs[index] = step_down(&records[index], rungs[index]);
                continue;
            }
            match (hot_from..records.len()).find(|index| rungs[*index].is_some()) {
                Some(index) => rungs[index] = None,
                None => break,
            }
        }
    }
    let mut messages = Vec::new();
    let mut earlier = Vec::new();
    let mut omitted = Vec::new();
    let mut prior = Vec::with_capacity(records.len());
    let mut counts = RungCounts::default();
    let mut opaque_dropped = 0;
    let mut bytes = 0;
    for (index, ((head, rung), native)) in records
        .iter()
        .zip(&rungs)
        .zip(natives)
        .enumerate()
    {
        prior.push(Prior {
            record: head.header.id,
            rung: *rung,
            pin: head.header.pin,
            hot: index >= hot_from,
        });
        bytes += cost(head, *rung, native.bytes);
        match rung {
            None => omitted.push(head.header.id),
            Some(Rung::Native) => {
                counts.native += 1;
                opaque_dropped += native.opaque_dropped;
                messages.extend(
                    native
                        .messages
                        .into_iter()
                        .map(|message| ModelMessage::Journal { message }),
                );
            }
            Some(rung @ (Rung::Summary(_) | Rung::Metadata)) => {
                match rung {
                    Rung::Summary(_) => counts.summary += 1,
                    Rung::Native | Rung::Metadata => counts.metadata += 1,
                }
                earlier.push(Text::Owned(format!("\n{}", envelope_text(head, *rung))));
            }
        }
    }
    if !earlier.is_empty() {
        earlier.insert(0, Text::Owned(EARLIER.to_owned()));
        messages.insert(
            0,
            ModelMessage::Journal {
                message: ReplayMessage::User(earlier),
            },
        );
    }
    Ok(Projected {
        messages,
        prior,
        omitted,
        opaque_dropped,
        repacked,
        rungs: counts,
        bytes,
    })
}

/// The record a decider is asked about this turn: the oldest older record with no choice, all
/// slots final and at least one generated summary.
pub(crate) fn decider_candidate(records: &[Head], hot_turns: usize) -> Option<&Head> {
    let hot_from = records.len().saturating_sub(hot_turns.max(1));
    records[..hot_from].iter().find(|head| {
        head.header.choice.is_none()
            && !head.header.summaries.iter().any(Slot::is_pending)
            && head
                .header
                .summaries
                .iter()
                .any(|slot| matches!(slot, Slot::Generated(_)))
    })
}

pub(crate) fn decider_labels(head: &Head) -> Vec<Rung> {
    let mut labels: Vec<Rung> = generated(head).into_iter().map(Rung::Summary).collect();
    labels.push(Rung::Metadata);
    labels
}
