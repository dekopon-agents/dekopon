use super::super::journal::records::tests::{fixture, journal};
use super::*;
use crate::{
    config::ModelConfig,
    routes::RoutingTable,
    session::{ModelFactory, SessionError, SharedModel},
};
use dekopon_agent::memory_tools::Rung;
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
    Failed,
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
        let mut captured = String::new();
        for message in messages {
            if let ModelMessage::Journal {
                message: ReplayMessage::User(texts),
            } = message
            {
                for text in texts {
                    match text {
                        Text::Owned(text) => captured.push_str(text),
                        Text::File { file, offset, len } => captured.push_str(
                            &super::super::journal::records::escaped_text(
                                file,
                                *offset,
                                *len,
                                256 * 1024,
                            )
                            .unwrap(),
                        ),
                        Text::Opaque(_) => panic!("opaque source in summary input"),
                    }
                }
            } else if let Some(text) = message.content() {
                captured.push_str(text);
            }
        }
        self.inputs.lock().push(captured);
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
            Action::Failed => {
                return Err(InferenceError::Provider(
                    dekopon_model::error::ProviderFailure::new(500, "fake summary error"),
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
        Ok(Arc::<Script>::clone(&self.0))
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
        Next::Continue(_)
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
        Next::Continue(_)
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
    let boot = journal.load_summary_tasks(64.try_into().unwrap()).unwrap();
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
    first.plan.as_mut().unwrap().parts.truncate(2);
    journal.append_record(STEM, &first, Vec::new()).unwrap();
    let mut second = fixture();
    second.id = RecordId(2.try_into().unwrap());
    second.summaries = vec![Slot::Queued];
    second.plan.as_mut().unwrap().parts.truncate(1);
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
    first.plan.as_mut().unwrap().parts.truncate(1);
    journal.append_record(STEM, &first, Vec::new()).unwrap();
    let mut second = fixture();
    second.id = RecordId(2.try_into().unwrap());
    second.summaries = vec![Slot::Queued; 2];
    second.plan.as_mut().unwrap().parts.truncate(2);
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
    let boot = journal.load_summary_tasks(64.try_into().unwrap()).unwrap();
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
    let boot = journal.load_summary_tasks(64.try_into().unwrap()).unwrap();
    assert_eq!(boot.reset, 1);
    assert_eq!(boot.crashed_files, 1);
    assert!(boot.tasks.is_empty());
    assert!(!dir.path().join(format!("{STEM}.jsonl")).exists());
    assert!(!dir.path().join("loading.marker").exists());
}

#[tokio::test]
async fn eviction_during_a_run_discards_the_result_and_leaves_no_orphan() {
    let (_dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let gate = Gate::new();
    let (resources, _script) = resources(
        Arc::clone(&journal),
        [Action::Hold(Arc::clone(&gate))],
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
    tokio::time::timeout(Duration::from_secs(3), gate.started.notified())
        .await
        .unwrap();
    journal.rewrite_records(STEM, |_| false).unwrap();
    let mut replacement = fixture();
    replacement.turn = TurnId(2);
    replacement.original = vec![dekopon_agent::memory::Item::User(
        dekopon_agent::memory::Kept::new("replacement", 1024),
    )];
    journal
        .append_record(STEM, &replacement, Vec::new())
        .unwrap();
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while sender.counts.evicted.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(slots(&journal), vec![Slot::Queued; 3]);
    let current = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(current.records.len(), 1);
    assert_eq!(current.records[0].header.turn, TurnId(2));
    owned.shutdown().await;
}

#[tokio::test]
async fn a_summary_cut_before_its_first_codepoint_is_empty() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.plan.as_mut().unwrap().parts[0] = 1.try_into().unwrap();
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let (resources, script) = resources(Arc::clone(&journal), [Action::Text("🦀")], true);
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
        Next::Continue(_)
    ));
    assert!(failed(&slots(&journal)[0], SummaryFailure::Empty));
    assert_eq!(script.calls.load(Ordering::Relaxed), 1);
}

struct FaultingFactory {
    path: std::path::PathBuf,
    script: Arc<Script>,
    faulted: std::sync::atomic::AtomicBool,
}
impl ModelFactory for FaultingFactory {
    fn build_bound(
        &self,
        _model: &Arc<ModelConfig>,
        _runtime: tokio::runtime::Handle,
        _cancel: watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        if !self.faulted.swap(true, Ordering::Relaxed) {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let original = std::fs::read(&self.path).unwrap();
            let temporary = self.path.with_extension("fault-test");
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .unwrap();
            out.write_all(&original).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&self.path)
                .unwrap()
                .set_len(0)
                .unwrap();
            std::fs::rename(temporary, &self.path).unwrap();
        }
        Ok(Arc::<Script>::clone(&self.script))
    }
}

