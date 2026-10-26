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
                None,
                io_failure(error),
                "could not read summary input",
            );
            return Next::Continue;
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
    if !matches!(write_slot(journal, task, Slot::Running, expected), Ok(true)) {
        return Next::Stop;
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
        Err(_error) => {
            let _written = mark_later(
                journal,
                task,
                expected,
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
                expected,
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
                if text.is_empty() {
                    error_slot(SummaryFailure::Empty, "summary returned no retained text")
                } else {
                    Slot::Generated(text)
                }
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
                expected,
                SummaryFailure::Refused,
                "summary provider refused",
            );
            return Next::Stop;
        }
        Err(error) => error_slot(SummaryFailure::ModelFailed, &error.to_string()),
    };
    match write_slot(journal, task, slot, expected) {
        Ok(true) => Next::Continue,
        Ok(false) => evicted(resources),
        Err(error) => {
            write_error(
                journal,
                task,
                expected,
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

fn write_error(
    journal: &Journal,
    task: &SlotTask,
    turn: Option<TurnId>,
    kind: SummaryFailure,
    detail: &str,
) {
    let _written = write_slot(journal, task, error_slot(kind, detail), turn);
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
    let Some(snapshot) = journal.record_snapshot(&task.stem, false)? else {
        return Ok(0);
    };
    let Some(head) = snapshot.records.iter().find(|head| {
        head.header.id == task.record && turn.is_none_or(|turn| head.header.turn == turn)
    }) else {
        return Ok(0);
    };
    let mut count = 0;
    for part in usize::from(task.part.get() - 1)..head.header.summaries.len() {
        let part = NonZeroU8::new(u8::try_from(part + 1).map_err(|_| JournalError::Corrupt)?)
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
mod tests {
    use super::super::journal::records::tests::{fixture, journal};
    use super::*;
    use crate::{
        config::ModelConfig,
        routes::RoutingTable,
        session::{ModelFactory, SessionError, SharedModel},
    };
    use dekopon_model::model::{AssistantTurn, ChatModel, ModelTool};
    use parking_lot::Mutex;
    use std::{collections::VecDeque, sync::atomic::AtomicUsize, time::Duration};

    async fn run(resources: Arc<Resources>, queued: Queued, cancel: watch::Receiver<bool>) -> Next {
        super::run(resources, queued, cancel, Arc::default()).await
    }

    const STEM: &str = "reviewer/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    enum Action {
        Text(&'static str),
        Timeout,
        Cancelled,
        Refused,
        Panic,
        Hold(Arc<Gate>),
    }
    struct Script {
        actions: Mutex<VecDeque<Action>>,
        calls: AtomicUsize,
        inputs: Mutex<Vec<String>>,
    }
    struct Gate {
        started: tokio::sync::Notify,
        released: Mutex<bool>,
        wake: parking_lot::Condvar,
    }
    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                started: tokio::sync::Notify::new(),
                released: Mutex::new(false),
                wake: parking_lot::Condvar::new(),
            })
        }
        fn release(&self) {
            *self.released.lock() = true;
            self.wake.notify_all();
        }
    }
    impl ChatModel for Script {
        fn complete(
            &self,
            messages: &[ModelMessage],
            _tools: &[ModelTool],
            _options: &CompletionOptions,
            _on_event: &mut (dyn FnMut(dekopon_model::TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.inputs.lock().push(
                messages
                    .iter()
                    .filter_map(ModelMessage::content)
                    .collect::<String>(),
            );
            let action = self
                .actions
                .lock()
                .pop_front()
                .expect("one action per expected call");
            let text = match action {
                Action::Text(text) => text,
                Action::Timeout => return Err(InferenceError::DeadlineExceeded),
                Action::Cancelled => return Err(InferenceError::Cancelled),
                Action::Refused => {
                    return Err(InferenceError::Provider(
                        dekopon_model::error::ProviderFailure::new(403, "refused"),
                    ));
                }
                Action::Panic => panic!("fake summary panic"),
                Action::Hold(gate) => {
                    gate.started.notify_one();
                    let mut released = gate.released.lock();
                    while !*released {
                        if gate
                            .wake
                            .wait_for(&mut released, Duration::from_secs(10))
                            .timed_out()
                        {
                            panic!("test did not release its held model");
                        }
                    }
                    "held"
                }
            };
            Ok(AssistantTurn::new(Some(text.to_owned()), Vec::new(), None))
        }
    }
    struct Factory(Arc<Script>);
    impl ModelFactory for Factory {
        fn build_bound(
            &self,
            _model: &Arc<ModelConfig>,
            _runtime: tokio::runtime::Handle,
            _cancel: watch::Receiver<bool>,
        ) -> Result<SharedModel, SessionError> {
            Ok(self.0.clone())
        }
    }
    fn resources(
        journal: Arc<Journal>,
        actions: impl IntoIterator<Item = Action>,
        configured: bool,
    ) -> (Resources, Arc<Script>) {
        let script = Arc::new(Script {
            actions: Mutex::new(actions.into_iter().collect()),
            calls: AtomicUsize::new(0),
            inputs: Mutex::new(Vec::new()),
        });
        let mut routes = RoutingTable::default();
        if configured {
            routes.models.insert("summary".to_owned(), Arc::new(serde_json::from_value(serde_json::json!({"kind":"openaiCompatible","name":"summary","endpoint":"http://127.0.0.1:9","model":"fixture","timeoutMs":1000})).unwrap()));
        }
        (
            Resources {
                journal,
                current: CurrentGeneration::boot(Arc::new(routes), Arc::default()),
                models: Arc::new(ModelCache::new(Arc::new(Factory(Arc::clone(&script))))),
                metering: Arc::new(Metering::new(Vec::new(), Metering::system_clock())),
                counts: Arc::default(),
            },
            script,
        )
    }
    fn task() -> SlotTask {
        SlotTask {
            stem: STEM.to_owned(),
            record: fixture().id,
            part: NonZeroU8::new(1).unwrap(),
        }
    }
    fn slots(journal: &Journal) -> Vec<Slot> {
        journal
            .record_snapshot(STEM, true)
            .unwrap()
            .unwrap()
            .records
            .remove(0)
            .header
            .summaries
    }
    async fn wait_for(journal: &Journal, predicate: impl Fn(&[Slot]) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if predicate(&slots(journal)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("summary state reached");
    }
    fn failed(slot: &Slot, kind: SummaryFailure) -> bool {
        matches!(slot, Slot::Error(error) if error.kind == kind)
    }

    #[tokio::test]
    async fn summary_errors_map_to_slot_failures() {
        let (_dir, journal) = journal();
        journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
        let (resources, script) = resources(
            Arc::clone(&journal),
            [Action::Timeout, Action::Text(""), Action::Cancelled],
            true,
        );
        let resources = Arc::new(resources);
        let cancel = watch::channel(false).1;
        assert!(matches!(
            run(
                Arc::clone(&resources),
                Queued {
                    task: task(),
                    since: Instant::now()
                },
                cancel.clone()
            )
            .await,
            Next::Continue
        ));
        let mut task = task();
        task.part = NonZeroU8::new(2).unwrap();
        assert!(matches!(
            run(
                Arc::clone(&resources),
                Queued {
                    task: task.clone(),
                    since: Instant::now()
                },
                cancel.clone()
            )
            .await,
            Next::Continue
        ));
        task.part = NonZeroU8::new(3).unwrap();
        assert!(matches!(
            run(
                resources,
                Queued {
                    task,
                    since: Instant::now()
                },
                cancel
            )
            .await,
            Next::Stop
        ));
        let slots = slots(&journal);
        assert!(failed(&slots[0], SummaryFailure::Timeout));
        assert!(failed(&slots[1], SummaryFailure::Empty));
        assert_eq!(slots[2], Slot::Running);
        assert_eq!(script.calls.load(Ordering::Relaxed), 3);
        let boot = journal.load_summary_tasks().unwrap();
        assert_eq!(boot.requeued, 1);
        assert_eq!(boot.tasks.len(), 1);
        assert_eq!(boot.tasks[0].part.get(), 3);
    }

    #[tokio::test]
    async fn an_unconfigured_plan_model_stops_its_chain() {
        let (_dir, journal) = journal();
        journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
        let (resources, script) = resources(Arc::clone(&journal), [], false);
        assert!(matches!(
            run(
                Arc::new(resources),
                Queued {
                    task: task(),
                    since: Instant::now()
                },
                watch::channel(false).1
            )
            .await,
            Next::Stop
        ));
        assert!(
            slots(&journal)
                .iter()
                .all(|slot| failed(slot, SummaryFailure::Unconfigured))
        );
        assert_eq!(script.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_refused_part_does_not_reopen_its_later_slots() {
        let (_dir, journal) = journal();
        journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
        let (resources, script) = resources(Arc::clone(&journal), [Action::Refused], true);
        let mut owned = JoinSet::new();
        let sender = spawn(
            resources,
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &mut owned,
        );
        sender.enqueue(task()).await;
        wait_for(&journal, |slots| {
            slots
                .iter()
                .all(|slot| failed(slot, SummaryFailure::Refused))
        })
        .await;
        assert_eq!(script.calls.load(Ordering::Relaxed), 1);
        owned.shutdown().await;
    }

    #[tokio::test]
    async fn a_panicked_summary_marks_its_later_parts() {
        let (_dir, journal) = journal();
        let mut first = fixture();
        first.summaries = vec![Slot::Queued; 2];
        journal.append_record(STEM, &first, Vec::new()).unwrap();
        let mut second = fixture();
        second.id = RecordId(2.try_into().unwrap());
        second.summaries = vec![Slot::Queued];
        journal.append_record(STEM, &second, Vec::new()).unwrap();
        let (resources, script) = resources(
            Arc::clone(&journal),
            [Action::Panic, Action::Text("next")],
            true,
        );
        let mut owned = JoinSet::new();
        let sender = spawn(
            resources,
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &mut owned,
        );
        sender.enqueue(task()).await;
        sender
            .enqueue(SlotTask {
                record: second.id,
                ..task()
            })
            .await;
        wait_for(&journal, |slots| {
            slots
                .iter()
                .all(|slot| failed(slot, SummaryFailure::Panicked))
        })
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
                if matches!(snapshot.records[1].header.summaries[0], Slot::Generated(_)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(script.calls.load(Ordering::Relaxed), 2);
        owned.shutdown().await;
    }

    #[tokio::test]
    async fn an_overflowed_part_marks_its_later_parts() {
        let (_dir, journal) = journal();
        journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let sender = Sender {
            queue: tx,
            journal: Arc::clone(&journal),
            counts: Arc::default(),
        };
        sender.enqueue(task()).await;
        sender.enqueue(task()).await;
        assert!(
            slots(&journal)
                .iter()
                .all(|slot| failed(slot, SummaryFailure::Overflow))
        );
        assert_eq!(sender.counts.overflowed.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn a_summary_model_serving_no_route_resolves() {
        let (_dir, journal) = journal();
        journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
        let (resources, script) = resources(
            Arc::clone(&journal),
            [
                Action::Text("one"),
                Action::Text("two"),
                Action::Text("🦀abcdef"),
            ],
            true,
        );
        let mut owned = JoinSet::new();
        let sender = spawn(
            resources,
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            &mut owned,
        );
        sender.enqueue(task()).await;
        wait_for(&journal, |slots| {
            slots.iter().all(|slot| matches!(slot, Slot::Generated(_)))
        })
        .await;
        assert_eq!(
            slots(&journal),
            vec![
                Slot::Generated("one".to_owned()),
                Slot::Generated("two".to_owned()),
                Slot::Generated("🦀abcd".to_owned())
            ]
        );
        assert_eq!(script.calls.load(Ordering::Relaxed), 3);
        assert!(script.inputs.lock()[2].contains("Earlier parts:\nonetwo"));
        owned.shutdown().await;
    }

    #[tokio::test]
    async fn a_finished_summary_is_joined_at_once_at_concurrency_two() {
        let (_dir, journal) = journal();
        let mut first = fixture();
        first.summaries = vec![Slot::Queued];
        journal.append_record(STEM, &first, Vec::new()).unwrap();
        let mut second = fixture();
        second.id = RecordId(2.try_into().unwrap());
        second.summaries = vec![Slot::Queued; 2];
        journal.append_record(STEM, &second, Vec::new()).unwrap();
        let gate = Gate::new();
        let (resources, script) = resources(
            Arc::clone(&journal),
            [
                Action::Hold(Arc::clone(&gate)),
                Action::Text("part1"),
                Action::Text("part2"),
            ],
            true,
        );
        let mut owned = JoinSet::new();
        let sender = spawn(
            resources,
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(2).unwrap(),
            &mut owned,
        );
        sender.enqueue(task()).await;
        gate.started.notified().await;
        sender
            .enqueue(SlotTask {
                record: second.id,
                ..task()
            })
            .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
                if matches!(snapshot.records[1].header.summaries[1], Slot::Generated(_)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(slots(&journal), vec![Slot::Running]);
        assert_eq!(script.calls.load(Ordering::Relaxed), 3);
        gate.release();
        wait_for(&journal, |slots| matches!(slots[0], Slot::Generated(_))).await;
        owned.shutdown().await;
    }

    #[test]
    fn boot_enqueues_only_the_lowest_queued_part() {
        let (_dir, journal) = journal();
        let mut record = fixture();
        record.summaries = vec![
            Slot::Generated("done".to_owned()),
            Slot::Running,
            Slot::Queued,
        ];
        journal.append_record(STEM, &record, Vec::new()).unwrap();
        journal
            .write_slot(
                STEM,
                SlotLine {
                    at_ms: 2,
                    record: record.id,
                    part: NonZeroU8::new(1).unwrap(),
                    slot: Slot::Running,
                    subject: Subject::Asset(1.try_into().unwrap()),
                },
            )
            .unwrap();
        let boot = journal.load_summary_tasks().unwrap();
        assert_eq!(boot.requeued, 1);
        assert_eq!(boot.tasks.len(), 1);
        assert_eq!(boot.tasks[0].part.get(), 2);
        assert_eq!(
            slots(&journal),
            vec![
                Slot::Generated("done".to_owned()),
                Slot::Queued,
                Slot::Queued
            ]
        );
        assert_eq!(
            journal
                .record_snapshot(STEM, true)
                .unwrap()
                .unwrap()
                .records[0]
                .asset_lines
                .len(),
            1
        );
    }

    #[test]
    fn a_file_that_crashed_the_last_boot_is_reset_first() {
        let (dir, journal) = journal();
        journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
        std::fs::write(dir.path().join("loading.marker"), STEM).unwrap();
        let boot = journal.load_summary_tasks().unwrap();
        assert_eq!(boot.reset, 1);
        assert_eq!(boot.crashed_files, 1);
        assert!(boot.tasks.is_empty());
        assert!(!dir.path().join(format!("{STEM}.jsonl")).exists());
        assert!(!dir.path().join("loading.marker").exists());
    }
}
