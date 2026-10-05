use super::*;
use crate::routes::BoundRoute;
use crate::{
    collection::Dispositions,
    config::Steering,
    session::{Admit, MAILBOX_CAPACITY, SessionAdmission},
};
use dekopon_agent::prompt::CancellationProbe;

fn admit(gate: &SessionGate, route: &BoundRoute, message: InboundMessage) -> Admit {
    let receipts = Dispositions(message.constituents.clone());
    gate.admit(route, message, receipts)
}

fn held(gate: &SessionGate, route: &BoundRoute) -> SessionAdmission {
    let Admit::Admitted(admission, _, receipts) = admit(gate, route, message("first")) else {
        panic!("first message must be admitted");
    };
    receipts.finish("answered");
    admission
}

#[test]
fn same_conversation_steers_even_when_every_permit_is_taken() {
    let gate = SessionGate::new(1);
    let mut route = route(model_config());
    for flavor in [Steering::Abort, Steering::Boundary] {
        route.steering = flavor;
        let admission = held(&gate, &route);
        let cancellation = admission.cancellation();
        let model = cancellation.model_watch();
        let session = cancellation.signal().watch();
        assert!(matches!(
            admit(&gate, &route, message("steer")),
            Admit::Steered(_)
        ));
        assert_eq!(*model.borrow(), flavor == Steering::Abort);
        assert!(!*session.borrow(), "steering never cancels the broker leg");
        let key = (message("").transport, message("").conversation.key());
        assert_eq!(gate.take_steers(&key)[0].text, "steer");
        assert!(!*model.borrow(), "the drain rearms the model");
        assert!(matches!(
            admit(&gate, &route, message("stop drops this")),
            Admit::Steered(_)
        ));
        let stopped = gate.cancel(&cancel(SUBJECT, CancelVia::StopReply));
        assert!(stopped.dropped);
        assert_eq!(stopped.running, CancelOutcome::Cancelled);
        assert!(gate.take_steers(&key).is_empty());
        assert!(*model.borrow(), "draining never clears a stop");
        assert!(*session.borrow());
        assert!(admission.next_or_release().is_none());
    }
}

#[test]
fn drained_steers_exhaust_the_abort_budget_but_a_followup_gets_a_fresh_budget() {
    let gate = SessionGate::new(1);
    let route = route(model_config());
    let admission = held(&gate, &route);
    let cancellation = admission.cancellation();
    let model = cancellation.model_watch();
    let key = (message("").transport, message("").conversation.key());

    for index in 0..MAILBOX_CAPACITY + 2 {
        assert!(matches!(
            admit(&gate, &route, message("steer")),
            Admit::Steered(_)
        ));
        assert_eq!(*model.borrow(), index < MAILBOX_CAPACITY);
        assert!(!cancellation.is_cancelled());
        assert_eq!(gate.take_steers(&key)[0].text, "steer");
        assert!(!*model.borrow());
    }

    assert!(matches!(
        admit(&gate, &route, message_from("tel.999", "followup")),
        Admit::Queued
    ));
    let (admission, _) = admission.next_or_release().expect("queued followup");
    let model = admission.cancellation().model_watch();
    assert!(matches!(
        admit(&gate, &route, message_from("tel.999", "new steer")),
        Admit::Steered(_)
    ));
    assert!(*model.borrow());
    assert_eq!(gate.take_steers(&key)[0].text, "new steer");
    assert!(admission.next_or_release().is_none());
}

#[test]
fn the_ninth_item_is_full_counting_steers_and_followups_together() {
    let gate = SessionGate::new(1);
    let route = route(model_config());
    let _held = held(&gate, &route);
    for index in 0..MAILBOX_CAPACITY {
        let result = if index % 2 == 0 {
            admit(&gate, &route, message("steer"))
        } else {
            admit(&gate, &route, message_from("tel.999", "followup"))
        };
        assert!(matches!(result, Admit::Steered(_) | Admit::Queued));
    }
    assert!(matches!(
        admit(&gate, &route, message("overflow")),
        Admit::Full(_, _)
    ));
}

