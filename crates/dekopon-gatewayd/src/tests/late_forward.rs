use super::*;
use crate::{
    config::Steering,
    session::MAILBOX_CAPACITY,
    transport::{CancelRequest, receive_span},
};
use tracing::instrument::WithSubscriber as _;

const BLOCK: &str = "[quoted Slack forward — author: \"UORIGINAL\"; channel: \"CORIGINAL\"; time: \"1700000000.000100\"]\na quoted answer\n[/quoted Slack forward]";

fn late_for(original: &InboundMessage) -> Box<InboundMessage> {
    let mut late = original.clone();
    late.text = BLOCK.to_owned();
    late.control_text = Some(BLOCK.to_owned());
    late.assets = vec![pending("scene.png", "image/png", 24)];
    late.receive_span = receive_span(dekopon_broker_protocol::ChatTransportKind::Slack);
    late.received_at = tokio::time::Instant::now();
    Box::new(late)
}

fn original(text: &str) -> InboundMessage {
    let mut inbound = message(text);
    inbound.message_id = MessageId::Native("1700000000.000001".to_owned());
    inbound
}

fn other_sender(text: &str, ts: &str) -> InboundMessage {
    let mut inbound = message(text);
    inbound.subject =
        dekopon_core::AttestedSubject::one("drn:tel:number::16034700199".parse().expect("subject"))
            .expect("one-form subject");
    inbound.message_id = MessageId::Native(ts.to_owned());
    inbound
}

