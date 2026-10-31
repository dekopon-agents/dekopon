use std::{fs::File, num::NonZeroU8, sync::Arc};

mod assets;
mod object;

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
        records::{Field, Head, NamedTool, Unescaped, escaped_text},
        tools::envelope,
    },
    record::{RecordId, Slot},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PriorAsset {
    pub number: dekopon_core::AssetNumber,
    pub rung: Rung,
    pub pin: Option<Rung>,
}

/// One record's place in the previous turn's projection, kept in the conversation slot so the
/// next turn changes only what entered or left.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PriorRecord {
    pub record: RecordId,
    pub rung: Option<Rung>,
    pub pin: Option<Rung>,
    pub hot: bool,
    pub assets: Vec<PriorAsset>,
}

pub(crate) struct ProjectionInput<'a> {
    pub file: &'a Arc<File>,
    pub records: &'a [Head],
    pub prior: &'a [PriorRecord],
    pub hot_turns: usize,
    pub max_turns: usize,
    pub max_bytes: usize,
    pub model: &'a ModelKey,
    pub assets: Option<crate::asset::MemoryAssets<'a>>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RungCounts {
    pub native: u32,
    pub summary: u32,
    pub metadata: u32,
}

pub(crate) struct Projected {
    pub messages: Vec<ModelMessage>,
    pub prior: Vec<PriorRecord>,
    pub omitted: Vec<RecordId>,
    pub opaque_dropped: u32,
    pub repacked: bool,
    pub rungs: RungCounts,
    pub bytes: u64,
    pub assets: assets::Counts,
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
    if let Some(choice) = head
        .header
        .choice
        .filter(|choice| ladder(head).contains(choice))
    {
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
    let mut item = None;
    let mut tool = NamedTool::Rejected;
    for span in &head.header.spans {
        if item != Some(span.item) {
            item = Some(span.item);
            tool = NamedTool::Rejected;
        }
        match span.field {
            Field::Arguments { tool: named, .. } => tool = named,
            Field::Result { outcome, .. } => {
                let outcome = match outcome {
                    dekopon_agent::memory::Outcome::Exit(code) => format!("exit {code}"),
                    dekopon_agent::memory::Outcome::Refused => "refused".to_owned(),
                    dekopon_agent::memory::Outcome::Failed => "failed".to_owned(),
                };
                lines.push_str(&format!("\n[tool: {}, {outcome}]", tool.name()));
            }
            Field::User { .. }
            | Field::Steer { .. }
            | Field::Assistant { .. }
            | Field::Plain { .. }
            | Field::Opaque { .. }
            | Field::CallId
            | Field::RejectedTool => {}
        }
    }
    lines
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
    positions: Vec<(u32, usize)>,
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
    object::is_json_object(Unescaped::new(file, offset, len, 16 * 1024)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            JournalError::Corrupt
        } else {
            JournalError::Io { kind: error.kind() }
        }
    })
}

fn asset_positions(
    head: &Head,
    item: u32,
    next_item: u32,
    at: usize,
) -> impl Iterator<Item = (u32, usize)> {
    head.header
        .arrived
        .iter()
        .filter(move |asset| asset.item > item && asset.item < next_item)
        .map(move |asset| (asset.item, at))
}

/// Verbatim steps within the record's share: text items first, then exchanges, cutting only
/// result text; an argument that does not fit, or is not an object, becomes `{"cut":N}`.
fn native(
    file: &Arc<File>,
    head: &Head,
    share: u64,
    model: &ModelKey,
    asset_bytes: u64,
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
    let mut left = share.saturating_sub(text_bytes.saturating_add(asset_bytes));
    let mut out = Vec::new();
    let mut step = Step::default();
    let mut bytes = text_bytes;
    let mut opaque_dropped = 0;
    let mut index = 0;
    let mut positions = Vec::new();
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
                let next_item = spans.get(index).map_or(u32::MAX, |span| span.item);
                positions.extend(asset_positions(head, item, next_item, out.len()));
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
            Field::CallId
            | Field::RejectedTool
            | Field::Arguments { .. }
            | Field::Result { .. } => {
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
                                name = Some(tool.name().to_owned());
                            }
                            arguments =
                                Some(if span.len <= left && is_object(file, offset, span.len)? {
                                    left -= span.len;
                                    bytes = bytes.saturating_add(span.len);
                                    slice(file, head, span.rel_offset, span.len)?
                                } else {
                                    let cut =
                                        serde_json::json!({ "cut": original_bytes }).to_string();
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
        positions,
    })
}