#[tokio::test]
async fn an_io_error_is_typed() {
    let (dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let (mut resources, script) = resources(
        Arc::clone(&journal),
        [Action::Text("second"), Action::Text("third")],
        true,
    );
    resources.models = Arc::new(ModelCache::new(Arc::new(FaultingFactory {
        path: dir.path().join(format!("{STEM}.jsonl")),
        script: Arc::clone(&script),
        faulted: std::sync::atomic::AtomicBool::new(false),
    })));
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        NonZeroUsize::new(4).unwrap(),
        NonZeroUsize::new(1).unwrap(),
        &mut owned,
    );
    sender.enqueue(task()).await;
    wait_for(&journal, |slots| matches!(&slots[2], Slot::Generated(_))).await;
    let slots = slots(&journal);
    assert!(failed(
        &slots[0],
        SummaryFailure::Io {
            kind: std::io::ErrorKind::UnexpectedEof
        }
    ));
    assert_eq!(slots[1], Slot::Generated("second".to_owned()));
    assert_eq!(slots[2], Slot::Generated("third".to_owned()));
    assert_eq!(script.calls.load(Ordering::Relaxed), 2);
    owned.shutdown().await;
}

async fn held_listener() -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Receiver<()>,
    JoinSet<()>,
) {
    use tokio::io::AsyncReadExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (seen, requested) = tokio::sync::oneshot::channel();
    let (closed, disconnected) = tokio::sync::oneshot::channel();
    let mut servers = JoinSet::new();
    servers.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 64 * 1024];
        assert!(stream.read(&mut buffer).await.unwrap() > 0);
        seen.send(()).unwrap();
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("held connection failed: {error}"),
            }
        }
        closed.send(()).unwrap();
    });
    (endpoint, requested, disconnected, servers)
}

fn live_resources(journal: Arc<Journal>, endpoint: &str, timeout_ms: u64) -> Resources {
    let mut routes = RoutingTable::default();
    routes.models.insert("summary".to_owned(), Arc::new(serde_json::from_value(serde_json::json!({"kind":"openaiCompatible","name":"summary","endpoint":endpoint,"model":"fixture","timeoutMs":timeout_ms})).unwrap()));
    Resources {
        journal,
        current: CurrentGeneration::boot(Arc::new(routes), Arc::default()),
        models: Arc::new(ModelCache::new(Arc::new(
            crate::session::ConfiguredModels::default(),
        ))),
        counts: Arc::default(),
    }
}

