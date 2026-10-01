use super::*;
use crate::jobs::{JobContext, JobOwner, Jobs};
use dekopon_broker_protocol::Trigger;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, ExitCode, JobId, JobOutcome, JobState, JobSummary,
    JobWait,
};

fn job_route(script_timeout: Duration, job_timeout: Duration) -> crate::routes::BoundRoute {
    crate::routes::BoundRoute {
        script_timeout,
        job_timeout: Some(job_timeout),
        ..route(model_config())
    }
}

fn runner_with_jobs(
    broker: ResolvedBroker,
    models: Arc<ModelScript>,
    max_concurrent: usize,
    max_jobs: usize,
) -> Arc<SessionRunner> {
    let mut runner = Arc::into_inner(runner(broker, models, max_concurrent))
        .expect("a fresh runner has one owner");
    runner.jobs = Arc::new(Jobs::new(max_jobs));
    Arc::new(runner)
}

async fn settled(jobs: &Jobs, owner: &JobOwner, within: Duration) -> Vec<JobSummary> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let rows = jobs.list(owner);
        let running = rows
            .iter()
            .any(|row| matches!(row.state, JobState::Running { .. }));
        if !running || tokio::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn finished(row: &JobSummary) -> (JobOutcome, ExitCode) {
    match row.state {
        JobState::Finished { outcome, exit, .. } => (outcome, exit),
        JobState::Running { .. } => panic!("job {} is still running", row.id),
    }
}

