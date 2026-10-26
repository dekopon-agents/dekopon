use super::*;
use crate::session::{Admit, CancelOutcome};
use crate::{collection::Dispositions, routes::BoundRoute, transport::CancelRequest};
use dekopon_agent::prompt::CancellationProbe as _;
use dekopon_broker_protocol::{DeliveryIdentity, SessionRefusal};
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
    assert_eq!(models.interrupted.load(Ordering::SeqCst), 0);
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
    let lookups = capture
        .records()
        .into_iter()
        .filter(|record| {
            matches!(
                record,
                dekopon_test_support::Record::Span {
                    name: "gateway.participant.lookup",
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(lookups.len(), 1);
    assert_eq!(lookups[0].parent(), Some("gateway.session"));
    assert_eq!(
        lookups[0].field("turn.owner"),
        Some(first.subject.canonical()).as_deref()
    );
    assert_eq!(
        lookups[0].field("subject"),
        Some(speaker(8, "").subject.canonical()).as_deref()
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
        let (broker, mut observed) = stub_broker(
            directory.path(),
            vec![listing("asker", true), refused, listing("asker", true)],
        )
        .await;
        let models = InterruptibleModel::new([answer("final"), answer("next")]);
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
            route.clone(),
            refused,
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        models.release();
        finish(task).await;
        assert_eq!(models.requests(), 1);
        assert_eq!(driver.replies(), ["final"]);
        run_session(
            Arc::clone(&runner),
            route,
            speaker(9, "next turn"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        assert_eq!(capability_listings(&mut observed), 3);
        let admissions = capture.events_where(
            "dekopon_gatewayd::audit",
            &[
                ("audit.event", "gateway.admission"),
                ("outcome", "unauthorized"),
            ],
        );
        assert_eq!(admissions.len(), 1);
        for (_, text) in models.prompt(1) {
            assert!(!text.contains("untrusted participant secret"));
            assert!(!text.contains("refused-secret.png"));
        }
        for record in capture.events_where(
            "dekopon_agent::audit",
            &[("audit.event", "agent.model.prompt")],
        ) {
            let transcript = record.field("messages").expect("model prompt transcript");
            assert!(!transcript.contains("untrusted participant secret"));
            assert!(!transcript.contains("refused-secret.png"));
        }
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

#[test]
fn github_transport_never_steers_even_with_a_native_message_id_on_a_mapped_route() {
    let gate = SessionGate::new(1);
    let route = shared_route();
    let first = speaker(9, "first");
    let key = (first.transport.clone(), first.conversation.key());
    let Admit::Admitted(admission, _, _) = gate.admit(&route, first, Dispositions(Vec::new()))
    else {
        panic!("first admitted")
    };
    gate.resolve_mapping(&key, &admission.cancellation(), true);
    let mut incoming = speaker(8, "github event");
    incoming.transport_kind = dekopon_broker_protocol::ChatTransportKind::Github;
    assert!(matches!(
        gate.admit(&route, incoming, Dispositions(Vec::new())),
        Admit::Queued
    ));
    assert!(gate.take_steers(&key).is_empty());
    assert!(admission.next_or_release().is_some());
}

#[test]
fn private_scope_and_direct_messages_keep_other_senders_as_followups() {
    for private in [true, false] {
        let gate = SessionGate::new(1);
        let mut route = shared_route();
        let mut first = speaker(9, "first");
        let mut second = speaker(8, "second");
        if private {
            route.memory = MemoryPolicy::Persistent(window());
        } else {
            first.conversation.kind = ConversationKind::DirectMessage;
            second.conversation.kind = ConversationKind::DirectMessage;
        }
        let key = (first.transport.clone(), first.conversation.key());
        let Admit::Admitted(admission, _, _) = gate.admit(&route, first, Dispositions(Vec::new()))
        else {
            panic!("first admitted")
        };
        gate.resolve_mapping(&key, &admission.cancellation(), true);
        assert!(matches!(
            gate.admit(&route, second, Dispositions(Vec::new())),
            Admit::Queued
        ));
        assert!(gate.take_steers(&key).is_empty());
        assert!(admission.next_or_release().is_some());
    }
}

#[test]
fn only_the_askers_native_chat_steer_interrupts_a_mapped_turn() {
    let gate = SessionGate::new(1);
    let route = shared_route();
    let first = speaker(9, "first");
    let key = (first.transport.clone(), first.conversation.key());
    let Admit::Admitted(admission, _, _) = gate.admit(&route, first, Dispositions(Vec::new()))
    else {
        panic!("first admitted")
    };
    let cancellation = admission.cancellation();
    let model = cancellation.model_watch();
    gate.resolve_mapping(&key, &cancellation, true);
    assert!(matches!(
        gate.admit(
            &route,
            speaker(8, "other participant"),
            Dispositions(Vec::new())
        ),
        Admit::Steered(_)
    ));
    assert!(!*model.borrow());
    let mut github = speaker(9, "same subject Github");
    github.transport_kind = dekopon_broker_protocol::ChatTransportKind::Github;
    assert!(matches!(
        gate.admit(&route, github, Dispositions(Vec::new())),
        Admit::Queued
    ));
    assert!(!*model.borrow());
    assert!(matches!(
        gate.admit(&route, speaker(9, "asker steer"), Dispositions(Vec::new())),
        Admit::Steered(_)
    ));
    assert!(*model.borrow());
    assert!(!cancellation.is_cancelled());
}

#[tokio::test(flavor = "multi_thread")]
async fn final_step_leftovers_never_fold_an_unchecked_participant_into_another_subject() {
    for third_authorized in [false, true] {
        let directory = temporary();
        let third = if third_authorized {
            listing("carol", true)
        } else {
            ResponseEnvelope::session_refused(&SessionRefusal::Other)
        };
        let (broker, mut observed) = stub_broker(
            directory.path(),
            vec![listing("asker", true), listing("ana", true), third],
        )
        .await;
        let mut answers = vec![answer("owner final"), answer("participant final")];
        if third_authorized {
            answers.push(answer("third final"));
        }
        let models = InterruptibleModel::new(answers);
        let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
        let driver = Arc::new(RecordingDriver::default());
        let mut route = shared_route();
        route.limits.max_steps = 1;
        let task = tokio::spawn(run_session(
            Arc::clone(&runner),
            route.clone(),
            speaker(9, "asker request"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        ));
        models.wait_until_entered().await;
        for (index, text) in [
            (8, "authorized participant text"),
            (7, "unchecked participant marker"),
        ] {
            run_session(
                Arc::clone(&runner),
                route.clone(),
                speaker(index, text),
                Arc::clone(&driver) as Arc<dyn ChatDriver>,
            )
            .await;
        }
        models.release();
        finish(task).await;
        assert_eq!(models.requests(), if third_authorized { 3 } else { 2 });
        let requests = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        for (request, subject) in requests.iter().zip([9, 8, 7]) {
            let BrokerRequest::Capabilities {
                attestation: Some(claim),
            } = &request.request
            else {
                panic!("each leftover must get its own capability lookup");
            };
            assert_eq!(claim.subject, speaker(subject, "").subject);
        }
        assert!(models.prompt(1).iter().any(|(role, text)| role == "user"
            && text.starts_with("[gateway: authenticated participant: ana]\n")
            && text.contains("authorized participant text")));
        assert!(
            !models
                .prompt(1)
                .iter()
                .any(|(_, text)| text.contains("unchecked participant marker"))
        );
        if third_authorized {
            assert!(models.prompt(2).iter().any(|(role, text)| role == "user"
                && text.starts_with("[gateway: authenticated participant: carol]\n")
                && text.contains("unchecked participant marker")));
            assert_eq!(
                driver.replies(),
                ["owner final", "participant final", "third final"]
            );
        } else {
            assert_eq!(
                driver.replies(),
                [
                    "owner final",
                    "participant final",
                    crate::session::UNAUTHORIZED_REPLY
                ]
            );
        }
    }
}