fn cost(head: &Head, rung: Option<Rung>, native_bytes: u64) -> u64 {
    match rung {
        None => 0,
        Some(Rung::Native) => native_bytes,
        Some(rung @ (Rung::Summary(_) | Rung::Metadata)) => {
            envelope_text(head, rung).len() as u64 + 1
        }
    }
}

fn asset_text(rows: &[assets::Row]) -> u64 {
    rows.iter().map(assets::Row::text_bytes).sum()
}

fn projected_cost(head: &Head, rung: Option<Rung>, native: &Native, rows: &[assets::Row]) -> u64 {
    let text = match rung {
        Some(Rung::Native) => asset_text(rows),
        Some(Rung::Summary(_)) => rows
            .iter()
            .filter_map(assets::Row::line)
            .map(|line| line.len() as u64 + 1)
            .sum(),
        Some(Rung::Metadata) | None => 0,
    };
    cost(head, rung, native.bytes) + text
}

fn asset_visible(rung: Option<Rung>) -> bool {
    match rung {
        Some(Rung::Native | Rung::Summary(_)) => true,
        Some(Rung::Metadata) | None => false,
    }
}

pub(crate) fn project(input: &ProjectionInput<'_>) -> Result<Projected, JournalError> {
    let records = input.records;
    let hot_turns = input.hot_turns.max(1);
    let hot_from = records.len().saturating_sub(hot_turns);
    let share = (input.max_bytes / hot_turns) as u64;
    let prior_of = |record: RecordId| input.prior.iter().find(|prior| prior.record == record);
    let fresh = |index: usize, head: &Head| {
        if index >= hot_from {
            Some(Rung::Native)
        } else {
            Some(ladder_start(head))
        }
    };
    let mut rungs: Vec<_> = records
        .iter()
        .enumerate()
        .map(|(index, head)| match prior_of(head.header.id) {
            Some(prior) if prior.hot == (index >= hot_from) && prior.pin == head.header.pin => {
                prior.rung.filter(|rung| ladder(head).contains(rung))
            }
            Some(_) | None => fresh(index, head),
        })
        .collect();
    let mut rows: Vec<Vec<_>> = records
        .iter()
        .enumerate()
        .map(|(index, head)| {
            let prior = prior_of(head.header.id);
            let stable = prior.is_some_and(|prior| prior.hot == (index >= hot_from));
            head.header
                .arrived
                .iter()
                .filter(|_| input.assets.is_some())
                .map(|asset| assets::Row::new(asset, head, prior, stable))
                .collect()
        })
        .collect();
    for (index, row) in rows.iter_mut().enumerate() {
        for asset in row {
            asset.prepare(&records[index], input.assets, asset_visible(rungs[index]));
        }
    }
    let mut natives: Vec<_> = records
        .iter()
        .zip(&rows)
        .map(|(head, rows)| native(input.file, head, share, input.model, asset_text(rows)))
        .collect::<Result<_, _>>()?;
    let total = |rungs: &[Option<Rung>], natives: &[Native], rows: &[Vec<assets::Row>]| -> u64 {
        records
            .iter()
            .enumerate()
            .map(|(i, head)| projected_cost(head, rungs[i], &natives[i], &rows[i]))
            .sum()
    };
    fit_natives(input, &rungs, &mut rows, &mut natives, share)?;
    let budget = input.max_bytes as u64;
    let shown = rungs.iter().filter(|rung| rung.is_some()).count();
    let mut repacked = false;
    if total(&rungs, &natives, &rows) > budget
        || shown > input.max_turns.saturating_mul(2)
        || (input.prior.is_empty() && records.len() >= input.max_turns.saturating_mul(2))
    {
        repacked = true;
        rungs = records
            .iter()
            .enumerate()
            .map(|(i, head)| fresh(i, head))
            .collect();
        for (i, assets) in rows.iter_mut().enumerate() {
            for asset in assets {
                asset.reset();
                asset.prepare(&records[i], input.assets, asset_visible(rungs[i]));
            }
        }
        for rung in rungs
            .iter_mut()
            .take(records.len().saturating_sub(input.max_turns))
        {
            *rung = None;
        }
        fit_natives(input, &rungs, &mut rows, &mut natives, share)?;
        for (i, head) in records.iter().enumerate() {
            natives[i] = native(input.file, head, share, input.model, asset_text(&rows[i]))?;
        }
        let target = budget.saturating_mul(3) / 4;
        while total(&rungs, &natives, &rows) > target {
            let asset = (0..records.len())
                .filter(|i| asset_visible(rungs[*i]))
                .find_map(|i| {
                    rows[i]
                        .iter_mut()
                        .find_map(|asset| asset.step().then_some(i))
                });
            if let Some(i) = asset {
                natives[i] = native(
                    input.file,
                    &records[i],
                    share,
                    input.model,
                    asset_text(&rows[i]),
                )?;
                continue;
            }
            if let Some(i) = (0..hot_from).find(|i| rungs[*i].is_some()) {
                rungs[i] = step_down(&records[i], rungs[i]);
                continue;
            }
            match (hot_from..records.len()).find(|i| rungs[*i].is_some()) {
                Some(i) => rungs[i] = None,
                None => break,
            }
        }
    }
    assemble(input, rungs, rows, natives, repacked)
}

