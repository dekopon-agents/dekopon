use super::*;
use crate::session::{Admit, CancelOutcome};
use tracing::instrument::WithSubscriber as _;

fn listing(principal: &str, mapped: bool) -> ResponseEnvelope {
    ResponseEnvelope::chat_capabilities(
        vec![capability("cli-probe.upper")],
        Vec::new(),
        BTreeMap::new(),
        None,
        Some(principal.parse().expect("principal")),
    )
    .with_space(mapped.then(|| vec!["cli-probe.upper".parse().expect("capability")]))
}

fn speaker(index: usize, text: &str) -> InboundMessage {
    let mut inbound = owned_slack_message(text, false);
    inbound.subject = format!("slack.t0123abc.u{index}xyz")
        .parse()
        .expect("subject");
    inbound.message_id = MessageId::Native(format!("1700000000.{index:06}"));
    inbound.thread_continuation = None;
    inbound
}

fn shared_route() -> BoundRoute {
    let mut route = persistent_route(model_config(), shared_window());
    route.transport = "scientist-slack".to_owned();
    route
}

async fn finish(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("session completes")
        .expect("session joins");
}

#[tokio::test(flavor = "multi_thread")]
async fn mapped_participants_are_consumed_once_with_their_own_attribution_and_join_lines() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![listing("asker", true), listing("ana", true)],
    )
    .await;
    let models = InterruptibleModel::new([answer("draft"), answer("final")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let route = shared_route();
    let first = speaker(9, "asker request");
    let task = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            first.clone(),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    for text in ["Ana's request", "Ana's followup"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            speaker(8, text),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    assert_eq!(models.interrupted.load(Ordering::SeqCst), 0);
    assert_eq!(
        runner
            .gate
            .cancel(&CancelRequest {
                transport: first.transport.clone(),
                conversation_id: first.conversation.key(),
                subject: speaker(8, "").subject.canonical().to_owned(),
                via: CancelVia::StopReply,
            })
            .running,
        CancelOutcome::OtherSubject
    );
    // The non-owner stop removes pending text, so submit it again before the drain.
    for text in ["Ana's request", "Ana's followup"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            speaker(8, text),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    models.release();
    finish(task).await;
    assert_eq!(driver.replies(), ["final"]);
    assert_eq!(models.requests(), 2);
    let prompt = models.prompt(1);
    let ana = prompt
        .iter()
        .filter(|(role, text)| {
            role == "user" && text.starts_with("[gateway: authenticated participant: ana]\n")
        })
        .map(|(_, text)| text)
        .collect::<Vec<_>>();
    assert_eq!(ana.len(), 2);
    assert_eq!(
        ana.iter()
            .filter(|text| text.contains("participant joined"))
            .count(),
        1
    );
    assert!(ana[0].contains("channel-downscoped capabilities: cli-probe: read"));
    assert!(ana[0].contains("Ana's request"));
    assert!(ana[1].contains("Ana's followup"));
    assert!(!ana.iter().any(|text| text.contains("handle p")));
    let requests = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    for (request, index) in requests.iter().zip([9, 8]) {
        let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = &request.request
        else {
            panic!("lookup expected")
        };
        assert_eq!(claim.subject, speaker(index, "").subject);
        assert_eq!(
            claim.scope.as_ref().unwrap().delivery,
            Some(DeliveryIdentity::Slack {
                channel: "c0123abc".into(),
                timestamp: format!("1700000000.{index:06}")
            })
        );
    }
    assert!(
        capture
            .events_where("dekopon_broker", &[("audit.event", "broker.decision")])
            .is_empty()
    );
    let parents = capture.span_parents();
    assert!(
        parents
            .iter()
            .any(|(name, parent)| *name == "gateway.participant.lookup"
                && parent.as_deref() == Some("gateway.session"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_and_empty_participant_legs_never_expose_their_messages_or_assets() {
    for refused in [
        ResponseEnvelope::session_refused(&SessionRefusal::Other),
        ResponseEnvelope::chat_capabilities(
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            None,
            Some("ana".parse().unwrap()),
        ),
    ] {
        let (capture, _guard) = capture_spans();
        let directory = temporary();
        let (broker, mut observed) =
            stub_broker(directory.path(), vec![listing("asker", true), refused]).await;
        let models = InterruptibleModel::new([answer("final")]);
        let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
        let driver = Arc::new(RecordingDriver::default());
        let route = shared_route();
        let task = tokio::spawn(
            run_session(
                Arc::clone(&runner),
                route.clone(),
                speaker(9, "asker request"),
                Arc::clone(&driver) as Arc<dyn ChatDriver>,
            )
            .with_current_subscriber(),
        );
        models.wait_until_entered().await;
        let mut refused = speaker(8, "untrusted participant secret");
        refused
            .assets
            .push(pending("refused-secret.png", "image/png", 12));
        run_session(
            Arc::clone(&runner),
            route,
            refused,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        models.release();
        finish(task).await;
        assert_eq!(models.requests(), 1);
        assert_eq!(driver.replies(), ["final"]);
        assert_eq!(capability_listings(&mut observed), 2);
        let admissions = capture.events_where(
            "dekopon_gatewayd::audit",
            &[
                ("audit.event", "gateway.admission"),
                ("outcome", "unauthorized"),
            ],
        );
        assert_eq!(admissions.len(), 1);
        assert!(
            !capture
                .events_text()
                .contains("untrusted participant secret")
        );
        assert!(!capture.events_text().contains("refused-secret.png"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overflow_participants_are_authorized_and_refused_before_text_is_injected() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let responses = (1..=9)
        .map(|index| listing(&format!("person{index}"), true))
        .chain([ResponseEnvelope::session_refused(&SessionRefusal::Other)])
        .collect();
    let (broker, mut observed) = stub_broker(directory.path(), responses).await;
    let models = InterruptibleModel::new([answer("draft"), answer("final")]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let route = shared_route();
    let task = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            speaker(1, "owner"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    for index in 2..=10 {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            speaker(index, &format!("text from {index}")),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
    }
    models.release();
    finish(task).await;
    let prompt = models.prompt(1);
    assert!(prompt.iter().any(|(_, text)| {
        text.starts_with("[gateway: authenticated participant: person9]\n")
            && text.contains("text from 9")
    }));
    assert!(!prompt.iter().any(|(_, text)| text.contains("text from 10")));
    assert_eq!(capability_listings(&mut observed), 10);
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd::audit",
                &[
                    ("audit.event", "gateway.admission"),
                    ("busy.cause", "participant-limit")
                ]
            )
            .len(),
        1
    );
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd::audit",
                &[
                    ("audit.event", "gateway.admission"),
                    ("outcome", "unauthorized")
                ]
            )
            .len(),
        1
    );
}

#[test]
fn mapping_resolution_promotes_pending_messages_and_keeps_unmapped_routes_queued() {
    for mapped in [true, false] {
        let gate = SessionGate::new(1);
        let route = shared_route();
        let first = speaker(9, "first");
        let key = (first.transport.clone(), first.conversation.key());
        let Admit::Admitted(admission, _, _) = gate.admit(&route, first, Dispositions(Vec::new()))
        else {
            panic!("first admitted")
        };
        assert!(matches!(
            gate.admit(&route, speaker(8, "second"), Dispositions(Vec::new())),
            Admit::Queued
        ));
        gate.resolve_mapping(&key, &admission.cancellation(), mapped);
        assert_eq!(gate.take_steers(&key).len(), usize::from(mapped));
        assert_eq!(admission.next_or_release().is_some(), !mapped);
    }
}