fn job_triggers(observed: &mut mpsc::UnboundedReceiver<RequestEnvelope>) -> Vec<Trigger> {
    let mut triggers = Vec::new();
    while let Ok(request) = observed.try_recv() {
        if let BrokerRequest::Capabilities {
            attestation: Some(Attestation {
                scope: Some(scope), ..
            }),
        } = request.request
        {
            triggers.push(scope.trigger);
        }
    }
    triggers
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_outlives_its_turn_and_the_turns_script_deadline() {
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([script_call("sleep 2 && echo done &"), answer("started")]);
    let runner = runner(broker, Arc::clone(&models), 1);
    let driver = Arc::new(RecordingDriver::default());
    let started = Instant::now();
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_millis(1_000), Duration::from_secs(10)),
        message("start it"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let turn_ended = started.elapsed();
    assert_eq!(driver.replies(), ["started"]);
    assert!(
        models
            .prompt(1)
            .iter()
            .any(|(role, text)| role == "tool" && text.contains("[1]")),
        "{:?}",
        models.prompt(1)
    );
    let owner = JobOwner::of(&message("start it"));
    let [row] = settled(&runner.jobs, &owner, Duration::from_secs(10))
        .await
        .try_into()
        .expect("one job");
    let ended = started.elapsed();
    assert_eq!(finished(&row), (JobOutcome::Succeeded, ExitCode::SUCCESS));
    assert_eq!(&*row.text, "sleep 2 && echo done");
    assert!(ended > turn_ended, "the job ended with its turn");
    assert!(ended > Duration::from_millis(1_000), "{ended:?}");
    assert_eq!(
        job_triggers(&mut observed),
        [Trigger::Message, Trigger::Job]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_finished_job_starts_a_notice_turn_with_its_output() {
    let directory = temporary();
    let routes = job_routes(directory.path(), Some(10_000)).await;
    let (broker, _observed) =
        stub_broker(directory.path(), listings(3, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 1 && echo finished &"),
        answer("started"),
        answer("noticed"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let driver = Arc::new(RecordingDriver::default());
    let drivers = Arc::new(BTreeMap::from([(
        "dev".to_owned(),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )]));
    let (sender, receiver) = mpsc::channel(4);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let service = tokio::spawn(crate::serve(
        Arc::clone(&runner),
        routes,
        Arc::new(BTreeMap::new()),
        drivers,
        Arc::new(Vec::new()),
        receiver,
        async move {
            stopped.await.ok();
        },
        Duration::from_secs(2),
        crate::collection::Collector::new(&[], 4),
    ));
    sender
        .send(TransportEvent::Message(Box::new(message("start"))))
        .await
        .unwrap();
    until(Duration::from_secs(10), || driver.replies().len() == 2).await;
    assert_eq!(
        driver.replies(),
        ["started", "noticed"],
        "requests={} prompt1={:?}",
        models.requests(),
        models.prompt(1)
    );
    let prompt = models.prompt(2);
    assert!(
        prompt.iter().any(|(role, text)| role == "user"
            && text.starts_with("[gateway: job 1 finished, exit 0, after")
            && text.contains("sleep 1 && echo finished\nfinished")),
        "{prompt:?}"
    );
    stop.send(()).unwrap();
    service.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_detached_job_records_its_start_root_finish_and_notice() {
    use opentelemetry::trace::{TraceId, TracerProvider as _};
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        trace::{SdkTracerProvider, SpanData, SpanExporter},
    };
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::prelude::*;

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
    let capture = dekopon_test_support::CaptureLayer::workspace();
    let subscriber = tracing::Dispatch::new(
        tracing_subscriber::registry()
            .with(capture.clone())
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("job-trace-test"))),
    );
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(3, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("echo finished &"),
        answer("started"),
        answer("noticed"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(5), Duration::from_secs(10)),
        message("start"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .with_subscriber(subscriber.clone())
    .await;
    let mut notices = runner.jobs.take_notices();
    let notice = tokio::time::timeout(Duration::from_secs(5), notices.recv())
        .await
        .expect("job finishes")
        .expect("notice arrives");
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(5), Duration::from_secs(10)),
        notice,
        driver as Arc<dyn ChatDriver>,
    )
    .with_subscriber(subscriber)
    .await;
    let records = capture.records();
    assert!(records.iter().any(|record| matches!(record, dekopon_test_support::Record::Span { name: "gateway.job", parent: None, fields } if fields.contains("job.id=1"))), "{records:?}");
    assert!(records.iter().any(|record| matches!(record, dekopon_test_support::Record::Span { name: "shell.script", parent: Some(parent), .. } if parent == "gateway.job")), "{records:?}");
    for (field, parent) in [
        ("job.deadline_ms=", Some("shell.script")),
        ("job.outcome=\"succeeded\"", Some("gateway.job")),
        ("job.notice.delivery=\"new-turn\"", Some("gateway.message")),
    ] {
        assert!(records.iter().any(|record| matches!(record, dekopon_test_support::Record::Event { fields, parent: actual, .. } if fields.contains("job.id=1") && fields.contains(field) && actual.as_deref() == parent)), "{field}: {records:?}");
    }
    let context = [
        format!("conversation.id={}", message("start").conversation.key()),
        format!("subject={}", subject().canonical()),
        "agent=".to_owned(),
    ];
    assert!(records.iter().any(|record| matches!(record, dekopon_test_support::Record::Event { fields, .. } if fields.contains("job.notice.delivery") && context.iter().all(|field| fields.contains(field.as_str())))), "{context:?}: {records:?}");

    provider.force_flush().unwrap();
    let spans = exported.0.lock().clone();
    provider.shutdown().unwrap();
    let in_trace = |trace: TraceId, name: &str| {
        spans
            .iter()
            .any(|span| span.span_context.trace_id() == trace && span.name == name)
    };
    let linked = |span: &SpanData| -> Vec<(TraceId, opentelemetry::trace::SpanId)> {
        span.links
            .links
            .iter()
            .map(|link| (link.span_context.trace_id(), link.span_context.span_id()))
            .collect()
    };
    let job = spans
        .iter()
        .find(|span| span.name == "gateway.job")
        .expect("gateway.job exported");
    let job_trace = job.span_context.trace_id();
    let [(starter_trace, starter_span)] = linked(job)[..] else {
        panic!("gateway.job links once to its starter: {:?}", job.links);
    };
    assert_ne!(starter_trace, job_trace, "{spans:#?}");
    assert!(
        spans
            .iter()
            .any(|span| span.span_context.span_id() == starter_span
                && span.span_context.trace_id() == starter_trace),
        "the job links to an exported span of its starter: {spans:#?}"
    );
    assert!(in_trace(starter_trace, "gateway.message"), "{spans:#?}");
    assert!(in_trace(job_trace, "shell.script"), "{spans:#?}");
    let notice = spans
        .iter()
        .find(|span| linked(span).iter().any(|(trace, _)| *trace == job_trace))
        .expect("the notice links to its job");
    let notice_trace = notice.span_context.trace_id();
    assert_ne!(notice_trace, job_trace, "{spans:#?}");
    assert_ne!(notice_trace, starter_trace, "{spans:#?}");
    assert!(in_trace(notice_trace, "gateway.message"), "{spans:#?}");
    assert!(
        linked(notice)
            .iter()
            .all(|(trace, span_id)| *trace == job_trace
                && spans
                    .iter()
                    .any(|span| span.span_context.span_id() == *span_id
                        && span.span_context.trace_id() == job_trace)),
        "the notice links to an exported span of the job: {spans:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_killed_or_stopped_job_sends_nothing_after_its_reply_and_keeps_its_outcome() {
    use tracing::instrument::WithSubscriber as _;
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let routes = job_routes(directory.path(), Some(3_600_000)).await;
    let (broker, _observed) =
        stub_broker(directory.path(), listings(6, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 3600 &"),
        answer("one started"),
        script_call("kill %1"),
        answer("killed"),
        script_call("sleep 3600 &"),
        answer("two started"),
        script_call("jobs"),
        answer("listed"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let driver = Arc::new(RecordingDriver::default());
    let drivers = Arc::new(BTreeMap::from([(
        "dev".to_owned(),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )]));
    let (sender, receiver) = mpsc::channel(4);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let service = tokio::spawn(
        crate::serve(
            Arc::clone(&runner),
            routes,
            Arc::new(BTreeMap::new()),
            drivers,
            Arc::new(vec!["stop".to_owned()]),
            receiver,
            async move {
                stopped.await.ok();
            },
            Duration::from_secs(2),
            crate::collection::Collector::new(&[], 4),
        )
        .with_current_subscriber(),
    );
    let owner = JobOwner::of(&message("start"));
    for (text, replies) in [
        ("start one", 1),
        ("kill it", 2),
        ("start two", 3),
        ("stop", 4),
    ] {
        sender
            .send(TransportEvent::Message(Box::new(message(text))))
            .await
            .unwrap();
        until(Duration::from_secs(10), || {
            driver.replies().len() == replies
        })
        .await;
    }
    let rows = settled(&runner.jobs, &owner, Duration::from_secs(5)).await;
    let outcomes = rows.iter().map(|row| finished(row).0).collect::<Vec<_>>();
    assert_eq!(outcomes, [JobOutcome::Killed, JobOutcome::Cancelled]);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        driver.replies(),
        [
            "one started",
            "killed",
            "two started",
            crate::session::STOPPED_REPLY
        ]
    );
    assert_eq!(models.requests(), 6);
    assert!(
        !capture.events_text().contains("job.notice"),
        "{}",
        capture.events_text()
    );
    sender
        .send(TransportEvent::Message(Box::new(message("list"))))
        .await
        .unwrap();
    until(Duration::from_secs(10), || driver.replies().len() == 5).await;
    let listing = tool_message(&models, 7);
    assert!(
        listing.contains("[1] killed") && listing.contains("[2] cancelled"),
        "{listing}"
    );
    stop.send(()).unwrap();
    service.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_with_a_large_output_is_bounded_to_sixteen_kibibytes() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call(
            "i=0; while [ $i -lt 1800 ]; do echo 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ; i=$((i+1)); done &",
        ),
        answer("started"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let mut notices = runner.jobs.take_notices();
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("start"),
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    let notice = tokio::time::timeout(Duration::from_secs(10), notices.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        notice
            .text
            .starts_with("[gateway: job 1 finished, exit 0, after")
    );
    assert_eq!(notice.text.len(), crate::transport::MAX_INBOUND_TEXT_BYTES);
}

fn notice_for(inbound: &InboundMessage, id: u64) -> InboundMessage {
    let anchor = crate::wake::Anchor::for_job(inbound, &route(model_config()).agent).unwrap();
    anchor.job_inbound(
        JobId::new(id),
        format!("[gateway: job {id} finished, exit 0, after 1s]\necho done\ndone"),
        None,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_notice_steers_without_aborting_and_keeps_its_words_out_of_recorded_user_text() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![
            memory_surface_response(),
            ResponseEnvelope::invocation(
                record_result(InvocationOutcome::Succeeded, None),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
        ],
    )
    .await;
    let models = InterruptibleModel::new(vec![answer("draft"), answer("finished")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = job_route(Duration::from_secs(10), Duration::from_secs(10));
    route.steering = crate::config::Steering::Abort;
    let held = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        message("person said this"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    models.wait_until_entered().await;
    run_session(
        Arc::clone(&runner),
        route,
        notice_for(&message("start"), 8),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(
        models.interrupted.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    models.release();
    held.await.unwrap();
    assert!(models.prompt(1).iter().any(|(role, text)| role == "user"
        && text.starts_with("[gateway: job 8 finished")
        && !text.contains("sent while you were working")));
    assert_eq!(driver.replies(), ["finished"]);
    assert!(matches!(
        observed.recv().await.expect("surface request").request,
        BrokerRequest::Capabilities { .. }
    ));
    let record = observed.recv().await.expect("the delivered turn's record");
    let BrokerRequest::RecordDeliveredTurn { turn, .. } = record.request else {
        panic!("expected a delivered turn: {record:?}");
    };
    assert_eq!(turn.user(), "person said this");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_notice_drops_without_a_busy_reply() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = BlockedModel::new("done");
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let held = tokio::spawn(run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("holding"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    models.wait_until_entered().await;
    let mut foreign = message("foreign");
    foreign.conversation.id = "different".to_owned();
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        notice_for(&foreign, 9),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(driver.replies().is_empty());
    let records = capture.events_text();
    assert_eq!(
        records.matches("job.notice.delivery=\"dropped\"").count(),
        1,
        "{records}"
    );
    models.release();
    held.await.unwrap();
    assert_eq!(driver.replies(), ["done"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_route_without_job_opt_in_drops_the_notice_once() {
    use tracing::instrument::WithSubscriber as _;
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let routes = job_routes(directory.path(), None).await;
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = ModelScript::new([]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 1);
    let driver = Arc::new(RecordingDriver::default());
    let drivers = Arc::new(BTreeMap::from([(
        "dev".to_owned(),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )]));
    let (_sender, receiver) = mpsc::channel(4);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let service = tokio::spawn(
        crate::serve(
            Arc::clone(&runner),
            routes,
            Arc::new(BTreeMap::new()),
            drivers,
            Arc::new(Vec::new()),
            receiver,
            async move {
                stopped.await.ok();
            },
            Duration::from_secs(2),
            crate::collection::Collector::new(&[], 4),
        )
        .with_current_subscriber(),
    );
    drop(admitted(&runner.jobs, "a", "job").unwrap());
    until(Duration::from_secs(2), || {
        capture
            .events_text()
            .contains("job.notice.delivery=\"dropped\"")
    })
    .await;
    assert_eq!(
        capture
            .events_text()
            .matches("job.notice.delivery=\"dropped\"")
            .count(),
        1
    );
    assert!(driver.replies().is_empty());
    assert_eq!(models.requests(), 0);
    stop.send(()).unwrap();
    service.await.unwrap();
}

#[test]
fn a_full_notice_channel_drops_once_without_blocking_job_completion() {
    let (capture, _guard) = capture_spans();
    let jobs = Arc::new(Jobs::new(1));
    drop(admitted(&jobs, "a", "first").unwrap());
    drop(admitted(&jobs, "a", "second").unwrap());
    let text = capture.events_text();
    assert_eq!(
        text.matches("job.notice.delivery=\"dropped\"").count(),
        1,
        "{text}"
    );
    assert_eq!(jobs.free_permits(), 1);
}

#[test]
fn leftover_notices_and_person_steers_become_separate_followups() {
    let gate = crate::session::SessionGate::new(1);
    let route = job_route(Duration::from_secs(10), Duration::from_secs(10));
    let original = message("person");
    let admission = match gate.admit(
        &route,
        original,
        crate::collection::Dispositions(Vec::new()),
    ) {
        crate::session::Admit::Admitted(admission, _, _) => admission,
        crate::session::Admit::Steered(_)
        | crate::session::Admit::Queued
        | crate::session::Admit::Full(..)
        | crate::session::Admit::Saturated(..) => panic!("original admitted"),
    };
    let notice = notice_for(&message("person"), 11);
    assert!(matches!(
        gate.admit(&route, notice, crate::collection::Dispositions(Vec::new())),
        crate::session::Admit::Steered(_)
    ));
    assert!(matches!(
        gate.admit(
            &route,
            message("person's steer"),
            crate::collection::Dispositions(Vec::new())
        ),
        crate::session::Admit::Steered(_)
    ));
    let (admission, first) = admission.next_or_release().expect("the notice follows");
    assert!(matches!(first.message.message_id, MessageId::Job { .. }));
    assert!(!first.message.text.contains("person's steer"));
    let (admission, second) = admission
        .next_or_release()
        .expect("the person follows separately");
    assert!(matches!(second.message.message_id, MessageId::Native(_)));
    assert!(second.message.text.contains("person's steer"));
    assert!(!second.message.text.contains("[gateway: job"));
    assert!(admission.next_or_release().is_none());
}

#[test]
fn another_persons_notice_queues_behind_the_running_turn() {
    let gate = crate::session::SessionGate::new(1);
    let route = job_route(Duration::from_secs(10), Duration::from_secs(10));
    let admission = match gate.admit(
        &route,
        from("a", "running"),
        crate::collection::Dispositions(Vec::new()),
    ) {
        crate::session::Admit::Admitted(admission, _, _) => admission,
        crate::session::Admit::Steered(_)
        | crate::session::Admit::Queued
        | crate::session::Admit::Full(..)
        | crate::session::Admit::Saturated(..) => panic!("first person admitted"),
    };
    let second = from("b", "starting job");
    let notice = notice_for(&second, 12);
    assert!(matches!(
        gate.admit(&route, notice, crate::collection::Dispositions(Vec::new())),
        crate::session::Admit::Queued
    ));
    let (_, followup) = admission
        .next_or_release()
        .expect("other person's notice follows");
    assert!(matches!(followup.message.message_id, MessageId::Job { .. }));
    assert_eq!(followup.message.subject, second.subject);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_past_max_jobs_fails_at_once_without_taking_a_session_permit() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 5 &\nsleep 5 &\necho status=$?"),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 1);
    let driver = Arc::new(RecordingDriver::default());
    let started = Instant::now();
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(30), Duration::from_secs(30)),
        message("two"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let tool = models
        .prompt(1)
        .into_iter()
        .find(|(role, _)| role == "tool")
        .map(|(_, text)| text)
        .expect("the script's output");
    assert!(
        tool.contains(
            "1 jobs are already running, the most this gateway allows (sessions.maxJobs)"
        ),
        "{tool}"
    );
    assert!(tool.contains("status=1"), "{tool}");
    assert!(
        runner
            .gate
            .admit_probe(("dev".to_owned(), "elsewhere".to_owned()))
            .is_some(),
        "a running job holds a session permit"
    );
    let owner = JobOwner::of(&message("two"));
    runner.jobs.kill(&owner, JobId::new(1)).expect("kill");
    let [row] = settled(&runner.jobs, &owner, Duration::from_secs(5))
        .await
        .try_into()
        .expect("one job");
    assert_eq!(finished(&row).0, JobOutcome::Killed);
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_returns_the_last_jobs_exit_and_bare_wait_covers_all_started_jobs() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(4, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 1 && false & sleep 1 & wait; echo status=$?; wait %1; echo first=$?"),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("wait"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let output = models
        .prompt(1)
        .into_iter()
        .find(|(role, _)| role == "tool")
        .unwrap()
        .1;
    assert!(output.contains("status=0"), "{output}");
    assert!(output.contains("first=1"), "{output}");
    assert_eq!(runner.jobs.free_permits(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_waited_while_running_sends_no_notice() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 1 && echo done & wait $!"),
        answer("waited"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 1);
    let mut notices = runner.jobs.take_notices();
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("wait"),
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(notices.try_recv().is_err());
    assert_eq!(
        finished(&runner.jobs.list(&JobOwner::of(&message("wait")))[0]).0,
        JobOutcome::Succeeded
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_wait_and_kill_restrict_rows_to_the_person_who_started_them() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(4, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 5 & jobs -p; kill -9 $!; wait $!; jobs"),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("control"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["done"]);
    let output = models
        .prompt(1)
        .into_iter()
        .find(|(role, _)| role == "tool")
        .unwrap()
        .1;
    assert!(output.contains("[1]\n1\n"), "{output}");
    assert!(output.contains("[1] killed exit"), "{output}");
    let owner = JobOwner::of(&message("control"));
    let mut foreign = message("control");
    foreign.subject = subject_named("foreign");
    let foreign = JobOwner::of(&foreign);
    assert!(runner.jobs.list(&foreign).is_empty());
    assert_eq!(
        runner.jobs.kill(&foreign, JobId::new(1)),
        Err(dekopon_shell::JobRefusal::NotYours)
    );
    assert_eq!(
        runner.jobs.wait(&foreign, JobId::new(1), &|| true),
        Err(dekopon_shell::JobRefusal::NotYours)
    );
    assert_eq!(finished(&runner.jobs.list(&owner)[0]).0, JobOutcome::Killed);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_persons_jobs_does_not_stop_someone_elses() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(4, &["cli-probe.upper"])).await;
    let models = ModelScript::new([script_call("sleep 5 &"), answer("done")]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("start"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let request = crate::transport::CancelRequest {
        transport: message("start").transport,
        conversation_id: message("start").conversation.key(),
        subject: message("start").subject.canonical(),
        via: dekopon_agent::CancelVia::NativeStop,
    };
    crate::cancel_session(&runner, &request);
    let rows = settled(
        &runner.jobs,
        &JobOwner::of(&message("start")),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(finished(&rows[0]).0, JobOutcome::Cancelled);
    assert_eq!(runner.jobs.free_permits(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_restarting_job_chain_ends_at_the_root_jobs_deadline() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(12, &["cli-probe.upper"])).await;
    let models = ModelScript::new([script_call("f() { sleep 1; f & }; f &"), answer("looping")]);
    let runner = runner(broker, Arc::clone(&models), 1);
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(30), Duration::from_millis(3_000)),
        message("loop"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let rows = runner.jobs.list(&JobOwner::of(&message("loop")));
    assert!(!rows.is_empty());
    for row in &rows {
        let (outcome, _) = finished(row);
        assert!(
            matches!(outcome, JobOutcome::Deadline | JobOutcome::Succeeded),
            "{row:?}"
        );
    }
    assert!(
        rows.iter()
            .any(|row| finished(row).0 == JobOutcome::Deadline),
        "{rows:?}"
    );
}

struct ProbeInvoker(JobContext);

impl CapabilityInvoker for ProbeInvoker {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn job_control(&self) -> Option<&dyn dekopon_shell::JobControl> {
        Some(&self.0)
    }

    fn invoke(&self, _: dekopon_shell::CommandProposal) -> CapabilityCallResult {
        CapabilityCallResult::NotFound
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_a_probe_starts_attests_as_a_probe() {
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let jobs = Arc::new(Jobs::new(2));
    let inbound = message("watch");
    let route = route(model_config());
    let anchor = crate::wake::Anchor::for_job(&inbound, &route.agent).expect("an anchor");
    let context = JobContext::for_turn(
        Arc::clone(&jobs),
        &inbound,
        anchor,
        broker,
        dekopon_shell::Limits::default(),
    )
    .for_probe();
    let outcome = tokio::task::spawn_blocking(move || {
        dekopon_shell::Interpreter::new(dekopon_shell::Limits::default())
            .run("true &", &ProbeInvoker(context))
    })
    .await
    .expect("the probe script joins");
    assert_eq!(outcome.output, "[1]");
    let [row] = settled(&jobs, &JobOwner::of(&inbound), Duration::from_secs(5))
        .await
        .try_into()
        .expect("one job");
    assert_eq!(finished(&row).0, JobOutcome::Succeeded);
    assert_eq!(job_triggers(&mut observed), [Trigger::Probe]);
}

fn subject_named(name: &str) -> ExternalSubject {
    let number = if name == "a" {
        "16030000001"
    } else {
        "16030000002"
    };
    ExternalSubject::telephone(number).expect("a telephone subject")
}

fn admitted(
    jobs: &Arc<Jobs>,
    subject_text: &str,
    text: &str,
) -> Result<crate::jobs::JobRun, dekopon_shell::JobRefusal> {
    let mut inbound = message(subject_text);
    inbound.subject = subject_named(subject_text);
    let anchor =
        crate::wake::Anchor::for_job(&inbound, &route(model_config()).agent).expect("an anchor");
    jobs.admit(JobOwner::of(&inbound), anchor, None, Arc::from(text))
        .map(|(run, _signal)| run)
}

fn owned_by(subject_text: &str) -> JobOwner {
    let mut inbound = message(subject_text);
    inbound.subject = subject_named(subject_text);
    JobOwner::of(&inbound)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_word_with_only_jobs_replies_stopped_and_cancels_only_its_owner() {
    let directory = temporary();
    let (runner, routes) = idle_routing_loop(directory.path()).await;
    let inbound = message("stop");
    let anchor = crate::wake::Anchor::for_job(&inbound, &route(model_config()).agent).unwrap();
    let (run, signal) = runner
        .jobs
        .admit(JobOwner::of(&inbound), anchor, None, Arc::from("sleep 5"))
        .unwrap();
    let other = admitted(&runner.jobs, "b", "sleep 5").expect("a second slot");
    let driver = Arc::new(RecordingDriver::default());
    let drivers = BTreeMap::from([("dev".to_owned(), Arc::clone(&driver) as Arc<dyn ChatDriver>)]);
    let mut sessions = tokio::task::JoinSet::new();
    crate::dispatch(
        &runner,
        &routes,
        &BTreeMap::new(),
        &drivers,
        &["stop".to_owned()],
        &mut sessions,
        &mut crate::collection::Collector::new(&[], 4),
        inbound,
    );
    assert!(signal.is_cancelled());
    while sessions.join_next().await.is_some() {}
    assert_eq!(driver.replies(), [crate::session::STOPPED_REPLY]);
    assert!(matches!(
        runner.jobs.list(&owned_by("b"))[0].state,
        JobState::Running { .. }
    ));
    drop((run, other));
}

#[test]
fn dropping_an_unfinished_run_fails_its_row_and_returns_the_permit() {
    let jobs = Arc::new(Jobs::new(1));
    let run = admitted(&jobs, "a", "sleep 9").expect("a free slot");
    assert_eq!(jobs.free_permits(), 0);
    drop(run);
    assert_eq!(jobs.free_permits(), 1);
    let [row] = jobs.list(&owned_by("a")).try_into().expect("one row");
    assert_eq!(finished(&row), (JobOutcome::Failed, ExitCode::FAILURE));
}

#[test]
fn a_full_table_evicts_its_oldest_finished_row_and_never_a_running_one() {
    let jobs = Arc::new(Jobs::new(2));
    let first = admitted(&jobs, "a", "a").expect("slot");
    let second = admitted(&jobs, "b", "b").expect("slot");
    assert!(admitted(&jobs, "a", "c").is_err());
    drop(first);
    let third = admitted(&jobs, "a", "c").expect("slot");
    let texts = |subject| {
        jobs.list(&owned_by(subject))
            .into_iter()
            .map(|row| row.text.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(texts("a"), ["c"]);
    assert_eq!(texts("b"), ["b"]);
    drop((second, third));
}

#[test]
fn a_finished_job_stays_listed_until_a_new_start_needs_its_slot() {
    let jobs = Arc::new(Jobs::new(2));
    let owner = owned_by("a");
    let a = admitted(&jobs, "a", "A").unwrap();
    drop(a);
    let b = admitted(&jobs, "a", "B").unwrap();
    let texts = || {
        jobs.list(&owner)
            .into_iter()
            .map(|row| row.text.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(texts(), ["A", "B"]);
    drop(b);
    let c = admitted(&jobs, "a", "C").unwrap();
    assert_eq!(texts(), ["B", "C"]);
    drop(c);
}

#[test]
fn a_panicking_wait_callback_releases_its_waiter_slot() {
    let jobs = Arc::new(Jobs::new(1));
    let run = admitted(&jobs, "a", "first").expect("slot");
    let owner = owned_by("a");
    let id = jobs.list(&owner)[0].id;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert!(jobs.wait(&owner, id, &|| panic!("caller panicked")).is_ok());
        }))
        .is_err()
    );
    drop(run);
    let next = admitted(&jobs, "a", "next").expect("waiter no longer pins the row");
    assert_eq!(jobs.list(&owner).len(), 1);
    drop(next);
}

#[test]
fn a_finished_job_keeps_its_row_while_a_wait_is_reading_it() {
    let jobs = Arc::new(Jobs::new(2));
    let waited = admitted(&jobs, "a", "waited").expect("slot");
    drop(admitted(&jobs, "a", "spare").expect("slot"));
    let owner = owned_by("a");
    let id = jobs.list(&owner)[0].id;
    let (parked, on_park) = std::sync::mpsc::channel();
    let (release, on_release) = std::sync::mpsc::channel::<()>();
    let (table, owner) = (&jobs, &owner);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(move || {
            table.wait(owner, id, &|| {
                parked.send(()).ok();
                on_release.recv().ok();
                true
            })
        });
        on_park.recv().expect("the wait parks");
        drop(waited);
        let next = admitted(&jobs, "a", "next").expect("the released slot");
        drop(release);
        assert_eq!(
            waiter.join().expect("the waiter"),
            Ok(JobWait::Exited(ExitCode::FAILURE))
        );
        drop(next);
    });
}

async fn until(within: Duration, mut reached: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + within;
    while !reached() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "not reached within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn from(subject_text: &str, text: &str) -> InboundMessage {
    let mut inbound = message(text);
    inbound.subject = subject_named(subject_text);
    inbound
}

fn running(rows: &[JobSummary]) -> bool {
    rows.iter()
        .any(|row| matches!(row.state, JobState::Running { .. }))
}

async fn job_routes(directory: &Path, job_timeout_ms: Option<u64>) -> Arc<RoutingTable> {
    let mut document = document(directory);
    if let Some(milliseconds) = job_timeout_ms {
        document["routes"][0]["limits"] = json!({ "jobTimeoutMs": milliseconds });
    }
    document["routes"][0]["wakes"] = json!(true);
    document["sessions"] = json!({ "wakes": { "path": directory.join("wakes.jsonl") } });
    let config = resolved(directory, &document).await;
    Arc::new(RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("route binds"))
}

async fn tool_output_of(
    runner: &Arc<SessionRunner>,
    models: &Arc<ModelScript>,
    route: crate::routes::BoundRoute,
    inbound: InboundMessage,
) -> String {
    let index = models.requests();
    run_session(
        Arc::clone(runner),
        route,
        inbound,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    tool_message(models, index + 1)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_word_from_an_unrouted_conversation_still_cancels_its_persons_jobs() {
    let directory = temporary();
    let (runner, routes) = idle_routing_loop(directory.path()).await;
    let mut inbound = message("stop");
    inbound.conversation.kind = ConversationKind::Channel;
    assert!(routes.route(&inbound).is_none(), "the fixture is unrouted");
    let anchor = crate::wake::Anchor::for_job(&inbound, &route(model_config()).agent).unwrap();
    let (run, signal) = runner
        .jobs
        .admit(JobOwner::of(&inbound), anchor, None, Arc::from("sleep 5"))
        .unwrap();
    let mut sessions = tokio::task::JoinSet::new();
    crate::dispatch(
        &runner,
        &routes,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &["stop".to_owned()],
        &mut sessions,
        &mut crate::collection::Collector::new(&[], 4),
        inbound,
    );
    assert!(signal.is_cancelled());
    drop(run);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_stop_word_ends_its_persons_running_turn_and_job_but_not_anothers_job() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(4, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 3600 &"),
        answer("b started"),
        script_call("sleep 3600 & sleep 3600"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 2, 2);
    let routes = job_routes(directory.path(), Some(3_600_000)).await;
    let driver = Arc::new(RecordingDriver::default());
    let turn = || job_route(Duration::from_secs(3600), Duration::from_secs(3600));
    run_session(
        Arc::clone(&runner),
        turn(),
        from("b", "start b"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let a_turn = tokio::spawn(run_session(
        Arc::clone(&runner),
        turn(),
        from("a", "start a"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    let (a, b) = (owned_by("a"), owned_by("b"));
    until(Duration::from_secs(10), || {
        running(&runner.jobs.list(&a)) && running(&runner.jobs.list(&b))
    })
    .await;
    let drivers = BTreeMap::from([("dev".to_owned(), Arc::clone(&driver) as Arc<dyn ChatDriver>)]);
    let mut sessions = tokio::task::JoinSet::new();
    crate::dispatch(
        &runner,
        &routes,
        &BTreeMap::new(),
        &drivers,
        &["stop".to_owned()],
        &mut sessions,
        &mut crate::collection::Collector::new(&[], 4),
        from("a", "stop"),
    );
    tokio::time::timeout(Duration::from_secs(2), a_turn)
        .await
        .expect("the stop word ends A's turn")
        .expect("A's turn joins");
    while sessions.join_next().await.is_some() {}
    let [row] = settled(&runner.jobs, &a, Duration::from_secs(2))
        .await
        .try_into()
        .expect("A's one job");
    assert_eq!(finished(&row).0, JobOutcome::Cancelled);
    assert!(running(&runner.jobs.list(&b)), "B's job keeps running");
    let stopped = format!("reply:{}", crate::session::STOPPED_REPLY);
    assert_eq!(
        driver
            .rendered()
            .iter()
            .filter(|event| **event == stopped)
            .count(),
        1,
        "{:?}",
        driver.rendered()
    );
    runner.jobs.cancel_all();
}

#[test]
fn serve_shutdown_cancels_a_sleeping_job_whose_thread_exits_within_two_seconds() {
    let directory = temporary();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let owner = JobOwner::of(&message("start"));
    let (runner, service, _transport) = runtime.block_on(async {
        let routes = job_routes(directory.path(), Some(3_600_000)).await;
        let (broker, _observed) =
            stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
        let models = ModelScript::new([script_call("sleep 3600 &"), answer("started")]);
        let runner = runner_with_jobs(broker, models, 1, 2);
        let driver = Arc::new(RecordingDriver::default());
        let drivers = Arc::new(BTreeMap::from([(
            "dev".to_owned(),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )]));
        let (transport, receiver) = mpsc::channel(4);
        let service = tokio::spawn(crate::serve(
            Arc::clone(&runner),
            routes,
            Arc::new(BTreeMap::new()),
            drivers,
            Arc::new(Vec::new()),
            receiver,
            async move {
                stopped.await.ok();
            },
            Duration::from_secs(5),
            crate::collection::Collector::new(&[], 4),
        ));
        transport
            .send(TransportEvent::Message(Box::new(message("start"))))
            .await
            .expect("serve receives");
        until(Duration::from_secs(10), || {
            driver.replies() == ["started"] && running(&runner.jobs.list(&owner))
        })
        .await;
        (runner, service, transport)
    });
    let shutdown = Instant::now();
    stop.send(()).expect("serve is listening");
    let outcome = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), service).await })
        .expect("serve returns within its grace")
        .expect("serve joins");
    assert_eq!(outcome, crate::ServeOutcome::Shutdown);
    let (joined, on_joined) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(runtime);
        joined.send(()).ok();
    });
    on_joined
        .recv_timeout(Duration::from_secs(2).saturating_sub(shutdown.elapsed()))
        .expect("every blocking thread, the job's included, exits within 2 s of shutdown");
    let [row] = runner.jobs.list(&owner).try_into().expect("one job");
    assert_eq!(finished(&row).0, JobOutcome::Cancelled);
    assert_eq!(runner.jobs.free_permits(), 2);
}

fn denied_probe_write() -> ResponseEnvelope {
    ResponseEnvelope::invocation(
        record_result(InvocationOutcome::Denied, Some("probe-write")),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

fn attested(
    observed: &mut mpsc::UnboundedReceiver<RequestEnvelope>,
) -> (Vec<Trigger>, Vec<Trigger>) {
    let (mut surfaces, mut invocations) = (Vec::new(), Vec::new());
    while let Ok(request) = observed.try_recv() {
        match request.request {
            BrokerRequest::Capabilities {
                attestation:
                    Some(Attestation {
                        scope: Some(scope), ..
                    }),
            } => surfaces.push(scope.trigger),
            BrokerRequest::Invoke {
                attestation:
                    Some(Attestation {
                        scope: Some(scope), ..
                    }),
                ..
            } => invocations.push(scope.trigger),
            BrokerRequest::Capabilities { .. }
            | BrokerRequest::RunCommand { .. }
            | BrokerRequest::Invoke { .. }
            | BrokerRequest::RecordDeliveredTurn { .. } => {}
        }
    }
    (surfaces, invocations)
}

fn watching(
    broker: ResolvedBroker,
    models: &Arc<ModelScript>,
    store: &Arc<crate::wake::WakeStore>,
) -> Arc<SessionRunner> {
    let mut runner = Arc::into_inner(runner_with_jobs(broker, Arc::clone(models), 2, 2))
        .expect("a fresh runner has one owner");
    runner.wakes = Some(Arc::clone(store));
    Arc::new(runner)
}

fn watch(script: &str) -> AssistantTurn {
    super::wakes::wake_call(&json!({
        "action": "watch",
        "note": "tell them",
        "script": script,
        "everySeconds": 60,
        "forSeconds": 3600,
    }))
}

async fn tick_once(
    runner: &Arc<SessionRunner>,
    routes: &Arc<RoutingTable>,
    store: &Arc<crate::wake::WakeStore>,
) {
    let tick = store
        .take_due(std::time::SystemTime::now() + Duration::from_secs(61))
        .expect("readable")
        .ticks
        .pop()
        .expect("the watch is due");
    let mut ticks = tokio::task::JoinSet::new();
    crate::spawn_tick(runner, routes, store, &mut ticks, tick);
    assert!(matches!(ticks.join_next().await, Some(Ok(None))));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_jobs_a_watch_starts_in_baseline_and_tick_attest_as_probes_and_cannot_write() {
    let directory = temporary();
    let routes = job_routes(directory.path(), Some(60_000)).await;
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![
            probe_listing(),
            probe_listing(),
            probe_listing(),
            upper_proposal("x"),
            denied_probe_write(),
            probe_listing(),
            probe_listing(),
            upper_proposal("x"),
            denied_probe_write(),
        ],
    )
    .await;
    let script = "probe upper --text x & wait $!; echo job=$?; exit 1";
    let models = ModelScript::new([watch(script), answer("watching")]);
    let store = super::wakes::wake_store(directory.path());
    let runner = watching(broker, &models, &store);
    let inbound = super::wakes::slack_message("watch it");
    let route = routes.route(&inbound).cloned().expect("routed");
    let receipt = tool_output_of(&runner, &models, route, inbound.clone()).await;
    let denied = format!("job={}", ExitCode::DENIED.get());
    assert!(receipt.contains(&denied), "{receipt}");
    tick_once(&runner, &routes, &store).await;
    let rows = runner.jobs.list(&JobOwner::of(&inbound));
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(finished(row), (JobOutcome::Failed, ExitCode::DENIED));
    }
    assert_eq!(
        attested(&mut observed),
        (
            vec![
                Trigger::Message,
                Trigger::Probe,
                Trigger::Probe,
                Trigger::Probe,
                Trigger::Probe
            ],
            vec![Trigger::Probe, Trigger::Probe]
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watch_on_a_route_without_a_job_deadline_starts_no_job_in_baseline_or_tick() {
    let directory = temporary();
    let routes = job_routes(directory.path(), None).await;
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![probe_listing(), probe_listing(), probe_listing()],
    )
    .await;
    let models = ModelScript::new([watch("true & echo started=$?; exit 1"), answer("watching")]);
    let store = super::wakes::wake_store(directory.path());
    let runner = watching(broker, &models, &store);
    let inbound = super::wakes::slack_message("watch it");
    let route = routes.route(&inbound).cloned().expect("routed");
    let receipt = tool_output_of(&runner, &models, route, inbound.clone()).await;
    assert!(receipt.contains(dekopon_shell::JOBS_OFF), "{receipt}");
    assert!(receipt.contains("started=1"), "{receipt}");
    tick_once(&runner, &routes, &store).await;
    assert!(runner.jobs.list(&JobOwner::of(&inbound)).is_empty());
    assert_eq!(
        attested(&mut observed),
        (
            vec![Trigger::Message, Trigger::Probe, Trigger::Probe],
            vec![]
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn another_persons_jobs_kill_and_wait_refuse_with_the_same_message() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(3, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 30 &"),
        answer("a"),
        script_call("jobs; echo listed; kill %1; echo kill=$?; wait %1; echo wait=$?"),
        answer("b"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let turn = || job_route(Duration::from_secs(10), Duration::from_secs(60));
    tool_output_of(&runner, &models, turn(), from("a", "start")).await;
    let output = tool_output_of(&runner, &models, turn(), from("b", "meddle")).await;
    assert!(output.starts_with("listed\n"), "{output}");
    assert_eq!(
        output.matches("no job of this person has that id").count(),
        2,
        "{output}"
    );
    assert!(output.contains("kill=1"), "{output}");
    assert!(output.contains("wait=1"), "{output}");
    assert!(running(&runner.jobs.list(&owned_by("a"))));
    runner.jobs.cancel_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_reports_the_exit_status_a_job_ended_with() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 1 && exit 3 & wait $!; echo status=$?"),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let output = tool_output_of(
        &runner,
        &models,
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("three"),
    )
    .await;
    assert!(output.contains("status=3"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_ends_at_the_scripts_deadline_while_the_job_runs_on() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 30 & wait %1; echo after=$?"),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let started = Instant::now();
    let output = tool_output_of(
        &runner,
        &models,
        job_route(Duration::from_secs(1), Duration::from_secs(60)),
        message("deadline"),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(5), "{output}");
    assert!(!output.contains("after="), "{output}");
    assert!(running(
        &runner.jobs.list(&JobOwner::of(&message("deadline")))
    ));
    runner.jobs.cancel_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_on_a_finished_job_says_no_such_job() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("true & wait $!; kill %1; echo kill=$?"),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let output = tool_output_of(
        &runner,
        &models,
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("finished"),
    )
    .await;
    assert!(output.contains("no such job"), "{output}");
    assert!(output.contains("kill=1"), "{output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn jobs_lists_running_rows_and_a_new_start_drops_the_oldest_finished_one() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(4, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call(
            "echo A & wait $!\nsleep 1 && echo B & jobs\necho ---\nwait $!\necho C & wait $!\njobs",
        ),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let output = tool_output_of(
        &runner,
        &models,
        job_route(Duration::from_secs(10), Duration::from_secs(10)),
        message("listing"),
    )
    .await;
    let (first, second) = output.split_once("---").expect("two listings");
    let rows = |listing: &str| {
        listing
            .lines()
            .filter(|line| line.contains(" exit ") || line.contains(" running "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let first = rows(first);
    assert_eq!(first.len(), 2, "{output}");
    assert!(
        first[0].starts_with("[1] succeeded exit 0 after "),
        "{output}"
    );
    assert!(first[0].ends_with(" echo A"), "{output}");
    assert_eq!(first[1], "[2] running 0s sleep 1 && echo B", "{output}");
    let second = rows(second);
    assert_eq!(second.len(), 2, "{output}");
    assert!(
        second[0].starts_with("[2] succeeded exit 0 after "),
        "{output}"
    );
    assert!(second[0].ends_with(" sleep 1 && echo B"), "{output}");
    assert!(
        second[1].starts_with("[3] succeeded exit 0 after "),
        "{output}"
    );
    assert!(second[1].ends_with(" echo C"), "{output}");
}