#[test]
fn stopping_a_queued_subject_preserves_the_holder_and_other_subjects() {
    let gate = SessionGate::new(1);
    let route = route(model_config());
    let admission = held(&gate, &route);
    assert!(matches!(
        admit(&gate, &route, message_from("tel.999", "discard")),
        Admit::Queued
    ));
    assert!(matches!(
        admit(&gate, &route, message_from("tel.888", "keep")),
        Admit::Queued
    ));
    let stopped = gate.cancel(&cancel("tel.999", CancelVia::StopReply));
    assert!(stopped.dropped);
    assert_eq!(stopped.running, CancelOutcome::OtherSubject);
    assert!(!admission.cancellation().is_cancelled());
    let (admission, followup) = admission.next_or_release().expect("other sender survives");
    assert_eq!(followup.message.subject.canonical(), "tel.888");
    assert!(admission.next_or_release().is_none());
}

#[test]
fn a_followup_gets_fresh_cancellation_and_is_stopped_as_its_own_subject() {
    let gate = SessionGate::new(1);
    let route = route(model_config());
    let admission = held(&gate, &route);
    assert!(matches!(
        admit(&gate, &route, message_from("tel.999", "next")),
        Admit::Queued
    ));
    assert_eq!(
        gate.cancel(&cancel(SUBJECT, CancelVia::StopReply)).running,
        CancelOutcome::Cancelled
    );
    let (admission, followup) = admission.next_or_release().expect("followup");
    assert_eq!(followup.message.text, "next");
    assert!(!admission.cancellation().is_cancelled());
    assert!(!*admission.cancellation().model_watch().borrow());
    assert_eq!(
        gate.cancel(&cancel("tel.999", CancelVia::StopReply))
            .running,
        CancelOutcome::Cancelled
    );
    assert!(admission.cancellation().is_cancelled());
    assert!(admission.next_or_release().is_none());
    drop(held(&gate, &route));
}

#[tokio::test(flavor = "multi_thread")]
async fn steers_and_queued_senders_are_acknowledged_only_on_their_own_targets() {
    for (subject, connection) in [(SUBJECT, Some(2)), ("tel.999", Some(3)), (SUBJECT, None)] {
        let directory = temporary();
        let listings = if subject == SUBJECT { 1 } else { 2 };
        let (broker, _observed) = stub_broker(
            directory.path(),
            (0..listings)
                .map(|_| {
                    ResponseEnvelope::capabilities(
                        vec![capability("cli-probe.upper")],
                        Vec::new(),
                        BTreeMap::new(),
                    )
                })
                .collect(),
        )
        .await;
        let model = BlockedModel::new("finished");
        let runner = runner_with(broker, Arc::new(Arc::clone(&model)), 4);
        let driver = Arc::new(RecordingDriver::default().with_steer_ack());
        let mut route = route(model_config());
        route.steering = Steering::Boundary;
        let mut first = message("first");
        first.liveness = Some(LivenessTarget::Local { connection: 1 });
        let held = tokio::spawn(run_session(
            Arc::clone(&runner),
            route.clone(),
            first,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        ));
        model.wait_until_entered().await;
        let mut next = message_from(subject, "next");
        next.liveness = connection.map(|connection| LivenessTarget::Local { connection });
        run_session(
            runner,
            route,
            next,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        let expected = connection
            .map(|connection| dekopon_test_support::DriverCall::Seen {
                target: format!("local:{connection}"),
            })
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(driver.calls(), expected);
        model.release();
        held.await.expect("holder and any follow-up finish");
    }
}

#[test]
fn leftover_steers_never_become_another_subjects_input() {
    let gate = SessionGate::new(1);
    let route = route(model_config());
    let admission = held(&gate, &route);
    assert!(matches!(
        admit(&gate, &route, message("leftover")),
        Admit::Steered(_)
    ));
    assert!(matches!(
        admit(&gate, &route, message_from("tel.999", "other")),
        Admit::Queued
    ));
    let (admission, followup) = admission.next_or_release().expect("other sender first");
    assert_eq!(followup.message.subject.canonical(), "tel.999");
    let key = (
        followup.message.transport.clone(),
        followup.message.conversation.key(),
    );
    assert!(gate.take_steers(&key).is_empty());
    let (admission, followup) = admission
        .next_or_release()
        .expect("original sender's leftover");
    assert_eq!(followup.message.subject.canonical(), SUBJECT);
    assert!(followup.message.text.contains("leftover"));
    assert!(admission.next_or_release().is_none());
}