fn blocks(models: &InterruptibleModel) -> usize {
    (0..models.requests())
        .map(|index| {
            models
                .prompt(index)
                .iter()
                .filter(|(role, _)| role == "user")
                .map(|(_, text)| text.matches("[quoted Slack forward").count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0)
}

fn unheld(capture: &dekopon_test_support::CaptureLayer) -> usize {
    capture
        .records()
        .iter()
        .filter(|record| record.field("drop.reason") == Some("late-forward-unheld"))
        .count()
}

async fn finish(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("session completes")
        .expect("session joins");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_for_an_admitted_turn_not_yet_polled_reaches_that_turn_once() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("seen")]);
    models.release();
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let first = original("what is this?");
    let late = late_for(&first);

    let session = run_session(
        Arc::clone(&runner),
        route(model_config()),
        first,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    );
    runner
        .gate
        .enrich(late)
        .expect("the admitted turn holds the message");
    tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("session completes");

    assert_eq!(driver.replies(), ["seen"]);
    assert_eq!(models.requests(), 1);
    assert_eq!(blocks(&models), 1);
    let prompt = models.prompt(0);
    assert_eq!(
        prompt
            .iter()
            .filter(|(_, text)| text.contains("chat-asset:1"))
            .count(),
        1,
        "{prompt:?}"
    );
    assert_eq!(unheld(&capture), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_for_a_turn_queued_behind_another_is_merged_into_it() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("first done"), answer("seen")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let route = route(model_config());
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            other_sender("another request", "1700000000.000000"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    let first = original("what is this?");
    let late = late_for(&first);
    run_session(
        Arc::clone(&runner),
        route,
        first,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;

    runner
        .gate
        .enrich(late)
        .expect("the queued follow-up holds the message");
    models.release();
    finish(held).await;

    assert_eq!(driver.replies(), ["first done", "seen"]);
    assert_eq!(models.requests(), 2);
    let queued = models.prompt(1);
    let user = &queued.last().expect("question").1;
    assert!(user.contains(&format!("what is this?\n{BLOCK}")), "{user}");
    assert_eq!(user.matches(BLOCK).count(), 1, "{user}");
    assert_eq!(blocks(&models), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_for_a_running_turn_is_taken_into_it_under_either_steering_mode() {
    for flavor in [Steering::Abort, Steering::Boundary] {
        let directory = temporary();
        let (broker, _observed) =
            stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
        let turns = match flavor {
            Steering::Abort => vec![answer("seen")],
            Steering::Boundary => vec![answer("draft"), answer("seen")],
        };
        let models = InterruptibleModel::new(turns);
        let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
        let driver = Arc::new(RecordingDriver::default());
        let mut route = route(model_config());
        route.steering = flavor;
        route.limits.max_steps = 2;
        let first = original("what is this?");
        let late = late_for(&first);
        let held = tokio::spawn(
            run_session(
                Arc::clone(&runner),
                route,
                first,
                Arc::clone(&driver) as Arc<dyn ChatDriver>,
            )
            .with_current_subscriber(),
        );
        models.wait_until_entered().await;

        runner
            .gate
            .enrich(late)
            .expect("the running turn holds the message");
        if flavor == Steering::Boundary {
            models.release();
        }
        finish(held).await;

        assert_eq!(driver.replies(), ["seen"], "{flavor:?}");
        assert_eq!(models.requests(), 2, "{flavor:?}");
        assert_eq!(
            models.interrupted.load(Ordering::SeqCst),
            usize::from(flavor == Steering::Abort)
        );
        assert_eq!(blocks(&models), 1, "{flavor:?}");
        assert!(
            models
                .prompt(1)
                .iter()
                .any(|(_, text)| text.contains("chat-asset:1")),
            "{flavor:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_arriving_during_the_final_call_is_dropped_without_another_turn() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("answered")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    route.limits.max_steps = 1;
    let first = original("what is this?");
    let late = late_for(&first);
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route,
            first,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;

    runner
        .gate
        .enrich(late)
        .expect("the running turn holds the message");
    models.release();
    finish(held).await;

    assert_eq!(driver.replies(), ["answered"]);
    assert_eq!(models.requests(), 1);
    assert_eq!(blocks(&models), 0);
    assert_eq!(unheld(&capture), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_after_its_turn_completed_is_refused() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("answered"), answer("next done")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let route = route(model_config());
    let first = original("what is this?");
    let late = late_for(&first);
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
    run_session(
        Arc::clone(&runner),
        route,
        other_sender("next request", "1700000000.000005"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    models.release();
    finish(held).await;

    let refused = runner
        .gate
        .enrich(late)
        .expect_err("a completed turn holds nothing");
    refused.unheld();

    assert_eq!(driver.replies(), ["answered", "next done"]);
    assert_eq!(models.requests(), 2);
    assert_eq!(blocks(&models), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_is_refused_once_its_turn_has_sealed_with_the_next_turn_queued() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([answer("answered"), answer("next done")]);
    let runner = runner(broker, Arc::clone(&models), 1);
    let (release, released) = tokio::sync::oneshot::channel();
    let driver = Arc::new(steering_e2e::HeldReplyDriver {
        recording: RecordingDriver::default(),
        entered: tokio::sync::Notify::new(),
        release: Mutex::new(Some(released)),
    });
    let route = route(model_config());
    let first = original("what is this?");
    let late = late_for(&first);
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            first,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    tokio::time::timeout(Duration::from_secs(10), driver.entered.notified())
        .await
        .expect("the sealed turn is delivering its answer");
    run_session(
        Arc::clone(&runner),
        route,
        other_sender("next request", "1700000000.000005"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;

    assert!(runner.gate.enrich(late).is_err());
    release.send(()).expect("reply waiting");
    finish(held).await;

    assert_eq!(driver.recording.replies(), ["answered", "next done"]);
    assert!(
        (0..2)
            .flat_map(|index| models.prompt(index))
            .all(|(_, text)| !text.contains("[quoted Slack forward"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_queued_when_its_sender_stops_the_turn_is_dropped_as_stopped() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    let first = original("what is this?");
    let late = late_for(&first);
    let request = CancelRequest {
        transport: first.transport.clone(),
        conversation_id: first.conversation.key(),
        subject: first.subject.name().canonical().to_owned(),
        via: CancelVia::StopReply,
    };
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route,
            first,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    runner
        .gate
        .enrich(late)
        .expect("the running turn holds the message");

    runner.gate.cancel(&request);
    finish(held).await;

    assert_eq!(models.requests(), 1);
    assert_eq!(blocks(&models), 0);
    assert!(
        capture
            .records()
            .iter()
            .any(|record| record.field("outcome") == Some("stopped")),
        "{}",
        capture.events_text()
    );
    assert_eq!(unheld(&capture), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_left_by_a_failed_turn_is_dropped_without_another_turn() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([AssistantTurn::new(
        None,
        vec![ModelToolCall {
            id: "asset-call".into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: "fetch_chat_asset".to_owned(),
                arguments: json!({"id": 99}).to_string(),
            },
        }],
        None,
    )]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    route.limits.max_steps = 1;
    let first = original("what is this?");
    let late = late_for(&first);
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route,
            first,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    runner
        .gate
        .enrich(late)
        .expect("the running turn holds the message");

    models.release();
    finish(held).await;

    assert_eq!(models.requests(), 1, "the failed turn starts no follow-up");
    assert_eq!(blocks(&models), 0);
    assert_eq!(unheld(&capture), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_forward_is_refused_by_a_full_mailbox_which_keeps_its_requests() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("answered")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    route.limits.max_steps = 1;
    let first = original("what is this?");
    let late = late_for(&first);
    let held = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            first.clone(),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    for index in 0..MAILBOX_CAPACITY {
        let queued = other_sender("queued", &format!("1700000001.{index:06}"));
        assert!(matches!(
            runner
                .gate
                .admit(&route, queued, crate::collection::Dispositions(Vec::new())),
            crate::session::Admit::Queued
        ));
    }

    assert!(runner.gate.enrich(late).is_err());
    assert!(matches!(
        runner.gate.admit(
            &route,
            other_sender("one more", "1700000002.000000"),
            crate::collection::Dispositions(Vec::new())
        ),
        crate::session::Admit::Full(..)
    ));
    runner.gate.cancel(&CancelRequest {
        transport: first.transport.clone(),
        conversation_id: first.conversation.key(),
        subject: other_sender("", "").subject.name().canonical().to_owned(),
        via: CancelVia::StopReply,
    });
    models.release();
    finish(held).await;
    assert_eq!(driver.replies(), ["answered"]);
    assert_eq!(blocks(&models), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn leftover_requests_still_follow_up_while_a_leftover_late_forward_is_dropped() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([answer("answered"), answer("followed up")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    route.limits.max_steps = 1;
    let first = original("what is this?");
    let late = late_for(&first);
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
    let mut steer = original("and another thing");
    steer.message_id = MessageId::Native("1700000000.000003".to_owned());
    run_session(
        Arc::clone(&runner),
        route,
        steer,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    runner
        .gate
        .enrich(late)
        .expect("the running turn holds the message");

    models.release();
    finish(held).await;

    assert_eq!(driver.replies(), ["answered", "followed up"]);
    assert_eq!(models.requests(), 2);
    let follow_up = models.prompt(1);
    assert!(
        follow_up
            .last()
            .expect("follow-up")
            .1
            .contains("and another thing")
    );
    assert_eq!(blocks(&models), 0);
    assert_eq!(unheld(&capture), 1);
}

#[test]
fn a_turn_remembers_at_most_a_mailbox_of_ids_and_the_next_turn_starts_over() {
    let gate = crate::session::SessionGate::new(1);
    let route = route(model_config());
    let first = original("what is this?");
    let key = (first.transport.clone(), first.conversation.key());
    let crate::session::Admit::Admitted(admission, ..) =
        gate.admit(&route, first, crate::collection::Dispositions(Vec::new()))
    else {
        panic!("the first message starts a turn");
    };
    for index in 0..=MAILBOX_CAPACITY {
        let mut steer = original("more");
        steer.message_id = MessageId::Native(format!("1700000001.{index:06}"));
        assert!(matches!(
            gate.admit(&route, steer, crate::collection::Dispositions(Vec::new())),
            crate::session::Admit::Steered(_)
        ));
        gate.take_steers(&key);
    }

    let ids = gate.turn_ids_for_test(&key);
    assert_eq!(ids.len(), MAILBOX_CAPACITY);
    assert!(!ids.contains(&"1700000000.000001".to_owned()));
    assert_eq!(
        ids.last().map(String::as_str),
        Some(format!("1700000001.{MAILBOX_CAPACITY:06}").as_str())
    );

    assert!(matches!(
        gate.admit(
            &route,
            other_sender("next", "1700000009.000000"),
            crate::collection::Dispositions(Vec::new())
        ),
        crate::session::Admit::Queued
    ));
    let (_next, _followup) = admission.next_or_release().expect("the queued turn starts");
    assert_eq!(gate.turn_ids_for_test(&key), ["1700000009.000000"]);
}
