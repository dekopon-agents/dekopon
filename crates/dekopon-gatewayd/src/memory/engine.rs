use std::{
    collections::HashMap,
    num::{NonZeroU8, NonZeroU32, NonZeroUsize},
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime},
};

use dekopon_core::AgentId;
use dekopon_model::{
    error::{InferenceError, TransportFailure},
    model::{ChatModel, CompletionOptions, ModelMessage},
    replay::ReplayMessage,
    text::Text,
};
use dekopon_model_token_governor::{Call, Estimate, Metering, Outcome, Sizes, Tokens, Via};
use tokio::{
    sync::{mpsc, watch},
    task::{Id, JoinSet},
};
use tracing::Instrument as _;

use super::{
    journal::{
        Journal, JournalError,
        records::{Field, Head, Snapshot},
    },
    record::{RecordId, Slot, SlotLine, Subject, SummaryError, SummaryFailure, TurnId},
};
use crate::{gateway::CurrentGeneration, session::ModelCache};

#[derive(Clone, Debug)]
pub(crate) struct SlotTask {
    pub stem: String,
    pub record: RecordId,
    pub part: NonZeroU8,
}

struct Queued {
    task: SlotTask,
    since: Instant,
}

pub(crate) struct Resources {
    pub journal: Arc<Journal>,
    pub current: CurrentGeneration,
    pub models: Arc<ModelCache>,
    pub counts: Arc<Counts>,
}