#[tokio::test]
async fn a_model_deadline_becomes_timeout() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.plan.as_mut().unwrap().parts.truncate(1);
    record.summaries.truncate(1);
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let (endpoint, seen, closed, mut server) = held_listener().await;
    let mut owned = JoinSet::new();
    let sender = spawn(
        live_resources(Arc::clone(&journal), &endpoint, 200),
        NonZeroUsize::new(4).unwrap(),
        NonZeroUsize::new(1).unwrap(),
        &mut owned,
    );
    sender.enqueue(task()).await;
    tokio::time::timeout(Duration::from_secs(3), seen)
        .await
        .unwrap()
        .unwrap();
    wait_for(&journal, |slots| failed(&slots[0], SummaryFailure::Timeout)).await;
    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .unwrap()
        .unwrap();
    server.join_next().await.unwrap().unwrap();
    owned.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_a_running_summary_call() {
    let (dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let (endpoint, seen, closed, mut server) = held_listener().await;
    let mut owned = JoinSet::new();
    let sender = spawn(
        live_resources(Arc::clone(&journal), &endpoint, 60_000),
        NonZeroUsize::new(4).unwrap(),
        NonZeroUsize::new(1).unwrap(),
        &mut owned,
    );
    sender.enqueue(task()).await;
    tokio::time::timeout(Duration::from_secs(3), seen)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(slots(&journal)[0], Slot::Running);
    tokio::time::timeout(Duration::from_secs(2), owned.shutdown())
        .await
        .expect("the dispatcher adds no model timeout to shutdown");
    tokio::time::timeout(Duration::from_secs(2), closed)
        .await
        .unwrap()
        .unwrap();
    server.join_next().await.unwrap().unwrap();
    assert_eq!(slots(&journal)[0], Slot::Running);
    drop(sender);
    drop(journal);
    let (reopened, boot) = Journal::open_records(
        dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
    )
    .unwrap();
    assert_eq!(boot.requeued, 1);
    assert_eq!(boot.tasks.len(), 1);
    assert_eq!(slots(&reopened)[0], Slot::Queued);
}

#[tokio::test]
async fn a_restart_mid_summary_requeues_and_resumes_from_the_original() {
    use dekopon_agent::memory_tools::Rung;
    let (dir, journal) = journal();
    let mut record = fixture();
    record.summaries[0] = Slot::Running;
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    journal
        .write_pin(
            STEM,
            super::super::record::PinLine {
                at_ms: 2,
                record: record.id,
                pin: Some(Rung::Native),
                subject: Subject::Record,
            },
        )
        .unwrap();
    journal
        .write_choice(
            STEM,
            super::super::record::ChoiceLine {
                at_ms: 3,
                record: record.id,
                choice: Rung::Metadata,
            },
        )
        .unwrap();
    drop(journal);
    let (journal, boot) = Journal::open_records(
        dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
    )
    .unwrap();
    assert_eq!(boot.requeued, 1);
    assert_eq!(boot.tasks.len(), 1);
    let journal = Arc::new(journal);
    journal.rewrite_records(STEM, |_| true).unwrap();
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(
        snapshot.records[0].header.plan.as_ref().unwrap().model,
        "summary"
    );
    assert_eq!(snapshot.records[0].header.pin, Some(Rung::Native));
    assert_eq!(snapshot.records[0].header.choice, Some(Rung::Metadata));
    let (resources, script) = resources(
        Arc::clone(&journal),
        [
            Action::Text("one"),
            Action::Text("two"),
            Action::Text("three"),
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
    for task in boot.tasks {
        sender.enqueue(task).await;
    }
    wait_for(&journal, |slots| {
        slots.iter().all(|slot| matches!(slot, Slot::Generated(_)))
    })
    .await;
    assert_eq!(script.calls.load(Ordering::Relaxed), 3);
    assert!(script.inputs.lock()[0].contains("a\n🦀\\\"z"));
    owned.shutdown().await;
}

#[test]
fn summary_input_keeps_originals_as_bounded_held_file_slices() {
    use dekopon_agent::memory::{Item, Kept};
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.original = vec![Item::User(Kept::new(
        &"source-canary-é\n".repeat(100_000),
        usize::MAX,
    ))];
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let snapshot = journal.record_snapshot(STEM, false).unwrap().unwrap();
    let input = super::input(&snapshot, &snapshot.records[0], 128 * 1024).unwrap();
    assert_eq!(input.len(), 1);
    let Text::File { file, len, .. } = &input[0] else {
        panic!("original stays in journal");
    };
    assert!(Arc::ptr_eq(file, &snapshot.file));
    assert!(*len > 0 && *len <= 128 * 1024);
}

#[test]
fn runtime_overflow_preserves_terminal_slots_and_counts_only_changed_slots() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    let old_error = error_slot(SummaryFailure::Timeout, "old failure");
    record
        .plan
        .as_mut()
        .unwrap()
        .parts
        .push(8.try_into().unwrap());
    record.summaries = vec![
        Slot::Running,
        Slot::Generated("kept".to_owned()),
        Slot::Queued,
        old_error.clone(),
    ];
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    assert_eq!(
        mark_later(
            &journal,
            &task(),
            Some(record.turn),
            SummaryFailure::Overflow,
            "overflow"
        )
        .unwrap(),
        2
    );
    let after = slots(&journal);
    assert!(failed(&after[0], SummaryFailure::Overflow));
    assert_eq!(after[1], Slot::Generated("kept".to_owned()));
    assert!(failed(&after[2], SummaryFailure::Overflow));
    assert_eq!(after[3], old_error);
    assert_eq!(
        mark_later(
            &journal,
            &task(),
            Some(record.turn),
            SummaryFailure::Overflow,
            "overflow"
        )
        .unwrap(),
        0
    );
    assert_eq!(slots(&journal), after);
}

#[tokio::test]
async fn recovery_skips_a_finalized_middle_part_and_resumes_later_queued_work() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.summaries = vec![
        Slot::Running,
        Slot::Generated("kept".to_owned()),
        Slot::Queued,
    ];
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let boot = journal.load_summary_tasks(64.try_into().unwrap()).unwrap();
    let (resources, script) = resources(
        Arc::clone(&journal),
        [Action::Text("first"), Action::Text("third")],
        true,
    );
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        64.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    assert_eq!(boot.tasks.len(), 1);
    for task in boot.tasks {
        sender.enqueue(task).await;
    }
    wait_for(&journal, |slots| matches!(slots[2], Slot::Generated(_))).await;
    assert_eq!(
        slots(&journal),
        vec![
            Slot::Generated("first".to_owned()),
            Slot::Generated("kept".to_owned()),
            Slot::Generated("third".to_owned())
        ]
    );
    assert_eq!(script.calls.load(Ordering::Relaxed), 2);
    assert!(script.inputs.lock()[1].contains("Earlier parts:\nfirstkept"));
    owned.shutdown().await;
}

#[tokio::test]
async fn a_continuation_never_bypasses_a_running_middle_part() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.summaries = vec![Slot::Queued, Slot::Running, Slot::Queued];
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let (resources, script) = resources(Arc::clone(&journal), [Action::Text("first")], true);
    let next = run(
        Arc::new(resources),
        Queued {
            task: task(),
            since: Instant::now(),
        },
        watch::channel(false).1,
    )
    .await;
    assert!(matches!(next, Next::Stop));
    assert_eq!(slots(&journal)[1..], [Slot::Running, Slot::Queued]);
    assert_eq!(script.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn a_transport_content_filter_refusal_stops_the_summary_chain_after_one_call() {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/prefix", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let mut server = JoinSet::new();
    server.spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (path, headers, body) = crate::tests::read_http_request_parts(&mut stream).await.unwrap();
            assert_eq!(path, "/prefix/chat/completions");
            assert!(!headers.to_ascii_lowercase().contains("authorization:"));
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["model"], "fixture");
            assert_eq!(body["stream"], true);
            counted.fetch_add(1, Ordering::Relaxed);
            let body = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\ndata: [DONE]\n\n";
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let (_dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let mut owned = JoinSet::new();
    let sender = spawn(
        live_resources(Arc::clone(&journal), &endpoint, 2_000),
        4.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    sender.enqueue(task()).await;
    wait_for(&journal, |slots| {
        slots.iter().all(|slot| !slot.is_pending())
    })
    .await;
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(
        slots(&journal)
            .iter()
            .all(|slot| failed(slot, SummaryFailure::Refused))
    );
    owned.shutdown().await;
    server.shutdown().await;
}

fn record(id: u64, user: &str, parts: usize) -> super::super::record::TurnRecord {
    let mut record = fixture();
    record.id = RecordId(id.try_into().unwrap());
    record.turn = TurnId(id);
    record.original = vec![dekopon_agent::memory::Item::User(
        dekopon_agent::memory::Kept::new(user, usize::MAX),
    )];
    record.summaries = vec![Slot::Queued; parts];
    record.plan.as_mut().unwrap().parts.truncate(parts);
    if parts == 0 {
        record.plan = None;
    }
    record
}

fn projection(
    journal: &Journal,
    prior: &[super::super::project::Prior],
    max_turns: usize,
    max_bytes: usize,
) -> super::super::project::Projected {
    let snapshot = journal.record_snapshot(STEM, false).unwrap().unwrap();
    super::super::project::project(&super::super::project::ProjectionInput {
        file: &snapshot.file,
        records: &snapshot.records,
        prior,
        hot_turns: 1,
        max_turns,
        max_bytes,
        model: &fixture().model,
    })
    .unwrap()
}

fn projected_text(projected: &super::super::project::Projected) -> String {
    projected
        .messages
        .iter()
        .map(|message| crate::tests::message_text(message).1)
        .collect()
}

async fn run_part(resources: &Arc<Resources>, record: u64, part: u8) -> Next {
    run(
        Arc::clone(resources),
        Queued {
            task: SlotTask {
                record: RecordId(record.try_into().unwrap()),
                part: NonZeroU8::new(part).unwrap(),
                ..task()
            },
            since: Instant::now(),
        },
        watch::channel(false).1,
    )
    .await
}

fn summaries_of(journal: &Journal, record: u64) -> Vec<Slot> {
    journal
        .record_snapshot(STEM, true)
        .unwrap()
        .unwrap()
        .records
        .into_iter()
        .find(|head| head.header.id.0.get() == record)
        .unwrap()
        .header
        .summaries
}

#[tokio::test]
async fn a_failing_or_refusing_summarizer_leaves_errors_not_fakes() {
    let (_dir, journal) = journal();
    journal
        .append_record(
            STEM,
            &record(1, &"refused-original ".repeat(90), 2),
            Vec::new(),
        )
        .unwrap();
    journal
        .append_record(
            STEM,
            &record(2, &"failed-original ".repeat(90), 2),
            Vec::new(),
        )
        .unwrap();
    journal
        .append_record(STEM, &record(3, "hot", 0), Vec::new())
        .unwrap();
    let (resources, script) = resources(
        Arc::clone(&journal),
        [Action::Refused, Action::Failed, Action::Timeout],
        true,
    );
    let resources = Arc::new(resources);
    assert!(matches!(run_part(&resources, 1, 1).await, Next::Stop));
    assert!(matches!(
        run_part(&resources, 2, 1).await,
        Next::Continue(_)
    ));
    assert!(matches!(run_part(&resources, 2, 2).await, Next::Stop));
    assert_eq!(script.calls.load(Ordering::Relaxed), 3);
    assert!(
        summaries_of(&journal, 1)
            .iter()
            .all(|slot| failed(slot, SummaryFailure::Refused))
    );
    let second = summaries_of(&journal, 2);
    assert!(failed(&second[0], SummaryFailure::ModelFailed));
    assert!(failed(&second[1], SummaryFailure::Timeout));
    for (max_bytes, native, metadata) in [(64 * 1024, 3, 0), (2000, 2, 1)] {
        let projected = projection(&journal, &[], 8, max_bytes);
        assert_eq!(
            projected.rungs,
            super::super::project::RungCounts {
                native,
                summary: 0,
                metadata
            }
        );
        let text = projected_text(&projected);
        assert!(!text.contains("fake summary error"), "{text}");
        assert!(!text.contains("summary 1/"), "{text}");
    }
}

#[tokio::test]
async fn a_late_summary_waits_for_the_next_repack() {
    let (_dir, journal) = journal();
    journal
        .append_record(STEM, &record(1, &"older ".repeat(700), 1), Vec::new())
        .unwrap();
    journal
        .append_record(STEM, &record(2, "middle", 0), Vec::new())
        .unwrap();
    journal
        .append_record(STEM, &record(3, "newest", 0), Vec::new())
        .unwrap();
    let first = projection(&journal, &[], 8, 64 * 1024);
    assert!(!first.repacked);
    assert_eq!(first.prior[0].rung, Some(Rung::Native));
    let (resources, _script) = resources(Arc::clone(&journal), [Action::Text("late")], true);
    assert!(matches!(
        run_part(&Arc::new(resources), 1, 1).await,
        Next::Stop
    ));
    assert_eq!(
        summaries_of(&journal, 1),
        vec![Slot::Generated("late".to_owned())]
    );
    let unchanged = projection(&journal, &first.prior, 8, 64 * 1024);
    assert!(!unchanged.repacked);
    assert_eq!(unchanged.prior, first.prior);
    assert_eq!(projected_text(&unchanged), projected_text(&first));
    let repacked = projection(&journal, &unchanged.prior, 8, first.bytes as usize - 1);
    assert!(repacked.repacked);
    assert_eq!(
        repacked.prior[0].rung,
        Some(Rung::Summary(NonZeroU8::new(1).unwrap()))
    );
    assert_eq!(repacked.rungs.summary, 1);
    assert!(projected_text(&repacked).contains("\nlate"));
}

struct Chooses(&'static str);
impl dekopon_shell::CapabilityInvoker for Chooses {
    fn granted(&self) -> Vec<String> {
        vec!["memory-decide.choose".to_owned()]
    }
    fn invoke(
        &self,
        _proposal: dekopon_shell::CommandProposal,
        streams: dekopon_shell::Streams,
        _tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::CapabilityCallResult {
        use std::io::Write as _;
        std::fs::File::from(streams.stdout)
            .write_all(format!("{{\"choice\":\"{}\"}}", self.0).as_bytes())
            .unwrap();
        dekopon_shell::CapabilityCallResult::Succeeded
    }
}

fn decided(journal: &Journal, answer: &'static str) -> Option<super::super::record::ChoiceLine> {
    let snapshot = journal.record_snapshot(STEM, false).unwrap().unwrap();
    let record = super::super::project::decider_candidate(&snapshot.records, 1)?;
    let runtime = dekopon_agent::ShellRuntime {
        invoker: Chooses(answer),
        limits: dekopon_process::Limits::default(),
        calls: dekopon_process::CallBudget::new(4),
    };
    super::super::decider::choose(
        &runtime,
        &crate::config::memory::Decider {
            capability: "memory-decide.choose".to_owned(),
            backend: "fixture".to_owned(),
            model: "fixture".to_owned(),
            require_probabilities: false,
        },
        record,
        &super::super::project::decider_labels(record),
        &super::super::decider::evidence(&snapshot.records, 10),
        10,
    )
}

#[tokio::test]
async fn the_decider_waits_for_final_slots() {
    let (_dir, journal) = journal();
    journal
        .append_record(STEM, &record(1, "leaving", 2), Vec::new())
        .unwrap();
    journal
        .append_record(STEM, &record(2, "hot", 0), Vec::new())
        .unwrap();
    assert!(decided(&journal, "summary-2").is_none());
    let (resources, _script) = resources(
        Arc::clone(&journal),
        [Action::Text("first"), Action::Text("second")],
        true,
    );
    let resources = Arc::new(resources);
    assert!(matches!(
        run_part(&resources, 1, 1).await,
        Next::Continue(_)
    ));
    assert_eq!(
        summaries_of(&journal, 1),
        vec![Slot::Generated("first".to_owned()), Slot::Queued]
    );
    assert!(decided(&journal, "summary-1").is_none());
    assert!(matches!(run_part(&resources, 1, 2).await, Next::Stop));
    let choice = decided(&journal, "summary-2").expect("a final record is asked");
    assert_eq!(choice.record, RecordId(1.try_into().unwrap()));
    assert_eq!(choice.choice, Rung::Summary(NonZeroU8::new(2).unwrap()));
}

#[test]
fn a_record_without_generated_summaries_is_never_asked() {
    let (_dir, journal) = journal();
    let mut errored = record(1, "errored-original", 2);
    errored.summaries = vec![error_slot(SummaryFailure::Timeout, "timeout"); 2];
    journal.append_record(STEM, &errored, Vec::new()).unwrap();
    journal
        .append_record(STEM, &record(2, "unplanned-original", 0), Vec::new())
        .unwrap();
    journal
        .append_record(STEM, &record(3, "hot", 0), Vec::new())
        .unwrap();
    assert!(decided(&journal, "metadata").is_none());
    let projected = projection(&journal, &[], 8, 64 * 1024);
    assert_eq!(projected.prior[0].rung, Some(Rung::Native));
    assert_eq!(projected.prior[1].rung, Some(Rung::Native));
    let text = projected_text(&projected);
    assert!(text.contains("errored-original") && text.contains("unplanned-original"));
}

fn window_projection(
    journal: &Journal,
    prior: &[super::super::project::Prior],
    (hot_turns, max_turns, max_bytes): (usize, usize, usize),
) -> (super::super::project::Projected, Vec<RecordId>) {
    let mut snapshot = journal.record_snapshot(STEM, false).unwrap().unwrap();
    let walked = snapshot.records.len().saturating_sub(max_turns * 2);
    snapshot.records.drain(..walked);
    let projected = super::super::project::project(&super::super::project::ProjectionInput {
        file: &snapshot.file,
        records: &snapshot.records,
        prior,
        hot_turns,
        max_turns,
        max_bytes,
        model: &fixture().model,
    })
    .unwrap();
    let ids = snapshot.records.iter().map(|head| head.header.id).collect();
    (projected, ids)
}

#[test]
fn consecutive_turns_differ_at_the_hot_edge_only() {
    const MAX_TURNS: usize = 4;
    let (_dir, journal) = journal();
    let mut previous: Option<super::super::project::Projected> = None;
    let mut held_below_pin = 0;
    for turn in 1..=(3 * MAX_TURNS) as u64 {
        journal
            .append_record(
                STEM,
                &record(turn, &format!("turn {turn} {}", "x".repeat(300)), 0),
                Vec::new(),
            )
            .unwrap();
        if turn == 4 {
            journal
                .write_pin(
                    STEM,
                    super::super::record::PinLine {
                        at_ms: 4,
                        record: RecordId(1.try_into().unwrap()),
                        pin: Some(Rung::Native),
                        subject: Subject::Record,
                    },
                )
                .unwrap();
        }
        let prior = previous
            .as_ref()
            .map_or(&[][..], |projected| &projected.prior);
        let (projected, _ids) = window_projection(&journal, prior, (2, MAX_TURNS, 1_500));
        if let Some(previous) = &previous {
            assert!(
                !(previous.repacked && projected.repacked),
                "turn {turn} repacked right after a repack"
            );
            if !projected.repacked {
                for now in &projected.prior {
                    if let Some(before) = previous
                        .prior
                        .iter()
                        .find(|before| before.record == now.record)
                        .filter(|before| before.hot == now.hot && before.pin == now.pin)
                    {
                        assert_eq!(before.rung, now.rung, "turn {turn}: {:?}", now.record);
                    }
                }
            }
        }
        if let Some(pinned) = projected
            .prior
            .iter()
            .find(|prior| prior.record.0.get() == 1 && prior.pin == Some(Rung::Native))
            && pinned.rung != Some(Rung::Native)
        {
            held_below_pin += 1;
        }
        previous = Some(projected);
    }
    assert!(
        held_below_pin >= 2,
        "the pinned record was held below its pin"
    );
}

#[test]
fn a_turn_never_silently_disappears() {
    let (_dir, journal) = journal();
    let mut summarized = record(1, &"summarized ".repeat(80), 1);
    summarized.summaries = vec![Slot::Generated("a generated summary".to_owned())];
    journal
        .append_record(STEM, &summarized, Vec::new())
        .unwrap();
    for id in 2..=6 {
        journal
            .append_record(STEM, &record(id, &"older ".repeat(120), 0), Vec::new())
            .unwrap();
    }
    let mut budget = 64 * 1024;
    loop {
        let (projected, ids) = window_projection(&journal, &[], (2, 12, budget));
        let shown = projected.rungs.native + projected.rungs.summary + projected.rungs.metadata;
        assert_eq!(
            shown as usize + projected.omitted.len(),
            ids.len(),
            "budget {budget}"
        );
        assert!(projected.omitted.iter().all(|id| ids.contains(id)));
        if budget == 1 {
            break;
        }
        budget = (budget / 2).max(1);
    }
}

#[tokio::test]
async fn each_slot_change_is_one_transition_event_inside_its_span() {
    let (capture, _guard) = crate::tests::capture_spans();
    let (_dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let (resources, _script) = resources(Arc::clone(&journal), [Action::Text("summary")], true);
    run(
        Arc::new(resources),
        Queued {
            task: task(),
            since: Instant::now(),
        },
        watch::channel(false).1,
    )
    .await;
    let transitions: Vec<_> = capture
        .records()
        .into_iter()
        .filter(|record| record.field("event") == Some("gateway_memory_summary_transition"))
        .collect();
    assert_eq!(
        transitions
            .iter()
            .map(|record| (
                record.field("memory.summary.from").unwrap(),
                record.field("memory.summary.to").unwrap()
            ))
            .collect::<Vec<_>>(),
        [("queued", "running"), ("running", "generated")]
    );
    assert!(
        transitions
            .iter()
            .all(|record| record.parent() == Some("memory.summarize"))
    );
    assert_eq!(transitions[1].field("memory.summary.bytes"), Some("7"));
    assert_eq!(transitions[1].field("memory.summary.text"), Some("summary"));
}

#[tokio::test]
async fn an_overflowed_part_logs_one_drop_per_later_part() {
    let (capture, _guard) = crate::tests::capture_spans();
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
    let parts: Vec<_> = capture
        .records()
        .into_iter()
        .filter(|record| record.field("event") == Some("gateway_memory_summary_dropped"))
        .map(|record| {
            assert_eq!(record.field("memory.summary.failure"), Some("overflow"));
            record.field("memory.summary.part").unwrap().to_owned()
        })
        .collect();
    assert_eq!(parts, ["1", "2", "3"]);
}
