use super::*;
use crate::{
    config::Steering,
    session::{MAILBOX_CAPACITY, STOPPED_REPLY},
};
use dekopon_test_support::{DriverCall, Record};
use tracing::instrument::WithSubscriber as _;

async fn finish_run(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("run finishes")
        .expect("run joins");
}

#[tokio::test(flavor = "multi_thread")]
async fn both_steering_flavors_consume_the_second_message_before_one_final_reply() {
    for flavor in [Steering::Abort, Steering::Boundary] {
        let (capture, _subscriber) = capture_spans();
        let directory = temporary();
        let (broker, mut observed) =
            stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
        let turns = match flavor {
            Steering::Abort => vec![answer("final")],
            Steering::Boundary => vec![answer("discarded draft"), answer("final")],
        };
        let models = InterruptibleModel::new(turns);
        let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
        let driver = Arc::new(RecordingDriver::default());
        let mut route = route(model_config());
        route.steering = flavor;
        route.limits.max_steps = if flavor == Steering::Abort { 1 } else { 2 };
        let held = tokio::spawn(
            run_session(
                Arc::clone(&runner),
                route.clone(),
                message("msg1"),
                Arc::clone(&driver) as Arc<dyn ChatDriver>,
            )
            .with_current_subscriber(),
        );
        models.wait_until_entered().await;
        run_session(
            runner,
            route,
            message("msg2"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        assert!(capture.events_text().contains("outcome=\"steered\""));
        if flavor == Steering::Boundary {
            assert_eq!(models.requests(), 1);
            models.release();
        }
        finish_run(held).await;
        assert_eq!(
            models.interrupted.load(Ordering::SeqCst),
            usize::from(flavor == Steering::Abort)
        );
        assert_eq!(models.requests(), 2);
        assert_eq!(driver.replies(), ["final"]);
        let retry = models.prompt(1);
        assert!(
            retry
                .iter()
                .any(|(role, text)| role == "user" && text.contains("msg2"))
        );
        assert_eq!(
            retry
                .iter()
                .any(|(role, text)| role == "assistant" && text == "discarded draft"),
            flavor == Steering::Boundary
        );
        assert_eq!(capability_listings(&mut observed), 1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_steered_text_is_recorded_with_a_marked_prefix_and_the_exact_answer() {
    let (capture, _subscriber) = capture_spans();
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![
            memory_surface_response(),
            help_rendered("memory"),
            ResponseEnvelope::invocation(
                record_result(InvocationOutcome::Succeeded, None),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
        ],
    )
    .await;
    let accepted = "delivered 🦀";
    let models = InterruptibleModel::new([answer("draft"), answer(accepted)]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    let first = "α".repeat(8 * 1024);
    let steer = "🦀".repeat(4 * 1024);
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(&first),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    for _ in 0..3 {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(&steer),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    models.release();
    finish_run(held).await;
    assert_eq!(driver.replies(), [accepted]);
    assert!(matches!(
        observed.recv().await.expect("surface request").request,
        BrokerRequest::Capabilities { .. }
    ));
    assert!(matches!(
        observed.recv().await.expect("help prefetch").request,
        BrokerRequest::RunCommand { word, argv, .. }
            if word == "memory" && argv == ["--help".to_owned()]
    ));
    let record = observed.recv().await.expect("bounded record request");
    let BrokerRequest::RecordDeliveredTurn { turn, .. } = record.request else {
        panic!("expected delivered turn: {record:?}");
    };
    assert!(turn.is_bounded());
    assert_eq!(turn.assistant().as_str(), accepted);
    let aggregate = [first.as_str(), &steer, &steer, &steer].join("\n\n");
    let budget = 64 * 1024 - accepted.len() - "[…]".len();
    assert!(aggregate.len() > 64 * 1024);
    assert!(!aggregate.is_char_boundary(budget));
    let end = aggregate.floor_char_boundary(budget);
    assert_eq!(turn.user(), format!("{}[…]", &aggregate[..end]));
    assert!(
        !capture
            .events_text()
            .contains("gateway_memory_record_failed")
    );
    assert!(observed.try_recv().is_err(), "record once, without a retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_steer_is_acknowledged_and_offers_fetch_in_the_retried_turn() {
    struct NoFetch;
    impl AssetFetcher for NoFetch {
        fn fetch(
            &self,
            _source: &AssetSourceRef,
            _max_bytes: u64,
        ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
            panic!("offering a photo must not eagerly fetch it");
        }
    }
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("photo received")]);
    let mut runner =
        Arc::into_inner(runner_with(broker, Arc::new(Arc::clone(&models)), 1)).expect("sole owner");
    assert!(
        runner
            .asset_fetchers
            .insert("dev".to_owned(), Arc::new(NoFetch))
            .is_none()
    );
    let runner = Arc::new(runner);
    let driver = Arc::new(RecordingDriver::default().with_steer_ack());
    let mut config = model_config();
    if let ModelConfig::OpenaiCompatible { modalities, .. } = &mut config {
        *modalities = vec![crate::config::Modality::Image];
    }
    let route = persistent_route(config, window());
    let held = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        message("inspect the next photo"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    models.wait_until_entered().await;
    let mut photo = burst_photo("");
    photo.liveness = Some(LivenessTarget::Local { connection: 2 });
    run_session(
        runner,
        route,
        photo,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    finish_run(held).await;
    assert_eq!(models.interrupted.load(Ordering::SeqCst), 1);
    assert_eq!(models.requests(), 2);
    assert!(
        models
            .prompt(1)
            .iter()
            .any(|(role, text)| role == "user" && text.contains("Chat Asset #1"))
    );
    assert!(
        !models
            .tool_names(0)
            .contains(&"fetch_chat_asset".to_owned())
    );
    assert!(
        models
            .tool_names(1)
            .contains(&"fetch_chat_asset".to_owned())
    );
    assert_eq!(
        driver
            .calls()
            .into_iter()
            .filter(|call| matches!(call, DriverCall::Seen { .. }))
            .collect::<Vec<_>>(),
        [DriverCall::Seen {
            target: "local:2".to_owned()
        }]
    );
    assert_eq!(driver.replies(), ["photo received"]);
    assert_eq!(capability_listings(&mut observed), 1);
}

struct HeldReplyDriver {
    recording: RecordingDriver,
    entered: tokio::sync::Notify,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl ChatDriver for HeldReplyDriver {
    async fn reply(
        &self,
        target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        let release = self.release.lock().expect("reply release").take();
        if let Some(release) = release {
            self.entered.notify_one();
            release.await.expect("first reply released");
        }
        ChatDriver::reply(&self.recording, target, reply).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn input_during_terminal_delivery_queues_a_fresh_turn_without_busy() {
    let (capture, _subscriber) = capture_spans();
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([answer("a1"), answer("a2")]);
    let runner = runner(broker, Arc::clone(&models), 1);
    let (release, released) = tokio::sync::oneshot::channel();
    let driver = Arc::new(HeldReplyDriver {
        recording: RecordingDriver::default(),
        entered: tokio::sync::Notify::new(),
        release: Mutex::new(Some(released)),
    });
    let route = route(model_config());
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message("first"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    tokio::time::timeout(Duration::from_secs(10), driver.entered.notified())
        .await
        .expect("reply started");
    run_session(
        runner,
        route,
        message("second"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(capture.events_text().contains("outcome=\"queued\""));
    assert_eq!(models.requests(), 1);
    assert!(driver.recording.replies().is_empty());
    release.send(()).expect("reply waiting");
    finish_run(held).await;
    assert_eq!(driver.recording.replies(), ["a1", "a2"]);
    assert_eq!(capability_listings(&mut observed), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn another_sender_runs_under_its_own_leg_after_the_holders_receipts_finish() {
    const OTHER: &str = "tel.16035550100";
    let (capture, _subscriber) = capture_spans();
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("a1"), answer("a2")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let route = persistent_route(model_config(), shared_window());
    let mut first = message("A's input");
    first.receive_span = tracing::info_span!(target: "dekopond::tests", "holder_receipt");
    first.constituents = vec![first.receive_span.clone()];
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            first,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    let mut next = message_from(OTHER, "B's input");
    next.receive_span = tracing::info_span!(target: "dekopond::tests", "followup_receipt");
    next.constituents = vec![next.receive_span.clone()];
    run_session(
        runner,
        route,
        next,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(capture.events_text().contains("outcome=\"queued\""));
    assert!(!capture.events_text().contains("gateway_input_disposition"));
    assert_eq!(models.requests(), 1);
    models.release();
    finish_run(held).await;
    assert_eq!(models.interrupted.load(Ordering::SeqCst), 0);
    assert_eq!(models.requests(), 2);
    assert!(
        !models
            .prompt(0)
            .iter()
            .any(|(_, text)| text.contains("B's input"))
    );
    assert!(
        models
            .prompt(1)
            .iter()
            .any(|(_, text)| text.contains("B's input"))
    );
    let mut subjects = Vec::new();
    while let Ok(request) = observed.try_recv() {
        let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = request.request
        else {
            panic!("a fresh attested listing per sender");
        };
        subjects.push(claim.subject);
    }
    assert_eq!(subjects, [subject(), OTHER.parse().expect("other subject")]);
    assert_eq!(driver.replies(), ["a1", "a2"]);
    let records = capture.records();
    let completed = records.iter().position(|record| matches!(record,
        Record::Span { name: "gateway.message", fields, .. } if fields.contains("outcome=\"answered\""))).expect("holder outcome");
    let finished_receipts: Vec<_> = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match record {
            Record::Event { fields, parent, .. }
                if fields.contains("gateway_input_disposition") =>
            {
                assert!(fields.contains("outcome=\"answered\""), "{fields}");
                Some((index, parent.as_deref()))
            }
            _ => None,
        })
        .collect();
    let starts: Vec<_> = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            matches!(record, Record::Span { name: "gateway.session", fields, .. }
            if fields.contains("gen_ai.operation.name=\"invoke_agent\""))
            .then_some(index)
        })
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(finished_receipts.len(), 2);
    assert_eq!(finished_receipts[0].1, Some("holder_receipt"));
    assert_eq!(finished_receipts[1].1, Some("followup_receipt"));
    assert!(completed < finished_receipts[0].0 && finished_receipts[0].0 < starts[1]);
    assert!(starts[1] < finished_receipts[1].0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ninth_pending_message_gets_busy_with_the_mailbox_cause() {
    let (capture, _subscriber) = capture_spans();
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = BlockedModel::new("finished");
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message("first"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    for index in 0..=MAILBOX_CAPACITY {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(&format!("pending {index}")),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    assert_eq!(driver.replies(), [BUSY_REPLY]);
    let events = capture.events();
    let admission = events
        .iter()
        .find(|(fields, _)| {
            fields.contains("audit.event=\"gateway.admission\"")
                && fields.contains("outcome=\"busy\"")
        })
        .expect("refusal audit");
    assert!(admission.0.contains("busy.cause=\"same-conversation\""));
    assert!(admission.0.contains("queue.depth=8"));
    assert!(events.iter().any(
        |(fields, _)| fields.contains("event=\"gateway_steer_refused\"")
            && fields.contains("reason=\"mailbox-full\"")
    ));
    models.release();
    finish_run(held).await;
    assert_eq!(driver.replies(), [BUSY_REPLY, "finished"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_stop_discards_a_boundary_steer_without_starting_a_followup() {
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("must not be delivered")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    let held = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        message("first"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    models.wait_until_entered().await;
    run_session(
        Arc::clone(&runner),
        route,
        message("discard this steer"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let stopped = runner.gate.cancel(&cancel(SUBJECT, CancelVia::StopReply));
    assert_eq!(stopped.running, CancelOutcome::Cancelled);
    assert!(stopped.dropped);
    finish_run(held).await;
    assert_eq!(driver.replies(), [STOPPED_REPLY]);
    assert_eq!(models.requests(), 1);
    assert_eq!(models.interrupted.load(Ordering::SeqCst), 1);
    assert_eq!(capability_listings(&mut observed), 1);
}