#[derive(Default)]
pub(crate) struct Counts {
    pub enqueued: AtomicU64,
    pub overflowed: AtomicU64,
    pub evicted: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct Sender {
    queue: mpsc::Sender<Queued>,
    journal: Arc<Journal>,
    pub counts: Arc<Counts>,
}

#[derive(Clone, Copy)]
enum Next {
    Continue,
    Stop,
}

type Binding = Arc<parking_lot::Mutex<Option<TurnId>>>;
struct RunningTask {
    task: SlotTask,
    binding: Binding,
}

struct CancelOnDrop(watch::Sender<bool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

pub(crate) fn spawn(
    resources: Resources,
    queue: NonZeroUsize,
    concurrency: NonZeroUsize,
    owned_tasks: &mut JoinSet<()>,
) -> Sender {
    let (tx, rx) = mpsc::channel(queue.get());
    let sender = Sender {
        queue: tx,
        journal: Arc::clone(&resources.journal),
        counts: Arc::clone(&resources.counts),
    };
    owned_tasks.spawn(dispatch(
        Arc::new(resources),
        rx,
        sender.clone(),
        concurrency,
    ));
    sender
}

impl Sender {
    pub(crate) async fn enqueue(&self, task: SlotTask) {
        match self.queue.try_send(Queued {
            task,
            since: Instant::now(),
        }) {
            Ok(()) => {
                self.counts.enqueued.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Full(queued)) => {
                let journal = Arc::clone(&self.journal);
                let counts = Arc::clone(&self.counts);
                let result = tokio::task::spawn_blocking(move || {
                    let count = mark_later(
                        &journal,
                        &queued.task,
                        None,
                        SummaryFailure::Overflow,
                        "summary queue is full",
                    )?;
                    counts.overflowed.fetch_add(count, Ordering::Relaxed);
                    Ok::<_, JournalError>(())
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    tracing::warn!(
                        event = "gateway_memory_summary_write_failed",
                        "could not persist summary overflow"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

async fn dispatch(
    resources: Arc<Resources>,
    mut receiver: mpsc::Receiver<Queued>,
    sender: Sender,
    concurrency: NonZeroUsize,
) {
    let cancel = CancelOnDrop(watch::channel(false).0);
    let mut running = JoinSet::new();
    let mut tasks: HashMap<Id, RunningTask> = HashMap::new();
    loop {
        tokio::select! {
            result = running.join_next_with_id(), if !running.is_empty() => {
                match result {
                    Some(Ok((id, next))) => {
                        if let Some(running) = tasks.remove(&id) {
                            let task = running.task;
                            match next {
                                Next::Continue => if let Some(part) = task.part.get().checked_add(1).and_then(NonZeroU8::new) { sender.enqueue(SlotTask { part, ..task }).await; },
                                Next::Stop => {}
                            }
                        }
                    }
                    Some(Err(error)) => {
                        if let Some(running) = tasks.remove(&error.id()).filter(|_| error.is_panic()) {
                            let journal = Arc::clone(&resources.journal);
                            let turn = *running.binding.lock();
                            let _written = tokio::task::spawn_blocking(move || mark_later(&journal, &running.task, turn, SummaryFailure::Panicked, "summary task panicked")).await;
                        }
                    }
                    None => {}
                }
            }
            queued = receiver.recv(), if running.len() < concurrency.get() => {
                let Some(queued) = queued else { break; };
                let task = queued.task.clone();
                let binding: Binding = Arc::default();
                let handle = running.spawn(run(Arc::clone(&resources), queued, cancel.0.subscribe(), Arc::clone(&binding)));
                tasks.insert(handle.id(), RunningTask { task, binding });
            }
        }
    }
}

async fn run(
    resources: Arc<Resources>,
    queued: Queued,
    cancel: watch::Receiver<bool>,
    binding: Binding,
) -> Next {
    let span = tracing::info_span!(
        "memory.summarize",
        memory.record.id = queued.task.record.0.get(),
        memory.summary.part = queued.task.part.get(),
        memory.summary.queue_wait_ms = queued.since.elapsed().as_millis() as u64
    );
    let runtime = tokio::runtime::Handle::current();
    let task = queued.task;
    let blocking_span = span.clone();
    match tokio::task::spawn_blocking(move || {
        blocking_span.in_scope(|| summarize(&resources, &task, runtime, cancel, &binding))
    })
    .instrument(span)
    .await
    {
        Ok(next) => next,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_cancelled) => Next::Stop,
    }
}

fn summarize(
    resources: &Resources,
    task: &SlotTask,
    runtime: tokio::runtime::Handle,
    cancel: watch::Receiver<bool>,
    binding: &Binding,
) -> Next {
    let journal = &resources.journal;
    let loaded = journal.record_lookup(&task.stem, task.record);
    let (snapshot, position) = match loaded {
        Ok(Some(snapshot)) => match snapshot
            .records
            .iter()
            .position(|head| head.header.id == task.record)
        {
            Some(position) => (snapshot, position),
            None => return evicted(resources),
        },
        Ok(None) => return evicted(resources),
        Err(error) => {
            return fail(
                resources,
                task,
                None,
                io_failure(error),
                "could not read summary input",
                Next::Continue,
            );
        }
    };
    let head = &snapshot.records[position];
    let expected = Some(head.header.turn);
    *binding.lock() = expected;
    let part = usize::from(task.part.get() - 1);
    let Some(plan) = &head.header.plan else {
        return Next::Stop;
    };
    if !matches!(head.header.summaries.get(part), Some(Slot::Queued)) {
        return Next::Stop;
    }
    let Some(limit) = plan.parts.get(part) else {
        return Next::Stop;
    };
    let next = if part + 1 < plan.parts.len() {
        Next::Continue
    } else {
        Next::Stop
    };
    match write_slot(journal, task, Slot::Running, expected) {
        Ok(true) => {}
        Ok(false) => return evicted(resources),
        Err(error) => {
            return fail(
                resources,
                task,
                expected,
                io_failure(error),
                "could not persist summary start",
                next,
            );
        }
    }
    let current = resources.current.load();
    let Some(model) = current.routes.models.get(&plan.model) else {
        let _written = mark_later(
            journal,
            task,
            expected,
            SummaryFailure::Unconfigured,
            "summary model is no longer configured",
        );
        return Next::Stop;
    };
    let client = match resources.models.client_bound(model, runtime, cancel) {
        Ok(client) => client,
        Err(error) => {
            return fail(
                resources,
                task,
                expected,
                SummaryFailure::ModelFailed,
                &error.to_string(),
                next,
            );
        }
    };
    let input = match input(&snapshot, head, plan.max_input_bytes.get() as usize) {
        Ok(input) => input,
        Err(error) => {
            return fail(
                resources,
                task,
                expected,
                io_failure(error),
                "could not read summary input",
                next,
            );
        }
    };
    let previous: String = head.header.summaries[..part]
        .iter()
        .filter_map(|slot| match slot {
            Slot::Generated(text) => Some(text.as_str()),
            Slot::Queued | Slot::Running | Slot::Error(_) => None,
        })
        .collect();
    let Ok(agent) = task
        .stem
        .split('/')
        .next()
        .unwrap_or_default()
        .parse::<AgentId>()
    else {
        return Next::Stop;
    };
    let call = Call {
        agent,
        model: plan.model.clone(),
        backend: model.backend(),
        via: Via::Summary,
    };
    let slot = match complete_summary(client.as_ref(), call, input, previous, task.part, *limit) {
        Completed::Slot(slot) => slot,
        Completed::Cancelled => return Next::Stop,
        Completed::Refused => {
            let _written = mark_later(
                journal,
                task,
                expected,
                SummaryFailure::Refused,
                "summary provider refused",
            );
            return Next::Stop;
        }
    };
    match write_slot(journal, task, slot, expected) {
        Ok(true) => next,
        Ok(false) => evicted(resources),
        Err(error) => fail(
            resources,
            task,
            expected,
            io_failure(error),
            "could not persist summary result",
            next,
        ),
    }
}

enum Completed {
    Slot(Slot),
    Refused,
    Cancelled,
}

fn complete_summary(
    client: &dyn ChatModel,
    call: Call,
    mut input: Vec<Text>,
    previous: String,
    part: NonZeroU8,
    limit: NonZeroU32,
) -> Completed {
    let instructions = format!(
        "Write only new additive summary information for part {}. Do not repeat earlier parts.",
        part
    );
    input.insert(0, Text::Owned("Source:\n".to_owned()));
    input.push(Text::Owned(format!("\nEarlier parts:\n{previous}")));
    let input_bytes = input.iter().map(text_bytes).sum::<usize>();
    let messages = [
        ModelMessage::system(instructions),
        ModelMessage::Journal {
            message: ReplayMessage::User(input),
        },
    ];
    let estimate = Estimate::from_sizes(
        Sizes {
            bytes: input_bytes + messages[0].content().map_or(0, str::len),
            images: 0,
        },
        None,
        Tokens::from_bytes(limit.get() as usize),
    );
    let mut meter = Metering::unadmitted(call, estimate);
    let result = client.complete(
        &messages,
        &[],
        &CompletionOptions::default(),
        &mut |event| {
            match event {
                dekopon_model::TurnEvent::TextDelta(text) => meter.observe_text(text.len()),
            }
            ControlFlow::Continue(())
        },
    );
    let (usage, bytes, outcome, kind) = match &result {
        Ok(turn) => (
            turn.usage,
            turn.content.as_ref().map_or(0, String::len),
            Outcome::Succeeded,
            None,
        ),
        Err(error) => (
            None,
            0,
            if matches!(error, InferenceError::Cancelled) {
                Outcome::Cancelled
            } else {
                Outcome::Failed
            },
            Some(error.kind().as_str()),
        ),
    };
    meter.finish(usage, bytes, outcome, kind);
    let slot = match result {
        Ok(turn) => match turn
            .content
            .filter(|text| !text.trim().is_empty() && turn.tool_calls.is_empty())
        {
            Some(mut text) => {
                text.truncate(text.floor_char_boundary(limit.get() as usize));
                if text.trim().is_empty() {
                    error_slot(SummaryFailure::Empty, "summary returned no retained text")
                } else {
                    Slot::Generated(text)
                }
            }
            None => error_slot(SummaryFailure::Empty, "summary returned no text"),
        },
        Err(InferenceError::Transport(TransportFailure::Body(error))) => error_slot(
            SummaryFailure::Io { kind: error.kind() },
            "could not read summary input",
        ),
        Err(InferenceError::Cancelled) => return Completed::Cancelled,
        Err(InferenceError::DeadlineExceeded) => {
            error_slot(SummaryFailure::Timeout, "summary model deadline exceeded")
        }
        Err(InferenceError::Provider(error))
            if matches!(
                error.code.as_deref(),
                Some("refused" | "content_filter" | "safety")
            ) || error.status == Some(403) =>
        {
            return Completed::Refused;
        }
        Err(error) => error_slot(SummaryFailure::ModelFailed, &error.to_string()),
    };
    Completed::Slot(slot)
}

fn evicted(resources: &Resources) -> Next {
    resources.counts.evicted.fetch_add(1, Ordering::Relaxed);
    Next::Stop
}

fn text_bytes(text: &Text) -> usize {
    match text {
        Text::Owned(text) => text.len(),
        Text::File { len, .. } => usize::try_from(*len).unwrap_or(usize::MAX),
        Text::Opaque(_) => 0,
    }
}

fn input(snapshot: &Snapshot, head: &Head, cap: usize) -> Result<Vec<Text>, JournalError> {
    let file_len = snapshot.file.metadata()?.len();
    let mut chosen = Vec::new();
    let mut left = cap;
    for text_first in [true, false] {
        for (index, span) in head.header.spans.iter().enumerate() {
            let selected = match span.field {
                Field::User { .. }
                | Field::Steer { .. }
                | Field::Assistant { .. }
                | Field::Plain { .. } => text_first,
                Field::Arguments { .. } | Field::Result { .. } => !text_first,
                Field::Opaque { .. } | Field::CallId | Field::RejectedTool => false,
            };
            if !selected || left == 0 {
                continue;
            }
            let offset = head
                .body_at
                .checked_add(span.rel_offset)
                .ok_or(JournalError::Corrupt)?;
            if offset
                .checked_add(span.len)
                .is_none_or(|end| end > file_len)
            {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            let text = match span.field {
                Field::Arguments { original_bytes, .. } if span.len > left as u64 => {
                    Text::Owned(serde_json::json!({"cut": original_bytes}).to_string())
                }
                Field::Arguments { .. }
                | Field::User { .. }
                | Field::Steer { .. }
                | Field::Assistant { .. }
                | Field::Plain { .. }
                | Field::Result { .. } => Text::File {
                    file: Arc::clone(&snapshot.file),
                    offset,
                    len: span.len,
                }
                .prefix(left as u64)?,
                Field::Opaque { .. } | Field::CallId | Field::RejectedTool => continue,
            };
            let bytes = text_bytes(&text);
            if bytes <= left {
                left -= bytes;
                chosen.push((index, text));
            }
        }
    }
    chosen.sort_by_key(|(index, _text)| *index);
    Ok(chosen.into_iter().map(|(_index, text)| text).collect())
}

fn slot_line(task: &SlotTask, slot: Slot) -> SlotLine {
    SlotLine {
        at_ms: super::journal::millis(SystemTime::now()),
        record: task.record,
        part: task.part,
        slot,
        subject: Subject::Record,
    }
}

fn error_slot(kind: SummaryFailure, detail: &str) -> Slot {
    Slot::Error(SummaryError {
        kind,
        detail: detail[..detail.floor_char_boundary(detail.len().min(256))].to_owned(),
    })
}

fn write_slot(
    journal: &Journal,
    task: &SlotTask,
    slot: Slot,
    turn: Option<TurnId>,
) -> Result<bool, JournalError> {
    let line = slot_line(task, slot);
    match turn {
        Some(turn) => journal.write_slot_for(&task.stem, line, turn),
        None => journal.write_slot(&task.stem, line),
    }
}

fn fail(
    resources: &Resources,
    task: &SlotTask,
    turn: Option<TurnId>,
    kind: SummaryFailure,
    detail: &str,
    next: Next,
) -> Next {
    match write_slot(&resources.journal, task, error_slot(kind, detail), turn) {
        Ok(false) => evicted(resources),
        Ok(true) => next,
        Err(_error) => {
            tracing::warn!(
                event = "gateway_memory_summary_write_failed",
                memory.record.id = task.record.0.get(),
                memory.summary.part = task.part.get(),
                "could not persist summary failure"
            );
            next
        }
    }
}

fn io_failure(error: JournalError) -> SummaryFailure {
    match error {
        JournalError::Io { kind } => SummaryFailure::Io { kind },
        JournalError::Corrupt => SummaryFailure::Io {
            kind: std::io::ErrorKind::InvalidData,
        },
    }
}

fn mark_later(
    journal: &Journal,
    task: &SlotTask,
    turn: Option<TurnId>,
    kind: SummaryFailure,
    detail: &str,
) -> Result<u64, JournalError> {
    let Some(snapshot) = journal.record_lookup(&task.stem, task.record)? else {
        return Ok(0);
    };
    let Some(head) = snapshot.records.iter().find(|head| {
        head.header.id == task.record && turn.is_none_or(|turn| head.header.turn == turn)
    }) else {
        return Ok(0);
    };
    let mut count = 0;
    for part in usize::from(task.part.get() - 1)..head.header.summaries.len() {
        let part =
            NonZeroU8::new(u8::try_from(part + 1).map_err(|_invalid| JournalError::Corrupt)?)
                .ok_or(JournalError::Corrupt)?;
        if write_slot(
            journal,
            &SlotTask {
                part,
                ..task.clone()
            },
            error_slot(kind, detail),
            Some(head.header.turn),
        )? {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests;
