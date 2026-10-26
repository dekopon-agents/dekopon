use std::{
    collections::HashMap,
    num::{NonZeroU8, NonZeroUsize},
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime},
};

use dekopon_core::AgentId;
use dekopon_model::{
    error::InferenceError,
    model::{CompletionOptions, ModelMessage},
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
        records::{Field, Head, Snapshot, escaped_text},
    },
    record::{RecordId, Slot, SlotLine, Subject, SummaryError, SummaryFailure},
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
    pub metering: Arc<Metering>,
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
    let mut tasks: HashMap<Id, SlotTask> = HashMap::new();
    loop {
        tokio::select! {
            result = running.join_next_with_id(), if !running.is_empty() => {
                match result {
                    Some(Ok((id, next))) => {
                        if let Some(task) = tasks.remove(&id) {
                            match next {
                                Next::Continue => if let Some(part) = task.part.get().checked_add(1).and_then(NonZeroU8::new) { sender.enqueue(SlotTask { part, ..task }).await; },
                                Next::Stop => {}
                            }
                        }
                    }
                    Some(Err(error)) => {
                        if let Some(task) = tasks.remove(&error.id()).filter(|_| error.is_panic()) {
                            let journal = Arc::clone(&resources.journal);
                            let _written = tokio::task::spawn_blocking(move || mark_later(&journal, &task, SummaryFailure::Panicked, "summary task panicked")).await;
                        }
                    }
                    None => {}
                }
            }
            queued = receiver.recv(), if running.len() < concurrency.get() => {
                let Some(queued) = queued else { break; };
                let task = queued.task.clone();
                let handle = running.spawn(run(Arc::clone(&resources), queued, cancel.0.subscribe()));
                tasks.insert(handle.id(), task);
            }
        }
    }
}

