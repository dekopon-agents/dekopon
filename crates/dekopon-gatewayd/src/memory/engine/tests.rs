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