fn fit_natives(
    input: &ProjectionInput<'_>,
    rungs: &[Option<Rung>],
    rows: &mut [Vec<assets::Row>],
    natives: &mut [Native],
    share: u64,
) -> Result<(), JournalError> {
    while rows
        .iter()
        .enumerate()
        .filter(|(i, _)| asset_visible(rungs[*i]))
        .flat_map(|(_, rows)| rows)
        .map(assets::Row::bytes)
        .sum::<u64>()
        > dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES as u64
    {
        let Some(i) = (0..input.records.len())
            .filter(|i| asset_visible(rungs[*i]))
            .find_map(|i| {
                rows[i]
                    .iter_mut()
                    .find(|asset| asset.bytes() > 0)
                    .map(|asset| {
                        asset.step();
                        i
                    })
            })
        else {
            break;
        };
        natives[i] = native(
            input.file,
            &input.records[i],
            share,
            input.model,
            asset_text(&rows[i]),
        )?;
    }
    Ok(())
}

fn flush_earlier(texts: &mut Vec<Text>, messages: &mut Vec<ModelMessage>) {
    if !texts.is_empty() {
        messages.push(ModelMessage::Journal {
            message: ReplayMessage::User(std::mem::take(texts)),
        });
    }
}

fn assemble(
    input: &ProjectionInput<'_>,
    rungs: Vec<Option<Rung>>,
    mut rows: Vec<Vec<assets::Row>>,
    natives: Vec<Native>,
    repacked: bool,
) -> Result<Projected, JournalError> {
    let mut messages = Vec::new();
    let mut earlier = Vec::new();
    let mut earlier_messages = Vec::new();
    let mut omitted = Vec::new();
    let mut prior = Vec::with_capacity(input.records.len());
    let mut counts = RungCounts::default();
    let mut asset_counts = assets::Counts::default();
    let mut opaque_dropped = 0;
    let mut bytes = 0;
    let hot_from = input.records.len().saturating_sub(input.hot_turns.max(1));
    for (i, ((head, rung), native)) in input.records.iter().zip(&rungs).zip(natives).enumerate() {
        prior.push(PriorRecord {
            record: head.header.id,
            rung: *rung,
            pin: head.header.pin,
            hot: i >= hot_from,
            assets: rows[i]
                .iter()
                .map(|asset| PriorAsset {
                    number: asset.number,
                    rung: asset.rung,
                    pin: asset.pin,
                })
                .collect(),
        });
        bytes += projected_cost(head, *rung, &native, &rows[i]);
        asset_counts.lines += head.asset_lines.len() as u64;
        for asset in &rows[i] {
            asset.count(&mut asset_counts, asset_visible(*rung));
        }
        match rung {
            None => omitted.push(head.header.id),
            Some(Rung::Native) => {
                counts.native += 1;
                opaque_dropped += native.opaque_dropped;
                for (index, message) in native.messages.into_iter().enumerate() {
                    messages.push(ModelMessage::Journal { message });
                    for asset in &mut rows[i] {
                        if native
                            .positions
                            .iter()
                            .any(|(item, at)| *item == asset.item && *at == index + 1)
                            && let Some(message) = asset.message()
                        {
                            messages.push(message);
                        }
                    }
                }
            }
            Some(rung @ (Rung::Summary(_) | Rung::Metadata)) => {
                match rung {
                    Rung::Summary(_) => counts.summary += 1,
                    Rung::Native | Rung::Metadata => counts.metadata += 1,
                }
                if earlier.is_empty() && earlier_messages.is_empty() {
                    earlier.push(Text::Owned(EARLIER.to_owned()));
                }
                let mut text = envelope(head, *rung);
                if let Rung::Summary(part) = rung {
                    text.push('\n');
                    text.push_str(&summary_text(head, *part));
                }
                if asset_visible(Some(*rung)) {
                    for asset in &rows[i] {
                        if let Some(line) = asset.line() {
                            text.push('\n');
                            text.push_str(&line);
                        }
                    }
                }
                text.push_str(&tool_lines(head));
                earlier.push(Text::Owned(format!("\n{text}")));
                if asset_visible(Some(*rung)) {
                    for asset in &mut rows[i] {
                        if asset.shown == Rung::Native
                            && let Some(message) = asset.message()
                        {
                            flush_earlier(&mut earlier, &mut earlier_messages);
                            earlier_messages.push(message);
                        }
                    }
                }
            }
        }
    }
    flush_earlier(&mut earlier, &mut earlier_messages);
    earlier_messages.extend(messages);
    Ok(Projected {
        messages: earlier_messages,
        prior,
        omitted,
        opaque_dropped,
        repacked,
        rungs: counts,
        bytes,
        assets: asset_counts,
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FileSlices {
    pub count: u64,
    pub bytes: u64,
}

impl FileSlices {
    fn text(&mut self, text: &Text) {
        match text {
            Text::File { len, .. } => self.add(*len),
            Text::Opaque(source) => self.source(source),
            Text::Owned(_) => {}
        }
    }

    fn source(&mut self, source: &OpaqueSource) {
        match source {
            OpaqueSource::File { len, .. } => self.add(*len),
            OpaqueSource::Owned(_) => {}
        }
    }

    fn add(&mut self, len: u64) {
        self.count += 1;
        self.bytes += len;
    }
}

pub(crate) fn file_slices(messages: &[ModelMessage]) -> FileSlices {
    let mut slices = FileSlices::default();
    for message in messages {
        match message {
            ModelMessage::Journal { message } => match message {
                ReplayMessage::User(texts) => texts.iter().for_each(|text| slices.text(text)),
                ReplayMessage::Assistant {
                    content,
                    calls,
                    reasoning,
                } => {
                    content.iter().for_each(|text| slices.text(text));
                    calls.iter().for_each(|call| slices.text(&call.arguments));
                    reasoning.iter().for_each(|item| slices.source(&item.value));
                }
                ReplayMessage::Tool { text, .. } => slices.text(text),
            },
            ModelMessage::System { .. }
            | ModelMessage::User { .. }
            | ModelMessage::Assistant { .. }
            | ModelMessage::ToolResults { .. } => {}
        }
    }
    slices
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, sync::Arc};

    use dekopon_model::{
        body::OpaqueSource,
        model::{Backend, ModelKey, ModelMessage},
        replay::{ReplayCall, ReplayMessage, ReplayReasoning},
        text::Text,
    };

    use super::{FileSlices, file_slices};

    fn file(len: u64) -> Arc<std::fs::File> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&vec![b'x'; usize::try_from(len).unwrap()])
            .unwrap();
        Arc::new(file)
    }

    fn reasoning(value: OpaqueSource) -> ReplayReasoning {
        ReplayReasoning {
            model: ModelKey::new(Backend::OpenRouter, "alias"),
            value,
        }
    }

    #[test]
    fn opaque_file_slices_count_in_the_body_metrics_and_owned_sources_do_not() {
        let opaque = OpaqueSource::File {
            file: file(47),
            offset: 0,
            len: 47,
        };
        let owned = OpaqueSource::owned(b"{\"owned\":1}");
        let reasoning_only = [ModelMessage::Journal {
            message: ReplayMessage::Assistant {
                content: None,
                calls: Vec::new(),
                reasoning: vec![reasoning(opaque.clone()), reasoning(owned.clone())],
            },
        }];
        assert_eq!(
            file_slices(&reasoning_only),
            FileSlices {
                count: 1,
                bytes: 47
            }
        );
        let text = |len: u64| Text::File {
            file: file(len),
            offset: 0,
            len,
        };
        let mixed = [
            ModelMessage::user("owned question"),
            ModelMessage::Journal {
                message: ReplayMessage::User(vec![text(10), Text::Owned("owned".to_owned())]),
            },
            ModelMessage::Journal {
                message: ReplayMessage::Assistant {
                    content: Some(text(5)),
                    calls: vec![ReplayCall {
                        id: "call".into(),
                        name: "bash".to_owned(),
                        arguments: text(7),
                    }],
                    reasoning: vec![reasoning(opaque), reasoning(owned)],
                },
            },
            ModelMessage::Journal {
                message: ReplayMessage::Tool {
                    id: "call".into(),
                    text: text(3),
                },
            },
        ];
        assert_eq!(
            file_slices(&mixed),
            FileSlices {
                count: 5,
                bytes: 72
            }
        );
    }
}

#[cfg(test)]
mod asset_tests;