async fn run(resources: Arc<Resources>, queued: Queued, cancel: watch::Receiver<bool>) -> Next {
    let span = tracing::info_span!(
        "memory.summarize",
        memory.record.id = queued.task.record.0.get(),
        memory.summary.part = queued.task.part.get(),
        memory.summary.queue_wait_ms = queued.since.elapsed().as_millis() as u64
    );
    let runtime = tokio::runtime::Handle::current();
    let task = queued.task;
    match tokio::task::spawn_blocking(move || summarize(&resources, &task, runtime, cancel))
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
) -> Next {
    let journal = &resources.journal;
    let loaded = journal.record_snapshot(&task.stem, false);
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
            write_error(
                journal,
                task,
                io_failure(error),
                "could not read summary input",
            );
            return Next::Continue;
        }
    };
    let head = &snapshot.records[position];
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
    if !matches!(
        journal.write_slot(&task.stem, slot_line(task, Slot::Running)),
        Ok(true)
    ) {
        return Next::Stop;
    }
    let current = resources.current.load();
    let Some(model) = current.routes.models.get(&plan.model) else {
        let _written = mark_later(
            journal,
            task,
            SummaryFailure::Unconfigured,
            "summary model is no longer configured",
        );
        return Next::Stop;
    };
    let client = match resources.models.client_bound(model, runtime, cancel) {
        Ok(client) => client,
        Err(_error) => {
            let _written = mark_later(
                journal,
                task,
                SummaryFailure::Unconfigured,
                "summary model could not be configured",
            );
            return Next::Stop;
        }
    };
    let input = match input(&snapshot, head, plan.max_input_bytes.get() as usize) {
        Ok(input) => input,
        Err(error) => {
            write_error(
                journal,
                task,
                io_failure(error),
                "could not read summary input",
            );
            return Next::Continue;
        }
    };
    let previous: String = head.header.summaries[..part]
        .iter()
        .filter_map(|slot| match slot {
            Slot::Generated(text) => Some(text.as_str()),
            Slot::Queued | Slot::Running | Slot::Error(_) => None,
        })
        .collect();
    let instructions = format!(
        "Write only new additive summary information for part {}. Do not repeat earlier parts.",
        task.part
    );
    let messages = [
        ModelMessage::system(instructions),
        ModelMessage::user(format!("Source:\n{input}\nEarlier parts:\n{previous}")),
    ];
    let estimate = Estimate::from_sizes(
        Sizes {
            bytes: messages
                .iter()
                .filter_map(ModelMessage::content)
                .map(str::len)
                .sum(),
            images: 0,
        },
        None,
        Tokens::from_bytes(limit.get() as usize),
    );
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
    let result = client.complete(
        &messages,
        &[],
        &CompletionOptions::default(),
        &mut |_event| ControlFlow::Continue(()),
    );
    let (usage, bytes, outcome, kind) = match &result {
        Ok(turn) => (
            turn.usage.clone(),
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
    resources
        .metering
        .record_unadmitted(&call, estimate, usage, bytes, outcome, kind);
    let slot = match result {
        Ok(turn) => match turn
            .content
            .filter(|text| !text.trim().is_empty() && turn.tool_calls.is_empty())
        {
            Some(mut text) => {
                text.truncate(text.floor_char_boundary(limit.get() as usize));
                Slot::Generated(text)
            }
            None => error_slot(SummaryFailure::Empty, "summary returned no text"),
        },
        Err(InferenceError::Cancelled) => return Next::Stop,
        Err(InferenceError::DeadlineExceeded) => {
            error_slot(SummaryFailure::Timeout, "summary model deadline exceeded")
        }
        Err(InferenceError::Provider(error))
            if matches!(
                error.code.as_deref(),
                Some("refused" | "content_filter" | "safety")
            ) || error.status == Some(403) =>
        {
            let _written = mark_later(
                journal,
                task,
                SummaryFailure::Refused,
                "summary provider refused",
            );
            return Next::Stop;
        }
        Err(error) => error_slot(SummaryFailure::ModelFailed, &error.to_string()),
    };
    match journal.write_slot(&task.stem, slot_line(task, slot)) {
        Ok(true) => Next::Continue,
        Ok(false) => evicted(resources),
        Err(error) => {
            write_error(
                journal,
                task,
                io_failure(error),
                "could not persist summary result",
            );
            Next::Continue
        }
    }
}

fn evicted(resources: &Resources) -> Next {
    resources.counts.evicted.fetch_add(1, Ordering::Relaxed);
    Next::Stop
}

fn input(snapshot: &Snapshot, head: &Head, cap: usize) -> Result<String, JournalError> {
    let mut input = String::new();
    for text_first in [true, false] {
        for span in &head.header.spans {
            let selected = match span.field {
                Field::User { .. }
                | Field::Steer { .. }
                | Field::Assistant { .. }
                | Field::Plain { .. } => text_first,
                Field::Arguments { .. } | Field::Result { .. } => !text_first,
                Field::Opaque { .. } | Field::CallId | Field::RejectedTool => false,
            };
            if selected && input.len() < cap {
                let text = escaped_text(
                    &snapshot.file,
                    head.body_at + span.rel_offset,
                    span.len,
                    cap - input.len(),
                )?;
                input.push_str(&text);
            }
        }
    }
    Ok(input)
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

fn write_error(journal: &Journal, task: &SlotTask, kind: SummaryFailure, detail: &str) {
    let _written = journal.write_slot(&task.stem, slot_line(task, error_slot(kind, detail)));
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
    kind: SummaryFailure,
    detail: &str,
) -> Result<u64, JournalError> {
    let Some(snapshot) = journal.record_snapshot(&task.stem, false)? else {
        return Ok(0);
    };
    let Some(head) = snapshot
        .records
        .iter()
        .find(|head| head.header.id == task.record)
    else {
        return Ok(0);
    };
    let mut count = 0;
    for part in usize::from(task.part.get() - 1)..head.header.summaries.len() {
        let part = NonZeroU8::new(u8::try_from(part + 1).map_err(|_| JournalError::Corrupt)?)
            .ok_or(JournalError::Corrupt)?;
        if journal.write_slot(
            &task.stem,
            slot_line(
                &SlotTask {
                    part,
                    ..task.clone()
                },
                error_slot(kind, detail),
            ),
        )? {
            count += 1;
        }
    }
    Ok(count)
}
