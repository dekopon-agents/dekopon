use super::*;
use crate::memory::{
    engine::{Counts, SlotTask},
    record::{Slot, SummaryFailure},
};

#[tokio::test]
async fn a_memory_persist_span_reaches_the_installed_trace_pipeline() {
    if std::env::var_os("DEKOPON_TEST_MEMORY_PERSIST_SUBSCRIBER").is_none() {
        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tests::memory::a_memory_persist_span_reaches_the_installed_trace_pipeline",
            ])
            .env("DEKOPON_TEST_MEMORY_PERSIST_SUBSCRIBER", "1")
            .output()
            .expect("isolated subscriber test");
        assert!(
            child.status.success(),
            "subscriber test: stdout={} stderr={}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        return;
    }
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        trace::{SdkTracerProvider, SpanData, SpanExporter},
    };

    #[derive(Clone, Debug, Default)]
    struct Exported(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Exported {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().extend(batch);
            Ok(())
        }
    }

    let exported = Exported::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exported.clone())
        .build();
    let telemetry = dekopon_telemetry::Install::new(dekopon_telemetry::Console {
        format: dekopon_telemetry::ConsoleFormat::Json,
        writer: dekopon_telemetry::ConsoleWriter::Stderr,
        filter: dekopon_telemetry::ConsoleFilter::Directive("info".into()),
    })
    .with_traces(provider, "persist-test", crate::OTEL_TRACE_FILTER)
    .with_logs(
        opentelemetry_sdk::logs::SdkLoggerProvider::builder().build(),
        "job=debug,meter=info",
    )
    .install()
    .expect("sole global test subscriber");
    let directory = temporary();
    let journal = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let model = ModelScript::new([answer("reply delivered")]);
    let runner = journaled_runner(
        broker,
        Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
        journal.path(),
    );
    let route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        runner,
        route,
        message("persist after delivery"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["reply delivered"]);
    telemetry.shutdown().expect("exporters flushed");

    let spans = exported.0.lock().clone();
    let named = |name: &str| {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("{name} exported"))
    };
    let (message, session, persist) = (
        named("gateway.message"),
        named("gateway.session"),
        named("memory.persist"),
    );
    assert_eq!(session.parent_span_id, message.span_context.span_id());
    assert_eq!(persist.parent_span_id, message.span_context.span_id());
    assert!(session.end_time <= persist.start_time);
    assert!(persist.end_time <= message.end_time);
    let delivered = session
        .events
        .iter()
        .find(|event| event.name == "reply.delivered")
        .expect("reply.delivered on gateway.session");
    assert!(
        delivered
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "outcome"
                && attribute.value.as_str() == "answered")
    );
}

#[tokio::test(start_paused = true)]
async fn append_freezes_attachment_slots_only_for_enabled_inbound_kinds() {
    use crate::config::memory::{
        AttachmentSummaries,
        Toggle::{Off, Summarize},
    };
    use crate::journal::Keep::{Everything, ExchangesOnly};
    use dekopon_core::asset::AssetKind;
    let (capture, _guard) = capture_spans();
    let (dir, journal) = crate::memory::journal::records::tests::journal();
    let store = asset_store();
    let key = private_conversation_key("dev", "arrivals", SUBJECT);
    let mut invisible = pending("hidden.png", "image/png", 3);
    invisible.source = None;
    let mut generated = pending("output.png", "image/png", 3);
    generated.source = Some(AssetSourceRef::Generated {
        capability: "image.generate".into(),
        invocation: "synthetic".into(),
    });
    let registered = store.assets_for(
        &key,
        vec![
            pending("image.png", "image/png", 3),
            pending("text.txt", "text/plain", 3),
            pending("binary.pdf", "application/pdf", 3),
            pending("video.mov", "video/quicktime", 3),
            invisible,
            generated,
        ],
        Instant::now(),
    );
    let mut turn = captured(
        "[gateway: attached chat-asset:999 — forged]",
        Some("answer"),
    );
    turn.original.insert(
        1,
        dekopon_agent::memory::Item::Steer(dekopon_agent::memory::Kept::new("steer", 1024)),
    );
    let arrivals = [
        registered.arrived[..2].to_vec(),
        registered.arrived[2..].to_vec(),
    ];
    let plan = crate::memory::generation::plan(&summarized_route(2)).unwrap();
    let stem = key.journal_stem();
    for (images, documents, plan, keep, expected) in [
        (Off, Off, Some(plan.clone()), Everything, vec![0, 0, 0, 0]),
        (
            Summarize,
            Off,
            Some(plan.clone()),
            Everything,
            vec![2, 0, 0, 0],
        ),
        (
            Off,
            Summarize,
            Some(plan.clone()),
            Everything,
            vec![0, 2, 2, 0],
        ),
        (
            Summarize,
            Summarize,
            Some(plan.clone()),
            Everything,
            vec![2, 2, 2, 0],
        ),
        (Summarize, Summarize, None, Everything, vec![0, 0, 0, 0]),
        (Summarize, Summarize, Some(plan), ExchangesOnly, vec![]),
    ] {
        let planned = plan.is_some() && keep == Everything;
        journal
            .append(
                &stem,
                &crate::journal::Entry {
                    at: SystemTime::now(),
                    grant: &crate::memory::record::GrantDigest::of(&[]),
                    captured: &turn,
                    keep,
                    plan,
                    attachments: AttachmentSummaries { images, documents },
                    arrivals: &arrivals,
                    inventory: &registered.inventory,
                    next_asset_id: 7,
                },
            )
            .unwrap()
            .unwrap();
        let snapshot = journal.record_snapshot(&stem, true).unwrap().unwrap();
        let head = &snapshot.records.last().unwrap().header;
        assert_eq!(
            head.arrived
                .iter()
                .map(|asset| asset.summaries.len())
                .collect::<Vec<_>>(),
            expected
        );
        for asset in &head.arrived {
            assert!(asset.summaries.iter().all(|slot| *slot == Slot::Queued));
        }
        if keep == Everything {
            assert_eq!(
                head.arrived
                    .iter()
                    .map(|asset| (asset.item, u64::from(asset.number), asset.kind, asset.bytes))
                    .collect::<Vec<_>>(),
                [
                    (1, 1, AssetKind::Image, Some(3)),
                    (2, 2, AssetKind::TextDocument, Some(3)),
                    (4, 3, AssetKind::BinaryDocument, Some(3)),
                    (5, 4, AssetKind::Other, Some(3))
                ]
            );
            let records = journal_records(&journal_path(dir.path(), &stem));
            assert_eq!(
                body_kinds(&records.last().unwrap().1),
                [
                    "user",
                    "asset",
                    "asset",
                    "steer",
                    "asset",
                    "asset",
                    "assistant"
                ]
            );
        }
        let records = capture.records();
        let published = records
            .iter()
            .rfind(|record| record.field("event") == Some("gateway_memory_published"))
            .unwrap();
        assert_eq!(
            published.field("memory.assets.arrived"),
            Some(expected.len().to_string().as_str())
        );
        assert_eq!(
            published.field("memory.slots.queued"),
            Some(
                (usize::from(planned) * 2 + expected.iter().sum::<usize>())
                    .to_string()
                    .as_str()
            )
        );
    }
}

const BIG_OUTPUT: &str = "x=0123456789abcdef; for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14; do x=$x$x; done; printf %s \"$x\"";

struct HeldListener {
    endpoint: String,
    accepted: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    _server: tokio::task::JoinSet<()>,
}

