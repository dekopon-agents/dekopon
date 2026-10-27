use super::*;

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

use crate::memory::{
    engine::{Counts, SlotTask},
    record::{Slot, SummaryFailure},
};

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
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap();
    runner
        .summaries
        .as_ref()
        .unwrap()
        .enqueue(SlotTask {
            stem: stem.clone(),
            record,
            part: std::num::NonZeroU8::MIN,
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
    let _ = observed.set(Arc::clone(&journal));
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
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap();
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
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap();
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
    {
        let journal = crate::journal::Journal::open(
            journal_dir.path(),
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        )
        .unwrap();
        append_captured(
            &journal,
            &stem,
            &["cli-probe.upper"],
            &captured("older-canary", Some("older")),
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut file, b"{broken\n").unwrap();
        append_captured(
            &journal,
            &stem,
            &["cli-probe.upper"],
            &captured("newer-canary", Some("newer")),
        );
    }
    let before = fs::read(&path).unwrap();
    let models = ModelScript::new([answer("fresh answer")]);
    let unchanged = Arc::new(AtomicBool::new(false));
    let factory = {
        let (unchanged, path, before) = (Arc::clone(&unchanged), path.clone(), before.clone());
        Hooked::new(&models, move |_| {
            unchanged.store(fs::read(&path).unwrap() == before, Ordering::SeqCst);
        })
    };
    let runner = journaled_runner(broker, Arc::new(factory), journal_dir.path());
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
            .all(|(_, text)| !text.contains("older-canary") && !text.contains("newer-canary")),
        "{:?}",
        models.prompt(0)
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
            _ => None,
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
    let _ = runner_slot.set(Arc::clone(&runner));
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
    let _ = journal_slot.set(Arc::clone(runner.journal.as_ref().unwrap()));
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