async fn held_listener() -> HeldListener {
    use tokio::io::AsyncReadExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let (seen, gone) = (Arc::clone(&accepted), Arc::clone(&closed));
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let (seen, gone) = (Arc::clone(&seen), Arc::clone(&gone));
            connections.spawn(async move {
                let mut buffer = vec![0_u8; 64 * 1024];
                let mut counted = false;
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    if !counted {
                        counted = true;
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                }
                gone.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    HeldListener {
        endpoint,
        accepted,
        closed,
        _server: server,
    }
}

async fn until(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

struct Summarizing {
    runner: Arc<SessionRunner>,
    journal: Arc<crate::journal::Journal>,
    counts: Arc<Counts>,
    owned: tokio::task::JoinSet<()>,
}

fn summarizing(
    broker: ResolvedBroker,
    models: Arc<dyn ModelFactory>,
    journal: &Path,
    endpoint: &str,
    timeout_ms: u64,
    (queue, concurrency): (usize, usize),
) -> Summarizing {
    let mut runner =
        Arc::into_inner(journaled_runner(broker, models, journal)).expect("a fresh runner");
    let journal = Arc::clone(runner.journal.as_ref().expect("journaled"));
    let counts = Arc::<Counts>::default();
    let (sender, dispatcher) = crate::memory::engine::channel(
        Arc::clone(&journal),
        Arc::clone(&runner.assets),
        Arc::clone(&counts),
        queue.try_into().unwrap(),
        concurrency.try_into().unwrap(),
    );
    let mut routes = RoutingTable::default();
    routes.models.insert(
        "summary".to_owned(),
        Arc::new(
            serde_json::from_value(json!({"kind":"openaiCompatible","name":"summary","endpoint":endpoint,"model":"fixture","timeoutMs":timeout_ms}))
                .unwrap(),
        ),
    );
    let mut owned = tokio::task::JoinSet::new();
    dispatcher.start(
        crate::gateway::CurrentGeneration::boot(Arc::new(routes), Arc::default()),
        Arc::new(ModelCache::new(Arc::new(
            crate::session::ConfiguredModels::default(),
        ))),
        Arc::clone(&runner.thread_owner),
        runner.shell_threads.clone(),
        &mut owned,
    );
    runner.summaries = Some(sender);
    Summarizing {
        runner: Arc::new(runner),
        journal,
        counts,
        owned,
    }
}

fn summarized_route(parts: usize) -> crate::routes::BoundRoute {
    let mut route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    route.memory_knobs.as_mut().unwrap().summaries =
        Some(crate::config::memory::ResolvedSummaries {
            model: crate::config::memory::SummaryModel::Named("summary".to_owned()),
            parts: vec![std::num::NonZeroU32::new(64).unwrap(); parts],
            max_input_bytes: std::num::NonZeroU32::new(2 * 1024 * 1024).unwrap(),
            attachments: Default::default(),
        });
    route
}

fn dev_stem() -> String {
    private_conversation_key("dev", &message("").conversation.key(), SUBJECT).journal_stem()
}

fn record_slots(journal: &crate::journal::Journal, stem: &str) -> Vec<Vec<Slot>> {
    journal
        .record_snapshot(stem, true)
        .unwrap()
        .map_or_else(Vec::new, |snapshot| {
            snapshot
                .records
                .into_iter()
                .map(|head| head.header.summaries)
                .collect()
        })
}

fn timed_out(slot: &Slot) -> bool {
    matches!(slot, Slot::Error(error) if error.kind == SummaryFailure::Timeout)
}

fn with_budget(runner: Arc<SessionRunner>, limit: u64, spent: u64) -> Arc<SessionRunner> {
    let mut runner = Arc::into_inner(runner).expect("a fresh runner");
    let now = dekopon_model_token_governor::UnixMillis::now();
    let agent: AgentId = "reviewer".parse().unwrap();
    let metering = dekopon_model_token_governor::Metering::new(
        vec![dekopon_model_token_governor::Budget::new(
            agent.clone(),
            None,
            &[dekopon_model_token_governor::MeterSpec::Rolling {
                limit: dekopon_model_token_governor::Tokens(limit),
                period: Duration::from_secs(5 * 3_600),
            }],
            now,
        )],
        dekopon_model_token_governor::Metering::system_clock(),
    );
    metering.restore(
        now,
        &[dekopon_model_token_governor::HistoryRow {
            agent,
            model: "local-qwen".to_owned(),
            at: now,
            tokens: dekopon_model_token_governor::Tokens(spent),
        }],
    );
    runner.metering = Arc::new(metering);
    Arc::new(runner)
}

fn used(runner: &SessionRunner) -> u64 {
    runner
        .metering
        .statuses(&"reviewer".parse().unwrap())
        .expect("a budget")[0]
        .1
        .used
        .0
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hanging_summarizer_never_delays_a_reply() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let listener = held_listener().await;
    let models = ModelScript::new([answer("first reply"), answer("second reply")]);
    let mut summarizing = summarizing(
        broker,
        Arc::new(Arc::clone(&models)),
        journal_dir.path(),
        &listener.endpoint,
        5_000,
        (4, 1),
    );
    let route = summarized_route(1);
    let driver = Arc::new(RecordingDriver::default());
    let stem = dev_stem();
    run_session(
        Arc::clone(&summarizing.runner),
        route.clone(),
        message("first"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    until("the summary call reached its listener", || {
        listener.accepted.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(record_slots(&summarizing.journal, &stem), [[Slot::Running]]);
    tokio::time::timeout(
        Duration::from_secs(4),
        run_session(
            Arc::clone(&summarizing.runner),
            route,
            message("second"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        ),
    )
    .await
    .expect("the next turn never waits for the held summary");
    assert_eq!(driver.replies(), ["first reply", "second reply"]);
    assert_eq!(
        record_slots(&summarizing.journal, &stem),
        [[Slot::Running], [Slot::Queued]]
    );
    until("the held summary timed out", || {
        timed_out(&record_slots(&summarizing.journal, &stem)[0][0])
    })
    .await;
    summarizing.owned.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_reply_waits_on_a_summary() {
    const LIMIT: u64 = 1_000_000;
    const ROOM: u64 = 120_000;
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(3, &["cli-probe.upper"])).await;
    let listener = held_listener().await;
    let models = ModelScript::new([answer("one"), answer("two"), answer("three")]);
    let Summarizing {
        runner,
        journal,
        mut owned,
        ..
    } = summarizing(
        broker,
        Arc::new(Arc::clone(&models)),
        journal_dir.path(),
        &listener.endpoint,
        3_000,
        (8, 1),
    );
    let runner = with_budget(runner, LIMIT, LIMIT - ROOM);
    let stem = dev_stem();
    let original = "1 MiB original ".repeat(64 * 1024 / 15 * 16);
    assert!(original.len() >= 1024 * 1024 - 64);
    let record = journal
        .append(
            &stem,
            &crate::journal::Entry {
                at: SystemTime::now(),
                grant: &crate::memory::record::GrantDigest::of(&granted(&["cli-probe.upper"])),
                captured: &captured(&original, Some("noted")),
                keep: crate::journal::Keep::Everything,
                plan: crate::memory::generation::plan(&summarized_route(1)),
                attachments: Default::default(),
                arrivals: &[],
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap()
        .id;
    runner
        .summaries
        .as_ref()
        .unwrap()
        .enqueue(SlotTask {
            stem: stem.clone(),
            record,
            part: std::num::NonZeroU8::MIN,
            subject: crate::memory::record::Subject::Record,
            access: None,
        })
        .await;
    until("the engine is sending the 1 MiB original", || {
        listener.accepted.load(Ordering::SeqCst) == 1
    })
    .await;
    let (held, holding) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let holder = {
        let journal = Arc::clone(&journal);
        std::thread::spawn(move || {
            let _lock = journal.hold_io_for_test();
            held.send(()).unwrap();
            released.recv().unwrap();
        })
    };
    holding.recv().unwrap();
    let route = summarized_route(1);
    let driver = Arc::new(RecordingDriver::default());
    let first = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        message("first"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    until("the reply went out while Journal::lock was held", || {
        driver.replies().len() == 1
    })
    .await;
    assert_eq!(
        models.prompt(0)[1],
        (
            "system".to_owned(),
            "[gateway: memory]\nOmitted window-fit: 1.\n".to_owned()
        )
    );
    assert_eq!(record_slots(&journal, &stem).len(), 1);
    release.send(()).unwrap();
    holder.join().unwrap();
    first.await.unwrap();
    for text in ["second", "third"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(text),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    assert_eq!(driver.replies(), ["one", "two", "three"]);
    assert!(listener.accepted.load(Ordering::SeqCst) >= 1);
    let slots = record_slots(&journal, &stem);
    assert_eq!(slots.len(), 4);
    assert!(matches!(slots[0][0], Slot::Running) || timed_out(&slots[0][0]));
    assert!(used(&runner) < LIMIT);
    owned.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_aborts_summaries_within_the_drain() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let listener = held_listener().await;
    let models = ModelScript::new([answer("delivered")]);
    let mut summarizing = summarizing(
        broker,
        Arc::new(Arc::clone(&models)),
        journal_dir.path(),
        &listener.endpoint,
        60_000,
        (4, 1),
    );
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&summarizing.runner),
        summarized_route(1),
        message("summarize me later"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["delivered"]);
    until("the summary call is held", || {
        listener.accepted.load(Ordering::SeqCst) == 1
    })
    .await;
    let stem = dev_stem();
    assert_eq!(record_slots(&summarizing.journal, &stem), [[Slot::Running]]);
    tokio::time::timeout(Duration::from_secs(2), summarizing.owned.shutdown())
        .await
        .expect("shutdown never waits for the model's timeoutMs");
    until("the held call's connection closed", || {
        listener.closed.load(Ordering::SeqCst) == 1
    })
    .await;
    drop(summarizing);
    let (journal, boot) = crate::journal::Journal::boot(
        journal_dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
        SystemTime::now(),
    )
    .unwrap();
    assert_eq!(boot.requeued, 1);
    assert_eq!(boot.tasks.len(), 1);
    assert_eq!(boot.tasks[0].stem, stem);
    assert_eq!(record_slots(&journal, &stem), [[Slot::Queued]]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_of_big_tool_turns_overflows_and_counts() {
    const TURNS: usize = 50;
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(TURNS, &["cli-probe.upper"])).await;
    let listener = held_listener().await;
    let models = ModelScript::new(
        (0..TURNS).flat_map(|turn| [script_call(BIG_OUTPUT), answer(&format!("reply {turn}"))]),
    );
    let mut summarizing = summarizing(
        broker,
        Arc::new(Arc::clone(&models)),
        journal_dir.path(),
        &listener.endpoint,
        60_000,
        (2, 1),
    );
    let route = summarized_route(1);
    let driver = Arc::new(RecordingDriver::default());
    for turn in 0..TURNS {
        run_session(
            Arc::clone(&summarizing.runner),
            route.clone(),
            message(&format!("turn {turn}")),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        if turn == 0 {
            until("the first summary is held", || {
                listener.accepted.load(Ordering::SeqCst) == 1
            })
            .await;
        }
    }
    assert_eq!(
        driver.replies(),
        (0..TURNS)
            .map(|turn| format!("reply {turn}"))
            .collect::<Vec<_>>()
    );
    assert!(
        fs::metadata(journal_dir.path().join(format!("{}.jsonl", dev_stem())))
            .unwrap()
            .len()
            >= (TURNS * 128 * 1024) as u64
    );
    let slots = record_slots(&summarizing.journal, &dev_stem());
    assert_eq!(slots.len(), TURNS);
    assert_eq!(slots[0], [Slot::Running]);
    assert_eq!(slots[1..3], [[Slot::Queued], [Slot::Queued]]);
    assert!(slots[3..].iter().all(|slots| matches!(
        &slots[..],
        [Slot::Error(error)] if error.kind == SummaryFailure::Overflow
    )));
    assert_eq!(
        summarizing.counts.overflowed.load(Ordering::Relaxed),
        TURNS as u64 - 3
    );
    assert_eq!(summarizing.counts.enqueued.load(Ordering::Relaxed), 3);
    summarizing.owned.shutdown().await;
}

type Before = Box<dyn Fn(usize) + Send + Sync>;

struct Hook {
    script: Arc<ModelScript>,
    before: Before,
}

struct Hooked(Arc<Hook>);

impl Hooked {
    fn new(script: &Arc<ModelScript>, before: impl Fn(usize) + Send + Sync + 'static) -> Self {
        Self(Arc::new(Hook {
            script: Arc::clone(script),
            before: Box::new(before),
        }))
    }
}

impl ModelFactory for Hooked {
    fn build_bound(
        &self,
        _model: &Arc<ModelConfig>,
        _runtime: tokio::runtime::Handle,
        _cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        Ok(Arc::new(HookedModel(Arc::clone(&self.0))))
    }
}

struct HookedModel(Arc<Hook>);

impl ChatModel for HookedModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        (self.0.before)(self.0.script.requests());
        ScriptedModel(Arc::clone(&self.0.script)).complete(messages, tools, options, on_event)
    }
}

fn journal_path(journal: &Path, stem: &str) -> PathBuf {
    journal.join(format!("{stem}.jsonl"))
}

fn tool_call(id: &str, name: &str, arguments: Value) -> AssistantTurn {
    AssistantTurn::new(
        None,
        vec![ModelToolCall {
            id: id.into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: name.to_owned(),
                arguments: arguments.to_string(),
            },
        }],
        None,
    )
}

fn body_kinds(body: &Value) -> Vec<String> {
    body.as_array()
        .expect("a body array")
        .iter()
        .map(|item| {
            item.as_object()
                .and_then(|object| object.keys().next().cloned())
                .unwrap_or_default()
        })
        .collect()
}

struct Answering {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    _server: tokio::task::JoinSet<()>,
}

async fn answering_summarizer(usage_tokens: u64) -> Answering {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            if read_http_request_parts(&mut stream).await.is_none() {
                continue;
            }
            counted.fetch_add(1, Ordering::SeqCst);
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"content":"generated summary"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":usage_tokens,"completion_tokens":5,"total_tokens":usage_tokens + 5}}),
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                continue;
            }
        }
    });
    Answering {
        endpoint,
        calls,
        _server: server,
    }
}

struct LargeReply {
    replies: Vec<String>,
    used: u64,
    summarized_before_step_two: bool,
    summary_calls: usize,
}

async fn large_reply_turn(limit: u64) -> LargeReply {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let summarizer = answering_summarizer(10 * limit).await;
    let models = ModelScript::new([
        script_call(&format!("sleep 2; {BIG_OUTPUT}")),
        answer("carried the output"),
    ]);
    let stem = dev_stem();
    let observed = Arc::new(std::sync::OnceLock::<Arc<crate::journal::Journal>>::new());
    let summarized = Arc::new(AtomicBool::new(false));
    let factory = {
        let (observed, summarized, stem) =
            (Arc::clone(&observed), Arc::clone(&summarized), stem.clone());
        Hooked::new(&models, move |request| {
            if request == 1 {
                let journal = observed.get().expect("journal installed");
                summarized.store(
                    matches!(record_slots(journal, &stem)[0][..], [Slot::Generated(_)]),
                    Ordering::SeqCst,
                );
            }
        })
    };
    let Summarizing {
        runner,
        journal,
        mut owned,
        ..
    } = summarizing(
        broker,
        Arc::new(factory),
        journal_dir.path(),
        &summarizer.endpoint,
        5_000,
        (4, 1),
    );
    assert!(observed.set(Arc::clone(&journal)).is_ok());
    let runner = with_budget(runner, limit, 0);
    let record = journal
        .append(
            &stem,
            &crate::journal::Entry {
                at: SystemTime::now(),
                grant: &crate::memory::record::GrantDigest::of(&granted(&["cli-probe.upper"])),
                captured: &captured("an earlier question", Some("an earlier answer")),
                keep: crate::journal::Keep::Everything,
                plan: crate::memory::generation::plan(&summarized_route(1)),
                attachments: Default::default(),
                arrivals: &[],
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap()
        .id;
    let driver = Arc::new(RecordingDriver::default());
    let session = tokio::spawn(run_session(
        Arc::clone(&runner),
        summarized_route(1),
        message("produce a large output"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    until("step one asked for the bash call", || {
        models.requests() == 1
    })
    .await;
    runner
        .summaries
        .as_ref()
        .unwrap()
        .enqueue(SlotTask {
            stem: stem.clone(),
            record,
            part: std::num::NonZeroU8::MIN,
            subject: crate::memory::record::Subject::Record,
            access: None,
        })
        .await;
    session.await.unwrap();
    owned.shutdown().await;
    LargeReply {
        replies: driver.replies(),
        used: used(&runner),
        summarized_before_step_two: summarized.load(Ordering::SeqCst),
        summary_calls: summarizer.calls.load(Ordering::SeqCst),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn summaries_leave_room_for_a_large_reply() {
    let measured = large_reply_turn(1_000_000_000).await;
    assert_eq!(measured.replies, ["carried the output"]);
    let limit = measured.used + model_config().output_reserve().0 + 2_000;
    let tight = large_reply_turn(limit).await;
    assert_eq!(tight.replies, ["carried the output"]);
    assert!(tight.summarized_before_step_two);
    assert!(tight.summary_calls >= 1);
    assert!(tight.used < limit, "{} of {limit}", tight.used);
    assert!(tight.used > 128 * 1024 / 4, "step two carried the output");
}

#[tokio::test]
async fn an_unloadable_journal_resets_with_one_startup_record() {
    let directory = temporary();
    let journal = crate::journal::Journal::open(
        directory.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
    )
    .unwrap();
    let pending = private_conversation_key("dev", "pending", SUBJECT).journal_stem();
    let unloadable = private_conversation_key("dev", "unloadable", SUBJECT).journal_stem();
    let crashed = private_conversation_key("dev", "crashed", SUBJECT).journal_stem();
    let record = journal
        .append(
            &pending,
            &crate::journal::Entry {
                at: SystemTime::now(),
                grant: &crate::memory::record::GrantDigest::of(&granted(&["cli-probe.upper"])),
                captured: &captured("summarize me", Some("later")),
                keep: crate::journal::Keep::Everything,
                plan: crate::memory::generation::plan(&summarized_route(2)),
                attachments: Default::default(),
                arrivals: &[],
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap()
        .id;
    journal
        .write_slot(
            &pending,
            crate::memory::record::SlotLine {
                at_ms: 1,
                record,
                part: std::num::NonZeroU8::MIN,
                slot: Slot::Running,
                subject: crate::memory::record::Subject::Record,
            },
        )
        .unwrap();
    append_captured(
        &journal,
        &crashed,
        &["cli-probe.upper"],
        &captured("crashed", None),
    );
    drop(journal);
    fs::write(
        journal_path(directory.path(), &unloadable),
        b"{\"b\":[]}\n{broken\n",
    )
    .unwrap();
    fs::write(directory.path().join("loading.marker"), &crashed).unwrap();
    let (capture, _guard) = capture_spans();
    let (journal, boot) = crate::journal::Journal::boot(
        directory.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
        SystemTime::now(),
    )
    .unwrap();
    assert!(!journal_path(directory.path(), &unloadable).exists());
    assert!(!journal_path(directory.path(), &crashed).exists());
    assert_eq!(
        record_slots(&journal, &pending),
        [[Slot::Queued, Slot::Queued]]
    );
    let (sender, _dispatcher) = crate::memory::engine::channel(
        Arc::new(journal),
        Arc::new(asset_store()),
        Arc::default(),
        4.try_into().unwrap(),
        1.try_into().unwrap(),
    );
    crate::enqueue_boot(&sender, boot).await;
    let startup = capture.events_where("dekopon_gatewayd", &[("event", "gateway_memory_startup")]);
    assert_eq!(startup.len(), 1);
    for (field, value) in [
        ("memory.journals.reset", "2"),
        ("memory.slots.requeued", "1"),
        ("memory.slots.enqueued", "1"),
        ("memory.slots.overflowed", "0"),
        ("memory.boot.crashed_files", "1"),
    ] {
        assert_eq!(startup[0].field(field), Some(value), "{field}");
    }
    assert_eq!(sender.counts.enqueued.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_turn_keeps_its_record() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::results([
        Ok(tool_call(
            "first-call",
            "bash",
            json!({"command": "printf first-exchange"}),
        )),
        Ok(tool_call(
            "second-call",
            "bash",
            json!({"command": "printf second-exchange"}),
        )),
        Err(InferenceError::DeadlineExceeded),
    ]);
    let runner = journaled_runner(broker, Arc::new(Arc::clone(&models)), journal_dir.path());
    let route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        route.clone(),
        message("run two commands"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(models.requests(), 3);
    assert_eq!(driver.replies().len(), 1);
    let path = journal_path(journal_dir.path(), &dev_stem());
    let records = journal_records(&path);
    assert_eq!(records.len(), 1);
    let (_header, body) = &records[0];
    assert_eq!(body_kinds(body), ["user", "exchange", "exchange"]);
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(bytes.contains("first-exchange") && bytes.contains("second-exchange"));

    let zero_dir = temporary();
    let (broker, _) = stub_broker(zero_dir.path(), listings(1, &["cli-probe.upper"])).await;
    let zero_journal = temporary();
    let models = ModelScript::forbidden();
    let runner = journaled_runner(broker, Arc::new(Arc::clone(&models)), zero_journal.path());
    let mut zero = route;
    zero.limits.max_steps = 0;
    run_session(
        runner,
        zero,
        message("zero steps"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies().len(), 2);
    assert!(!journal_path(zero_journal.path(), &dev_stem()).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resident_platform_turn_replays_the_earlier_turn() {
    for recall in [RecallSource::Platform, RecallSource::None] {
        let directory = temporary();
        let journal_dir = temporary();
        let (broker, _) = stub_broker(directory.path(), listings(4, &["cli-probe.upper"])).await;
        let models = ModelScript::new((1..=4).map(|turn| answer(&format!("answer {turn}"))));
        let runner = journaled_runner(broker, Arc::new(Arc::clone(&models)), journal_dir.path());
        let mut window = recall_window(recall);
        window.limits.max_turns = 2;
        let route = persistent_route(model_config(), window);
        let driver = HistoryDriver::new(Ok(Vec::new()));
        for turn in 1..=4 {
            run_session(
                Arc::clone(&runner),
                route.clone(),
                message(&format!("turn {turn}")),
                Arc::clone(&driver) as Arc<dyn ChatDriver>,
            )
            .await;
        }
        let conversation = |index: usize| -> Vec<(String, String)> {
            models
                .prompt(index)
                .into_iter()
                .filter(|(role, _)| role != "system")
                .collect()
        };
        assert_eq!(
            conversation(1),
            transcript(&[
                ("user", "[gateway: unmapped participant]\nturn 1"),
                ("assistant", "answer 1"),
                ("user", "[gateway: unmapped participant]\nturn 2"),
            ]),
            "{recall:?}"
        );
        assert_eq!(
            conversation(3),
            transcript(&[
                ("user", "[gateway: unmapped participant]\nturn 2"),
                ("assistant", "answer 2"),
                ("user", "[gateway: unmapped participant]\nturn 3"),
                ("assistant", "answer 3"),
                ("user", "[gateway: unmapped participant]\nturn 4"),
            ]),
            "{recall:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mid_file_malformed_line_is_reset_after_delivery() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let stem = dev_stem();
    let path = journal_path(journal_dir.path(), &stem);
    let models = ModelScript::new([answer("fresh answer")]);
    let before = Arc::new(Mutex::new(Vec::new()));
    let unchanged = Arc::new(AtomicBool::new(false));
    let factory = {
        let (unchanged, path, before) = (Arc::clone(&unchanged), path.clone(), Arc::clone(&before));
        Hooked::new(&models, move |_| {
            unchanged.store(fs::read(&path).unwrap() == *before.lock(), Ordering::SeqCst);
        })
    };
    let runner = journaled_runner(broker, Arc::new(factory), journal_dir.path());
    let journal = runner.journal.as_ref().unwrap();
    append_captured(
        journal,
        &stem,
        &["cli-probe.upper"],
        &captured("older-canary", Some("older")),
    );
    append_captured(
        journal,
        &stem,
        &["cli-probe.upper"],
        &captured("newer-canary", Some("newer")),
    );
    let written = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = written.split_inclusive('\n').collect();
    assert_eq!(lines.len(), 4);
    fs::write(
        &path,
        [lines[0], lines[1], "{broken\n", lines[2], lines[3]].concat(),
    )
    .unwrap();
    *before.lock() = fs::read(&path).unwrap();
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        runner,
        persistent_route(model_config(), recall_window(RecallSource::Journal)),
        message("fresh question"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["fresh answer"]);
    assert!(
        unchanged.load(Ordering::SeqCst),
        "the reader left the file alone"
    );
    assert!(
        models
            .prompt(0)
            .iter()
            .all(|(_, text)| !text.contains("older-canary") && !text.contains("newer-canary"))
    );
    let records = journal_records(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(journal_lines(&path).len(), 2);
    assert!(records[0].1.to_string().contains("fresh question"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resident_journal_turn_projects_from_its_own_fd() {
    use std::os::unix::fs::MetadataExt as _;
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([answer("first answer"), answer("second answer")]);
    let runner = journaled_runner(broker, Arc::new(Arc::clone(&models)), journal_dir.path());
    let route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    let mut first = message("first question");
    first.message_id = MessageId::Native("0123456789abcdef0123456789abcdef-1-1".to_owned());
    run_session(
        Arc::clone(&runner),
        route.clone(),
        first,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let stem = dev_stem();
    let path = journal_path(journal_dir.path(), &stem);
    let written = fs::metadata(&path).unwrap().ino();
    let journal = runner.journal.as_ref().unwrap();
    journal.rewrite_records(&stem, |_| true).unwrap();
    let renamed = fs::metadata(&path).unwrap().ino();
    assert_ne!(written, renamed);
    let mut second = message("second question");
    second.message_id = MessageId::Native("0123456789abcdef0123456789abcdef-2-2".to_owned());
    run_session(
        Arc::clone(&runner),
        route,
        second,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["first answer", "second answer"]);
    assert_eq!(
        models.prompt(1)[2..],
        transcript(&[
            ("user", "[gateway: unmapped participant]\nfirst question"),
            ("assistant", "first answer"),
            ("user", "[gateway: unmapped participant]\nsecond question"),
        ])
    );
    let files: Vec<u64> = models.prompts.lock()[1]
        .iter()
        .filter_map(|message| match message {
            ModelMessage::Journal { message } => Some(message),
            ModelMessage::System { .. }
            | ModelMessage::User { .. }
            | ModelMessage::Assistant { .. }
            | ModelMessage::ToolResults { .. } => None,
        })
        .flat_map(|message| match message {
            dekopon_model::replay::ReplayMessage::User(texts) => texts.clone(),
            dekopon_model::replay::ReplayMessage::Assistant { content, .. } => {
                content.iter().cloned().collect()
            }
            dekopon_model::replay::ReplayMessage::Tool { text, .. } => vec![text.clone()],
        })
        .filter_map(|text| match text {
            dekopon_model::text::Text::File { file, .. } => Some(file.metadata().unwrap().ino()),
            dekopon_model::text::Text::Owned(_) | dekopon_model::text::Text::Opaque(_) => None,
        })
        .collect();
    assert!(!files.is_empty());
    assert!(files.iter().all(|inode| *inode == renamed));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pin_is_appended_by_persist() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(3, &["cli-probe.upper"])).await;
    let pin = |representation: &str| {
        tool_call(
            "pin-call",
            "set_memory_pin",
            json!({"recordId": 1, "representationId": representation}),
        )
    };
    let models = ModelScript::new([
        answer("remembered"),
        pin("metadata"),
        answer("pinned"),
        pin("native"),
        answer("never delivered"),
    ]);
    let stem = dev_stem();
    let path = journal_path(journal_dir.path(), &stem);
    let lines_before_persist = Arc::new(Mutex::new(Vec::new()));
    let runner_slot = Arc::new(std::sync::OnceLock::<Arc<SessionRunner>>::new());
    let factory = {
        let (lines, path, runner_slot) = (
            Arc::clone(&lines_before_persist),
            path.clone(),
            Arc::clone(&runner_slot),
        );
        Hooked::new(&models, move |request| match request {
            2 => lines.lock().push(journal_lines(&path).len()),
            4 => {
                runner_slot
                    .get()
                    .expect("runner installed")
                    .gate
                    .cancel(&cancel(SUBJECT, CancelVia::NativeStop));
            }
            _ => {}
        })
    };
    let runner = journaled_runner(broker, Arc::new(factory), journal_dir.path());
    assert!(runner_slot.set(Arc::clone(&runner)).is_ok());
    let route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    for text in ["remember this", "pin it", "pin again and stop"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(text),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    assert_eq!(*lines_before_persist.lock(), [2]);
    let lines = journal_lines(&path);
    let pins: Vec<&Value> = lines.iter().filter_map(|line| line.get("p")).collect();
    assert_eq!(pins.len(), 1, "{lines:?}");
    assert_eq!(pins[0]["pin"], "metadata");
    assert_eq!(journal_records(&path).len(), 2);
    assert!(!driver.replies().contains(&"never delivered".to_owned()));
    let dropped = capture.events_where(
        "dekopon_gatewayd",
        &[("event", "gateway_memory_pin_dropped")],
    );
    assert_eq!(dropped.len(), 1);
    assert_eq!(
        dropped[0].field("memory.pin.reason"),
        Some("turn-not-persisted")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_remains_held_until_the_record_is_appended() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([answer("first answer"), answer("second answer")]);
    let journal_slot = Arc::new(std::sync::OnceLock::<Arc<crate::journal::Journal>>::new());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Arc::new(Mutex::new(Some(released)));
    let holder = Arc::new(Mutex::new(None));
    let factory = {
        let (journal_slot, released, holder) = (
            Arc::clone(&journal_slot),
            Arc::clone(&released),
            Arc::clone(&holder),
        );
        Hooked::new(&models, move |request| {
            if request != 0 {
                return;
            }
            let journal = Arc::clone(journal_slot.get().expect("journal installed"));
            let released = released.lock().take().expect("one hold");
            let (held, holding) = std::sync::mpsc::channel();
            *holder.lock() = Some(std::thread::spawn(move || {
                let _lock = journal.hold_io_for_test();
                held.send(()).unwrap();
                released.recv().unwrap();
            }));
            holding.recv().unwrap();
        })
    };
    let runner = journaled_runner(broker, Arc::new(factory), journal_dir.path());
    assert!(
        journal_slot
            .set(Arc::clone(runner.journal.as_ref().unwrap()))
            .is_ok()
    );
    let route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    let first = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        message("first question"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    until("turn 1 delivered while its append waits", || {
        driver.replies().len() == 1
    })
    .await;
    tokio::time::timeout(
        Duration::from_secs(10),
        run_session(
            Arc::clone(&runner),
            route,
            message("second question"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        ),
    )
    .await
    .expect("turn 2 is queued behind turn 1's admission");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(models.requests(), 1, "turn 2 has not started");
    let path = journal_path(journal_dir.path(), &dev_stem());
    assert!(!path.exists());
    release.send(()).unwrap();
    holder.lock().take().unwrap().join().unwrap();
    tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(driver.replies(), ["first answer", "second answer"]);
    assert_eq!(
        models.prompt(1)[2..],
        transcript(&[
            ("user", "[gateway: unmapped participant]\nfirst question"),
            ("assistant", "first answer"),
            ("user", "[gateway: unmapped participant]\nsecond question"),
        ])
    );
    assert_eq!(journal_records(&path).len(), 2);
}

pub(crate) struct Vendor {
    pub(crate) endpoint: String,
    pub(crate) bodies: Arc<Mutex<Vec<Value>>>,
    _server: tokio::task::JoinSet<()>,
}

pub(crate) async fn vendor(responses: Vec<String>) -> Vendor {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let Some((_uri, _headers, body)) = read_http_request_parts(&mut stream).await else {
                continue;
            };
            recorded
                .lock()
                .push(serde_json::from_str::<Value>(&body).expect("a JSON request body"));
            let body = responses
                .lock()
                .pop_front()
                .unwrap_or_else(|| router_answer("unscripted"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                continue;
            }
        }
    });
    Vendor {
        endpoint,
        bodies,
        _server: server,
    }
}

fn router_sse(chunk: Value) -> String {
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

pub(crate) fn router_answer(text: &str) -> String {
    router_sse(
        json!({"model":"vendor/model","choices":[{"delta":{"content":text},"finish_reason":"stop"}]}),
    )
}

fn router_reasoned_call(call: &str, command: &str, sentinel: &str) -> String {
    router_sse(json!({"model":"vendor/model","choices":[{"delta":{
        "reasoning_details":[
            {"index":0,"type":"reasoning.text","id":"r0","text":format!("plain-{sentinel}"),"signature":null},
            {"index":1,"type":"reasoning.encrypted","id":"r1","format":"opaque","data":format!("cipher-{sentinel}")}
        ],
        "tool_calls":[{"index":0,"id":call,"type":"function","function":{"name":"bash","arguments":json!({"command":command}).to_string()}}]
    },"finish_reason":"tool_calls"}]}))
}

type Gate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

struct Router {
    endpoint: String,
    client: std::sync::OnceLock<Arc<dekopon_model::inference::ModelClient>>,
    gate: Mutex<Option<Gate>>,
}

impl Router {
    fn new(vendor: &Vendor) -> Self {
        Self {
            endpoint: vendor.endpoint.clone(),
            client: std::sync::OnceLock::new(),
            gate: Mutex::new(None),
        }
    }
}

impl ModelFactory for Router {
    fn build_bound(
        &self,
        _model: &Arc<ModelConfig>,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        let client = self.client.get_or_init(|| {
            Arc::new(dekopon_model::inference::ModelClient::OpenRouter(
                dekopon_model::openrouter::OpenRouterClient::new(
                    "vendor/model",
                    "router-test-token".to_owned(),
                    Duration::from_secs(10),
                    Default::default(),
                    &[],
                )
                .unwrap()
                .with_loopback_endpoint(&self.endpoint)
                .unwrap(),
            ))
        });
        Ok(Arc::new(GatedModel {
            inner: Arc::new(dekopon_model::blocking::BlockingModel::new(
                Arc::clone(client),
                runtime,
                cancel,
                Duration::from_secs(10),
            )),
            gate: Mutex::new(self.gate.lock().take()),
        }))
    }
}

struct GatedModel {
    inner: SharedModel,
    gate: Mutex<Option<Gate>>,
}

impl ChatModel for GatedModel {
    fn model_key(&self) -> dekopon_model::model::ModelKey {
        self.inner.model_key()
    }

    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        if let Some((entered, release)) = self.gate.lock().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(30)).unwrap();
        }
        self.inner.complete(messages, tools, options, on_event)
    }
}

fn router_route(window: MemoryWindow) -> crate::routes::BoundRoute {
    persistent_route(
        serde_json::from_value(json!({"kind":"openrouter","name":"router","model":"vendor/model","apiKeyEnv":"DEKOPON_ROUTER_TEST_KEY","timeoutMs":10_000}))
            .unwrap(),
        window,
    )
}

fn reasoning_details(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message.get("reasoning_details").is_some())
        .map(|message| message["reasoning_details"].clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn opaque_replays_as_an_object_and_never_reaches_the_trace() {
    const SENTINEL: &str = "opaque-sentinel-7f3a";
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let vendor = vendor(vec![
        router_reasoned_call("call-1", "printf ran", SENTINEL),
        router_answer("first done"),
        router_answer("second done"),
    ])
    .await;
    let runner = journaled_runner(broker, Arc::new(Router::new(&vendor)), journal_dir.path());
    let route = router_route(recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    for text in ["first", "second"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(text),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    assert_eq!(driver.replies(), ["first done", "second done"]);
    let bodies = vendor.bodies.lock().clone();
    assert_eq!(bodies.len(), 3);
    let expected = json!([
        {"type":"reasoning.text","id":"r0","text":format!("plain-{SENTINEL}"),"signature":null,"index":0},
        {"type":"reasoning.encrypted","id":"r1","format":"opaque","data":format!("cipher-{SENTINEL}"),"index":1}
    ]);
    for (step, body) in [("in-RAM step 2", &bodies[1]), ("stored record", &bodies[2])] {
        let details = reasoning_details(body);
        assert_eq!(details.len(), 1, "{step}: {body}");
        assert!(details[0].is_array(), "{step}: {}", details[0]);
        let mut actual = details[0].clone();
        let mut wanted = expected.clone();
        for list in [&mut actual, &mut wanted] {
            for item in list.as_array_mut().unwrap() {
                item.as_object_mut().unwrap().remove("index");
            }
        }
        assert_eq!(actual, wanted, "{step}");
    }
    assert!(
        fs::read_to_string(journal_path(journal_dir.path(), &dev_stem()))
            .unwrap()
            .contains(SENTINEL)
    );
    assert!(!capture.text().contains(SENTINEL));
    assert!(!capture.events_text().contains(SENTINEL));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_gateway_replays_without_a_continuation_mismatch() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let vendor = vendor(vec![
        router_reasoned_call("call-1", "printf ran", "restart"),
        router_answer("before restart"),
        router_answer("after restart"),
    ])
    .await;
    let route = router_route(recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    for text in ["first", "second"] {
        let restarted = journaled_runner(
            broker.clone(),
            Arc::new(Router::new(&vendor)),
            journal_dir.path(),
        );
        run_session(
            restarted,
            route.clone(),
            message(text),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    assert_eq!(driver.replies(), ["before restart", "after restart"]);
    let bodies = vendor.bodies.lock().clone();
    assert_eq!(bodies.len(), 3);
    let replayed = bodies[2]["messages"].as_array().unwrap();
    assert!(
        replayed
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call-1")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn journal_recall_keeps_arrivals_beside_their_messages_and_other_recall_keeps_only_exchanges()
{
    for recall in [
        RecallSource::Journal,
        RecallSource::Platform,
        RecallSource::None,
    ] {
        let directory = temporary();
        let journal_dir = temporary();
        let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
        let vendor = vendor(vec![
            router_reasoned_call("call-1", "printf exchange-sentinel", "reasoning-sentinel"),
            router_answer("answer-sentinel"),
        ])
        .await;
        let router = Router::new(&vendor);
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        *router.gate.lock() = Some((entered, released));
        let runner = journaled_runner(broker, Arc::new(router), journal_dir.path());
        let mut route = router_route(recall_window(recall));
        route.steering = crate::config::Steering::Boundary;
        let driver = HistoryDriver::new(Ok(Vec::new()));
        let held = tokio::spawn(run_session(
            Arc::clone(&runner),
            route.clone(),
            {
                let mut inbound = message("user-sentinel");
                inbound.assets.push(pending("first.png", "image/png", 3));
                inbound
            },
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        ));
        tokio::task::spawn_blocking(move || entering.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the turn reached its model");
        run_session(
            Arc::clone(&runner),
            route,
            {
                let mut inbound = message("steer-sentinel");
                inbound
                    .assets
                    .push(pending("second.pdf", "application/pdf", 3));
                inbound
            },
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), held)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(driver.inner.replies(), ["answer-sentinel"], "{recall:?}");
        let bodies = vendor.bodies.lock().clone();
        assert!(
            bodies[1].to_string().contains("steer-sentinel"),
            "{recall:?}"
        );
        let path = journal_path(journal_dir.path(), &dev_stem());
        let bytes = fs::read_to_string(&path).unwrap();
        assert!(bytes.contains("exchange-sentinel"), "{recall:?}");
        let records = journal_records(&path);
        if recall == RecallSource::Journal {
            let head = &records[0].0;
            let body = &records[0].1;
            let arrived = head["arrived"].as_array().unwrap();
            assert_eq!(arrived.len(), 2);
            for (asset, message_kind) in arrived.iter().zip(["user", "steer"]) {
                let item = asset["item"].as_u64().unwrap() as usize;
                assert!(body[item - 1].get(message_kind).is_some());
                assert_eq!(body[item]["asset"], asset["number"]);
                assert_eq!(asset["summaries"], json!([]));
            }
            continue;
        }
        assert!(records[0].0.get("arrived").is_none());
        for sentinel in [
            "user-sentinel",
            "steer-sentinel",
            "answer-sentinel",
            "plain-reasoning-sentinel",
            "cipher-reasoning-sentinel",
        ] {
            assert!(!bytes.contains(sentinel), "{recall:?}: {sentinel}");
        }
        let records = journal_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(body_kinds(&records[0].1), ["exchange"]);
        assert!(records[0].0["plan"].is_null());
        assert_eq!(records[0].0["summaries"], json!([]));
    }
}

fn follow_capability(action: &str) -> AvailableCapability {
    serde_json::from_value(json!({
        "provider": "dekopon-provider-follow",
        "capability": {
            "id": format!("dekopon-provider-follow.{action}"),
            "description": "Follow the conversation",
            "effect": "local-write", "risk": "Low",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }
    }))
    .expect("test-only provider declaration")
}

fn stay_responses() -> Vec<ResponseEnvelope> {
    vec![
        ResponseEnvelope::capabilities(
            vec![follow_capability("stay")],
            vec!["follow".into()],
            BTreeMap::new(),
        ),
        ResponseEnvelope::command_run(
            serde_json::from_value(json!({
                "outcome": "proposed", "capability": "dekopon-provider-follow.stay", "input": {}
            }))
            .unwrap(),
        ),
        ResponseEnvelope::invocation(record_success(), Vec::new(), Vec::new(), Vec::new()),
    ]
}

fn followed(
    broker: ResolvedBroker,
    models: Arc<dyn ModelFactory>,
    journal: &Path,
) -> (Arc<SessionRunner>, Arc<crate::follow::FollowTable>) {
    let mut runner = Arc::into_inner(journaled_runner(broker, models, journal)).unwrap();
    let table = Arc::new(
        crate::follow::FollowTable::load(Arc::clone(runner.journal.as_ref().unwrap())).unwrap(),
    );
    runner.follow = Some(Arc::clone(&table));
    (Arc::new(runner), table)
}

fn follow_route() -> crate::routes::BoundRoute {
    let mut route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    route.follow = true;
    route
}

fn dev_follow_key(route: &crate::routes::BoundRoute) -> String {
    crate::follow::follow_key(
        &route.transport,
        route.agent.as_str(),
        &route.conversation,
        &message("").conversation.key(),
    )
}

fn marker_path(journal: &Path, key: &str) -> PathBuf {
    journal.join(format!("{key}.followed"))
}

struct Held {
    release: std::sync::mpsc::Sender<()>,
    holder: std::thread::JoinHandle<()>,
}

fn hold_journal(journal: &Arc<crate::journal::Journal>) -> Held {
    let (held, holding) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let journal = Arc::clone(journal);
    let holder = std::thread::spawn(move || {
        let _lock = journal.hold_io_for_test();
        held.send(()).unwrap();
        released.recv().unwrap();
    });
    holding.recv().unwrap();
    Held { release, holder }
}

impl Held {
    fn release(self) {
        self.release.send(()).unwrap();
        self.holder.join().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_left_mid_turn_is_not_revived() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), stay_responses()).await;
    let models = ModelScript::new([script_call("follow stay"), answer("staying")]);
    let route = follow_route();
    let key = dev_follow_key(&route);
    let table_slot = Arc::new(std::sync::OnceLock::<Arc<crate::follow::FollowTable>>::new());
    let invited = Arc::new(AtomicBool::new(false));
    let factory = {
        let (table_slot, key, invited) =
            (Arc::clone(&table_slot), key.clone(), Arc::clone(&invited));
        Hooked::new(&models, move |request| {
            if request == 1 {
                let table = table_slot.get().expect("table installed");
                invited.store(
                    matches!(
                        table.ticket(&key, SystemTime::now()),
                        crate::follow::FollowTicket::Active { .. }
                    ),
                    Ordering::SeqCst,
                );
                table.leave(&key).unwrap();
            }
        })
    };
    let (runner, table) = followed(broker, Arc::new(factory), journal_dir.path());
    assert!(table_slot.set(Arc::clone(&table)).is_ok());
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        runner,
        route,
        message("please stay"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["staying"]);
    assert!(invited.load(Ordering::SeqCst), "the turn invited a follow");
    assert_eq!(
        table.ticket(&key, SystemTime::now()),
        crate::follow::FollowTicket::Absent
    );
    assert!(!marker_path(journal_dir.path(), &key).exists());
    let reloaded = crate::follow::FollowTable::load(Arc::new(
        crate::journal::Journal::open(
            journal_dir.path(),
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        )
        .unwrap(),
    ))
    .unwrap();
    assert_eq!(
        reloaded.ticket(&key, SystemTime::now()),
        crate::follow::FollowTicket::Absent
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_journal_write_precedes_delivery() {
    let route = follow_route();
    let key = dev_follow_key(&route);
    let stem = dev_stem();

    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), stay_responses()).await;
    let models = ModelScript::new([
        script_call("follow stay"),
        tool_call(
            "pin-call",
            "set_memory_pin",
            json!({"recordId": 1, "representationId": "metadata"}),
        ),
        answer("invited and pinned"),
    ]);
    let (runner, _table) = followed(broker, Arc::new(Arc::clone(&models)), journal_dir.path());
    let journal = Arc::clone(runner.journal.as_ref().unwrap());
    append_captured(
        &journal,
        &stem,
        &["dekopon-provider-follow.stay"],
        &captured("earlier", Some("earlier answer")),
    );
    let path = journal_path(journal_dir.path(), &stem);
    let before = fs::read(&path).unwrap();
    let held = hold_journal(&journal);
    let driver = Arc::new(RecordingDriver::default());
    let session = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        message("stay and pin"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    until("the invite-and-pin turn delivered", || {
        driver.replies().len() == 1
    })
    .await;
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!marker_path(journal_dir.path(), &key).exists());
    held.release();
    session.await.unwrap();
    assert_eq!(driver.replies(), ["invited and pinned"]);
    assert_eq!(journal_records(&path).len(), 2);
    assert!(
        journal_lines(&path)
            .iter()
            .any(|line| line.get("p").is_some())
    );
    assert!(marker_path(journal_dir.path(), &key).exists());

    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let model = InterruptibleModel::new([]);
    let runner = journaled_runner(broker, Arc::new(Arc::clone(&model)), journal_dir.path());
    let held = hold_journal(runner.journal.as_ref().unwrap());
    let mut stopped = persistent_route(model_config(), recall_window(RecallSource::Journal));
    stopped.max_duration = Some(Duration::from_millis(300));
    let driver = Arc::new(RecordingDriver::default());
    let session = tokio::spawn(run_session(
        Arc::clone(&runner),
        stopped,
        message("take too long"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    until("the wall-clock stop delivered", || {
        driver.replies().len() == 1
    })
    .await;
    let seals = |dir: &Path| {
        fs::read_dir(dir)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "sealed")
            })
            .count()
    };
    assert_eq!(seals(journal_dir.path()), 0);
    held.release();
    session.await.unwrap();
    assert_eq!(model.interrupted.load(Ordering::SeqCst), 1);
    assert_eq!(seals(journal_dir.path()), 1);

    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), stay_responses()).await;
    let models = ModelScript::new([script_call("follow stay"), answer("never")]);
    let factory = Hooked::new(&models, |request| {
        assert_ne!(request, 1, "the session task fails after the invite");
    });
    let (runner, _table) = followed(broker, Arc::new(factory), journal_dir.path());
    let held = hold_journal(runner.journal.as_ref().unwrap());
    let driver = Arc::new(RecordingDriver::default());
    let session = tokio::spawn(run_session(
        Arc::clone(&runner),
        route,
        message("stay then fail"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    until("the task failure delivered", || driver.replies().len() == 1).await;
    assert!(!marker_path(journal_dir.path(), &key).exists());
    held.release();
    session.await.unwrap();
    assert!(marker_path(journal_dir.path(), &key).exists());
    assert!(!journal_path(journal_dir.path(), &stem).exists());
}

struct ChatFiles;

impl AssetFetcher for ChatFiles {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        _limit: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        let AssetSourceRef::Slack { file_id, .. } = source else {
            panic!("the fixture assets come from Slack");
        };
        let bytes = match file_id.as_str() {
            "F-chart.png" => b"\x89PNG\r\n\x1a\nchart-pixels".to_vec(),
            "F-report.pdf" => b"%PDF-1.4 report-bytes".to_vec(),
            _ => b"text-asset-content".to_vec(),
        };
        Box::pin(async move { Ok(bytes) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_image_fetch_never_promises_a_following_message() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let fetch = |id: u64| {
        tool_call(
            &format!("fetch-{id}"),
            "fetch_chat_asset",
            json!({"id": id}),
        )
    };
    let models = ModelScript::new([fetch(1), fetch(2), fetch(3), answer("all seen")]);
    let mut model = model_config();
    if let ModelConfig::OpenaiCompatible { modalities, .. } = &mut model {
        *modalities = vec![crate::config::Modality::Image];
    }
    let route = persistent_route(model, recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    let mut inbound = message("look at these");
    inbound.assets = vec![
        pending("chart.png", "image/png", 20),
        pending("report.pdf", "application/pdf", 21),
        pending("notes.txt", "text/plain", 18),
    ];
    let with_files = |runner: Arc<SessionRunner>| {
        let mut runner = Arc::into_inner(runner).unwrap();
        runner.asset_fetchers.insert(
            "dev".to_owned(),
            Arc::new(ChatFiles) as Arc<dyn AssetFetcher>,
        );
        Arc::new(runner)
    };
    run_session(
        with_files(journaled_runner(
            broker.clone(),
            Arc::new(Arc::clone(&models)),
            journal_dir.path(),
        )),
        route.clone(),
        inbound,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["all seen"]);
    let live_text_result = tool_message(&models, 3);
    assert!(live_text_result.contains("text-asset-content"));
    assert!(
        models
            .prompt(1)
            .iter()
            .any(|(_, text)| text.contains("follows in the next message")),
        "the live turn shows the image in a following message"
    );

    let replay = ModelScript::new([answer("remembered")]);
    run_session(
        with_files(journaled_runner(
            broker,
            Arc::new(Arc::clone(&replay)),
            journal_dir.path(),
        )),
        route,
        message("what did you see?"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["all seen", "remembered"]);
    let replayed = replay.prompt(0);
    let results: Vec<&str> = replayed
        .iter()
        .filter(|(role, _)| role == "tool")
        .map(|(_, text)| text.as_str())
        .collect();
    assert_eq!(
        results,
        [
            "chat-asset:1 was shown when it was fetched and is not replayed; call fetch_chat_asset to see it again.",
            "chat-asset:2 was shown when it was fetched and is not replayed; call fetch_chat_asset to see it again.",
            live_text_result.as_str(),
        ]
    );
    assert!(
        replayed
            .iter()
            .all(|(_, text)| !text.contains("follows in the next message"))
    );
    assert!(
        replay.prompts.lock()[0]
            .iter()
            .all(|message| message.parts().is_none())
    );
}

fn exchange(
    id: &str,
    tool: dekopon_agent::memory::Tool,
    arguments: &str,
    result: &str,
) -> dekopon_agent::memory::Item {
    use dekopon_agent::memory::{Call, Exchange, Item, Kept, Outcome, ToolResult};
    Item::Exchange(Exchange {
        call: Call {
            id: id.into(),
            tool,
            arguments: Kept::new(arguments, usize::MAX),
        },
        result: ToolResult {
            outcome: Outcome::Exit(0),
            text: Kept::new(result, usize::MAX),
        },
    })
}

fn call_pairs(body: &Value) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let mut calls = BTreeMap::new();
    let mut results = BTreeMap::new();
    for message in body["messages"].as_array().unwrap() {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            *calls
                .entry(call["id"].as_str().unwrap().to_owned())
                .or_insert(0) += 1;
            assert!(
                serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap())
                    .is_ok()
            );
        }
        if message["role"] == "tool" {
            *results
                .entry(message["tool_call_id"].as_str().unwrap().to_owned())
                .or_insert(0) += 1;
        }
    }
    (calls, results)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_degrade_never_orphans_a_call() {
    use dekopon_agent::memory::{Item, Kept, Tool};
    let journal_dir = temporary();
    let journal = crate::journal::Journal::open(
        journal_dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
    )
    .unwrap();
    let stem = dev_stem();
    let grant = crate::memory::record::GrantDigest::of(&granted(&["cli-probe.upper"]));
    let key = dekopon_model::model::ModelKey::new(
        dekopon_model::model::Backend::OpenRouter,
        "vendor/model",
    );
    for turn in 0..4 {
        let original = vec![
            Item::User(Kept::new(
                &format!("question {turn} {}", "q".repeat(300)),
                usize::MAX,
            )),
            exchange(
                &format!("bash-{turn}"),
                Tool::Bash,
                &json!({"command": format!("printf {turn}")}).to_string(),
                &"r".repeat(600),
            ),
            exchange(
                &format!("wake-{turn}"),
                Tool::Wake,
                &json!({"in": "1h"}).to_string(),
                "scheduled",
            ),
            exchange(
                &format!("rejected-{turn}"),
                Tool::Rejected(Kept::new("mystery_tool", usize::MAX)),
                "not an object",
                "unknown tool",
            ),
            Item::Assistant(Kept::new(&format!("answer {turn}"), usize::MAX)),
        ];
        journal
            .append(
                &stem,
                &crate::journal::Entry {
                    at: SystemTime::now(),
                    grant: &grant,
                    captured: &dekopon_agent::prompt::CapturedTurn {
                        original,
                        model: key.clone(),
                    },
                    keep: crate::journal::Keep::Everything,
                    plan: None,
                    attachments: Default::default(),
                    arrivals: &[],
                    inventory: &[],
                    next_asset_id: 1,
                },
            )
            .unwrap()
            .unwrap();
    }
    let vendor = vendor(Vec::new()).await;
    let client = Arc::new(dekopon_model::inference::ModelClient::OpenRouter(
        dekopon_model::openrouter::OpenRouterClient::new(
            "vendor/model",
            "router-test-token".to_owned(),
            Duration::from_secs(10),
            Default::default(),
            &[],
        )
        .unwrap()
        .with_loopback_endpoint(&vendor.endpoint)
        .unwrap(),
    ));
    let snapshot = journal.record_snapshot(&stem, false).unwrap().unwrap();
    let (_cancel, cancelled) = tokio::sync::watch::channel(false);
    let model = Arc::new(dekopon_model::blocking::BlockingModel::new(
        Arc::clone(&client),
        tokio::runtime::Handle::current(),
        cancelled,
        Duration::from_secs(10),
    ));
    let mut budget = 64 * 1024;
    let mut sent = 0;
    loop {
        let projected = crate::memory::project::project(&crate::memory::project::ProjectionInput {
            file: &snapshot.file,
            records: &snapshot.records,
            prior: &[],
            hot_turns: 2,
            max_turns: 12,
            max_bytes: budget,
            model: &key,
            assets: None,
        })
        .unwrap();
        let mut messages = vec![ModelMessage::system("Answer briefly.")];
        messages.extend(projected.messages);
        messages.push(ModelMessage::user("next question"));
        let model = Arc::clone(&model);
        tokio::task::spawn_blocking(move || {
            model.complete(&messages, &[], &CompletionOptions::default(), &mut |_| {
                ControlFlow::Continue(())
            })
        })
        .await
        .unwrap()
        .expect("the vendor accepts the encoded body");
        sent += 1;
        let body = vendor.bodies.lock()[sent - 1].clone();
        let (calls, results) = call_pairs(&body);
        assert_eq!(calls, results, "budget {budget}: {body}");
        assert!(calls.values().all(|count| *count == 1), "budget {budget}");
        if budget == 1 {
            break;
        }
        budget = (budget / 2).max(1);
    }
    assert!(sent > 10);
}

#[test]
fn a_narrowed_grant_or_a_seal_stops_admission_past_side_lines() {
    use dekopon_agent::memory_tools::{MemoryRefusal, MemoryTools as _, Rung};
    let journal_dir = temporary();
    let journal = crate::journal::Journal::open(
        journal_dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
    )
    .unwrap();
    let stem = dev_stem();
    let wide = crate::memory::record::GrantDigest::of(&granted(&["cli-probe.upper", "extra.cap"]));
    let narrow = crate::memory::record::GrantDigest::of(&granted(&["cli-probe.upper"]));
    let append = |grant: &crate::memory::record::GrantDigest, text: &str| {
        journal
            .append(
                &stem,
                &crate::journal::Entry {
                    at: SystemTime::now(),
                    grant,
                    captured: &captured(text, Some("answer")),
                    keep: crate::journal::Keep::Everything,
                    plan: None,
                    attachments: Default::default(),
                    arrivals: &[],
                    inventory: &[],
                    next_asset_id: 1,
                },
            )
            .unwrap()
            .unwrap()
            .id
    };
    let side_lines = |record: crate::memory::record::RecordId| {
        journal
            .write_pin(
                &stem,
                crate::memory::record::PinLine {
                    at_ms: 1,
                    record,
                    pin: Some(Rung::Metadata),
                    subject: crate::memory::record::Subject::Record,
                },
            )
            .unwrap();
        journal
            .write_choice(
                &stem,
                crate::memory::record::ChoiceLine {
                    at_ms: 1,
                    record,
                    choice: Rung::Metadata,
                },
            )
            .unwrap();
    };
    let recalled = |grant: &crate::memory::record::GrantDigest| {
        journal
            .recall(
                &stem,
                grant,
                recall_window(RecallSource::Journal),
                SystemTime::now(),
            )
            .unwrap()
    };
    let tools = |grant: &crate::memory::record::GrantDigest, sealed_at_ms: Option<u64>| {
        crate::memory::journal::JournalMemoryTools::new(
            &journal,
            stem.clone(),
            crate::memory::journal::tools::Admission {
                grant: grant.clone(),
                horizon_ms: 0,
                sealed_at_ms,
            },
        )
    };
    let wide_record = append(&wide, "wide-canary");
    side_lines(wide_record);
    let narrow_record = append(&narrow, "narrow");
    side_lines(narrow_record);
    let after_narrowing = recalled(&narrow);
    let records = &after_narrowing.snapshot.as_ref().unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].header.id, narrow_record);
    assert_eq!(records[0].header.pin, Some(Rung::Metadata));
    assert!(after_narrowing.grant_boundary);
    let narrowed = tools(&narrow, None);
    assert!(matches!(
        narrowed.read(
            dekopon_agent::memory_tools::Target::Record(wide_record.0.get()),
            "native",
            None
        ),
        Err(MemoryRefusal::RecordIneligible)
    ));
    assert!(matches!(
        narrowed.set_pin(
            dekopon_agent::memory_tools::Target::Record(wide_record.0.get()),
            "metadata"
        ),
        Err(MemoryRefusal::RecordIneligible)
    ));
    assert!(
        narrowed
            .read(
                dekopon_agent::memory_tools::Target::Record(narrow_record.0.get()),
                "native",
                None
            )
            .is_ok()
    );
    assert!(narrowed.into_pins().is_empty());

    let sealed_at = SystemTime::now();
    journal.seal(&stem, sealed_at).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let resumed = append(&narrow, "after the seal");
    side_lines(resumed);
    let after_seal = recalled(&narrow);
    let records = &after_seal.snapshot.as_ref().unwrap().records;
    assert_eq!(
        records
            .iter()
            .map(|head| head.header.id)
            .collect::<Vec<_>>(),
        [resumed]
    );
    assert!(!after_seal.grant_boundary);
    let sealed = tools(&narrow, Some(crate::journal::millis(sealed_at)));
    assert!(matches!(
        sealed.read(
            dekopon_agent::memory_tools::Target::Record(narrow_record.0.get()),
            "native",
            None
        ),
        Err(MemoryRefusal::RecordIneligible)
    ));
    assert!(matches!(
        sealed.set_pin(
            dekopon_agent::memory_tools::Target::Record(narrow_record.0.get()),
            "native"
        ),
        Err(MemoryRefusal::RecordIneligible)
    ));
    assert!(
        sealed
            .read(
                dekopon_agent::memory_tools::Target::Record(resumed.0.get()),
                "native",
                None
            )
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn asset_summaries_go_only_to_the_summaries_model() {
    use crate::{
        asset::{AssetAccess, AssetFence},
        config::memory::{AttachmentSummaries, Toggle},
        memory::{
            generation::{Deferred, PostReply, drain},
            record::GrantDigest,
        },
    };
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, mut frames) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let reply = vendor(Vec::new()).await;
    let summary = vendor((0..4).map(|_| router_answer("description")).collect()).await;
    let decoy = vendor(Vec::new()).await;
    let mut runner = Arc::into_inner(journaled_runner(
        broker,
        Arc::new(crate::session::ConfiguredModels::default()),
        journal_dir.path(),
    ))
    .unwrap();
    let journal = Arc::clone(runner.journal.as_ref().unwrap());
    let counts = Arc::<Counts>::default();
    let (sender, dispatcher) = crate::memory::engine::channel(
        Arc::clone(&journal),
        Arc::clone(&runner.assets),
        Arc::clone(&counts),
        8.try_into().unwrap(),
        1.try_into().unwrap(),
    );
    let mut routes = RoutingTable::default();
    for (name, vendor) in [("reply", &reply), ("summary", &summary), ("decoy", &decoy)] {
        routes.models.insert(name.into(), Arc::new(serde_json::from_value(json!({"kind":"openaiCompatible","name":name,"endpoint":vendor.endpoint,"model":"fixture","modalities":["image"],"timeoutMs":60000})).unwrap()));
    }
    let mut owned = tokio::task::JoinSet::new();
    dispatcher.start(
        crate::gateway::CurrentGeneration::boot(Arc::new(routes), Arc::default()),
        Arc::new(ModelCache::new(Arc::new(
            crate::session::ConfiguredModels::default(),
        ))),
        Arc::clone(&runner.thread_owner),
        runner.shell_threads.clone(),
        &mut owned,
    );
    runner.summaries = Some(sender);
    let key = private_conversation_key("dev", "only-summary", SUBJECT);
    let stem = key.journal_stem();
    let grant = GrantDigest::of(&[]);
    let access = AssetAccess::persistent(
        key.clone(),
        1,
        Arc::new(AssetFence::new().with_grant(grant.hex())),
    );
    let registered = runner.assets.assets_for_access(
        &access,
        vec![pending("sender-name-canary.png", "image/png", 6)],
        Instant::now(),
    );
    drop(
        runner
            .assets
            .admit(&access, registered.arrived[0], b"pixels")
            .unwrap(),
    );
    let mut plan = crate::memory::generation::plan(&summarized_route(2)).unwrap();
    plan.parts = vec![64.try_into().unwrap(), 128.try_into().unwrap()];
    let post = |plan, arrivals, recalled| Deferred {
        record: Some(PostReply {
            key: key.clone(),
            granted: Vec::new(),
            access: access.clone(),
            publication: None,
            captured: captured("conversation-canary", Some("delivered-reply-canary")),
            arrivals,
            attachments: AttachmentSummaries {
                images: Toggle::Summarize,
                documents: Toggle::Summarize,
            },
            keep: crate::journal::Keep::Everything,
            plan,
            recalled,
        }),
        ..Deferred::default()
    };
    assert!(journal.record_snapshot(&stem, true).unwrap().is_none());
    drain(&runner, post(Some(plan), vec![registered.arrived], None)).await;
    for part in 0..2 {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let joined = counts.joined.notified();
                let snapshot = journal.record_snapshot(&stem, true).unwrap().unwrap();
                let head = &snapshot.records[0];
                if let Slot::Generated(_) =
                    &head.header.arrived[0].join(&head.asset_lines).summaries[part]
                {
                    break;
                }
                joined.await;
            }
        })
        .await
        .unwrap();
        runner.summaries.as_ref().unwrap().wait_assets().await;
        if part == 0 {
            let recalled = journal.record_snapshot(&stem, true).unwrap();
            drain(&runner, post(None, vec![Vec::new()], recalled)).await;
        }
    }
    let bodies = summary.bodies.lock().clone();
    assert_eq!(bodies.len(), 4);
    let files: Vec<_> = bodies
        .iter()
        .filter(|body| {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"].as_array().is_some())
        })
        .collect();
    assert_eq!(files.len(), 2);
    for (index, body) in files.iter().enumerate() {
        assert_asset_summary_request(body, index);
    }
    assert!(reply.bodies.lock().is_empty());
    assert!(decoy.bodies.lock().is_empty());
    assert!(frames.try_recv().is_err(), "summaries sent no broker frame");
    owned.shutdown().await;
}

fn assert_asset_summary_request(body: &Value, index: usize) {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    assert_eq!(body["model"], "fixture");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    let image = &messages[1]["content"][0];
    assert_eq!(image["type"], "image_url");
    let url = image["image_url"]["url"].as_str().unwrap();
    assert_eq!(
        STANDARD
            .decode(url.strip_prefix("data:image/png;base64,").unwrap())
            .unwrap(),
        b"pixels"
    );
    let text = messages[0]["content"].as_str().unwrap();
    assert_eq!(
        text.contains("Earlier, shorter descriptions:\ndescription"),
        index == 1
    );
    for canary in [
        "conversation-canary",
        "delivered-reply-canary",
        "sender-name-canary",
        "files.slack.com",
    ] {
        assert!(!body.to_string().contains(canary));
    }
}

#[tokio::test(start_paused = true)]
async fn a_native_pin_replays_the_file_and_shares_the_request_bound() {
    use crate::{
        asset::{MemoryAssets, retention_tests::NoScripts},
        memory::{
            engine::tests::assets::World,
            journal::{JournalMemoryTools, records::tests::fixture, tools::Admission},
            project::{ProjectionInput, project},
        },
    };
    use dekopon_agent::{
        memory_tools::{MemoryTools as _, Target},
        prompt::{SessionInputs, run_prompt_session},
    };
    let world = World::with_budget(64 * 1024 * 1024);
    let header = world.arrive(&["image/png"; 5], None, 1);
    let size = 36 * 1024 * 1024 / 5;
    for (index, asset) in header.arrived.iter().enumerate() {
        drop(
            world
                .store
                .admit(
                    &world.access,
                    asset.number.get(),
                    &vec![(index + 1) as u8; size],
                )
                .unwrap(),
        );
    }
    let tools = JournalMemoryTools::new(
        &world.journal,
        world.stem.clone(),
        Admission {
            grant: fixture().grant,
            horizon_ms: 0,
            sealed_at_ms: None,
        },
    )
    .with_assets(MemoryAssets {
        store: &world.store,
        access: &world.access,
        images_supported: true,
    });
    for asset in &header.arrived {
        tools
            .set_pin(Target::Asset(asset.number), "native")
            .unwrap();
    }
    for line in tools.into_pins() {
        assert!(world.journal.write_pin(&world.stem, line).unwrap());
    }
    world.arrive(&["application/pdf"], Some(&vec![9; 6 * 1024 * 1024]), 1);
    let snapshot = world.snapshot();
    let make_projection = |prior| {
        project(&ProjectionInput {
            file: &snapshot.file,
            records: &snapshot.records,
            prior,
            hot_turns: 1,
            max_turns: 4,
            max_bytes: 64 * 1024,
            model: &fixture().model,
            assets: Some(MemoryAssets {
                store: &world.store,
                access: &world.access,
                images_supported: true,
            }),
        })
        .unwrap()
    };
    let first = make_projection(&[]);
    assert_eq!(first.assets.native_bytes, (size * 5) as u64);
    assert_eq!(first.assets.rungs.native, 5);
    let models = ModelScript::new([
        tool_call("pdf", "fetch_chat_asset", json!({"id":6})),
        answer("done"),
        answer("again"),
    ]);
    models.capture_images.store(true, Ordering::Relaxed);
    let session_assets = SessionAssets::new(
        Arc::clone(&world.store),
        world.access.clone(),
        None,
        tokio::runtime::Handle::current(),
        true,
    );
    let current = tokio::runtime::Handle::current();
    let (_cancel, cancelled) = tokio::sync::watch::channel(false);
    let model = models
        .build_bound(&Arc::new(model_config()), current, cancelled)
        .unwrap();
    let prior = first.prior.clone();
    let runner = Arc::clone(&models);
    tokio::task::spawn_blocking(move || {
        run_prompt_session(
            model.as_ref(),
            &NoScripts,
            SessionInputs::new(
                "fetch the PDF",
                PromptLimits {
                    max_steps: 3,
                    max_capability_calls: 0,
                },
            )
            .with_assets(&session_assets)
            .with_native_asset_bytes(first.assets.native_bytes as usize),
            &first.messages,
        )
        .0
        .unwrap();
        let refused = tool_message(&runner, 1);
        assert!(refused.contains("the model request would exceed the 40 MiB decoded asset limit"));
    })
    .await
    .unwrap();
    let next = make_projection(&prior);
    assert_eq!(next.assets.native_bytes, (size * 5) as u64);
    let current = tokio::runtime::Handle::current();
    let (_cancel, cancelled) = tokio::sync::watch::channel(false);
    let model = models
        .build_bound(&Arc::new(model_config()), current, cancelled)
        .unwrap();
    let session_assets = SessionAssets::new(
        Arc::clone(&world.store),
        world.access.clone(),
        None,
        tokio::runtime::Handle::current(),
        true,
    );
    tokio::task::spawn_blocking(move || {
        run_prompt_session(
            model.as_ref(),
            &NoScripts,
            SessionInputs::new(
                "again",
                PromptLimits {
                    max_steps: 1,
                    max_capability_calls: 0,
                },
            )
            .with_assets(&session_assets)
            .with_native_asset_bytes(next.assets.native_bytes as usize),
            &next.messages,
        )
        .0
        .unwrap()
    })
    .await
    .unwrap();
    let images = models.image_bytes.lock();
    assert_eq!(images.len(), 15);
    assert_eq!(&images[..5], &images[10..]);
    assert_eq!(models.requests(), 3);
}
