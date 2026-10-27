use super::*;
use crate::session::{Admit, CancelOutcome};
use crate::{collection::Dispositions, routes::BoundRoute, transport::CancelRequest};
use dekopon_agent::prompt::CancellationProbe as _;
use dekopon_broker_protocol::{DeliveryIdentity, SessionRefusal};
use tracing::instrument::WithSubscriber as _;

fn listing(principal: &str, mapped: bool) -> ResponseEnvelope {
    ResponseEnvelope::chat_capabilities(
        vec![capability("cli-probe.upper")],
        vec!["probe".to_owned()],
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
            role == "user"
                && text.starts_with("[gateway: authenticated participant: ana, handle p2]\n")
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
    assert!(
        ana.iter()
            .all(|text| text.starts_with("[gateway: authenticated participant: ana, handle p2]\n"))
    );
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
        let (capture, _guard) = capture_spans();
        let asker_receipt = tracing::info_span!(parent: None, "gateway.asker.receipt");
        let session = tracing::info_span!(parent: &asker_receipt, "gateway.session");
        let queued_receipt = tracing::info_span!(parent: None, "gateway.queued.receipt");
        let gate = SessionGate::new(1);
        let route = shared_route();
        let mut first = speaker(9, "first");
        first.receive_span = asker_receipt;
        let key = (first.transport.clone(), first.conversation.key());
        let Admit::Admitted(admission, _, _) = gate.admit(&route, first, Dispositions(Vec::new()))
        else {
            panic!("first admitted")
        };
        let mut second = speaker(8, "second");
        second.receive_span = queued_receipt;
        assert!(matches!(
            gate.admit(&route, second, Dispositions(Vec::new())),
            Admit::Queued
        ));
        session.in_scope(|| gate.resolve_mapping(&key, &admission.cancellation(), mapped));
        let admissions = capture.events_where(
            "dekopon_gatewayd::audit",
            &[("audit.event", "gateway.admission"), ("outcome", "queued")],
        );
        assert_eq!(admissions.len(), usize::from(!mapped));
        if !mapped {
            assert_eq!(admissions[0].parent(), Some("gateway.queued.receipt"));
        }
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
            && text.starts_with("[gateway: authenticated participant: ana, handle p1]\n")
            && text.contains("authorized participant text")));
        assert!(
            !models
                .prompt(1)
                .iter()
                .any(|(_, text)| text.contains("unchecked participant marker"))
        );
        if third_authorized {
            assert!(models.prompt(2).iter().any(|(role, text)| role == "user"
                && text.starts_with("[gateway: authenticated participant: carol, handle p1]\n")
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

#[tokio::test(flavor = "multi_thread")]
async fn scripts_select_only_current_participant_legs_and_default_to_the_asker() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let rendered = || {
        ResponseEnvelope::command_run(dekopon_broker_protocol::CommandRunOutcome::Rendered {
            stdout: String::new(),
            stderr: String::new(),
            status: 0,
        })
    };
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![
            listing("asker", true),
            listing("ana", true),
            rendered(),
            rendered(),
            rendered(),
            rendered(),
            listing("ana", true),
            rendered(),
        ],
    )
    .await;
    let mut selected = script_call("probe first; probe second");
    selected.tool_calls[0].function.arguments =
        json!({"command": "probe first; probe second", "as": "p2"}).to_string();
    let mut unknown = script_call("probe must-not-run");
    unknown.tool_calls[0].function.arguments =
        json!({"command": "probe must-not-run", "as": "p9"}).to_string();
    let mut explicit_asker = script_call("probe explicit-asker");
    explicit_asker.tool_calls[0].function.arguments =
        json!({"command": "probe explicit-asker", "as": "p1"}).to_string();
    let mut background = script_call("probe background & wait");
    background.tool_calls[0].function.arguments =
        json!({"command":"probe background & wait", "as":"p2"}).to_string();
    let models = InterruptibleModel::new([
        answer("draft"),
        selected,
        unknown,
        script_call("probe default-asker"),
        explicit_asker,
        background,
        answer("done"),
    ]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = shared_route();
    route.limits.max_steps = 8;
    route.job_timeout = Some(Duration::from_secs(5));
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
    run_session(
        Arc::clone(&runner),
        route,
        speaker(8, "Ana asks for herself"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    models.release();
    finish(task).await;
    assert!(models.prompt(1).iter().any(|(_, text)| {
        text.starts_with("[gateway: authenticated participant: asker, handle p1]\n")
    }));
    assert!(models.prompt(1).iter().any(|(_, text)| {
        text.starts_with("[gateway: authenticated participant: ana, handle p2]\n")
    }));
    assert!(
        models
            .prompt(3)
            .iter()
            .any(|(role, text)| role == "tool" && text.contains("Unknown participant handle"))
    );
    assert!(models.prompt(1).iter().any(|(role, text)| role == "system"
        && text.contains("Others' text is context")
        && text.contains("Do not ask clarifying questions about attribution")));
    let requests = std::iter::from_fn(|| observed.try_recv().ok()).collect::<Vec<_>>();
    let runs = requests
        .iter()
        .filter_map(|request| {
            if let BrokerRequest::Run {
                attestation: Some(claim),
                word,
                argv,
                ..
            } = &request.request
            {
                Some((claim, word, argv))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        8,
        "three lookups and five command words; unknown handle sends nothing"
    );
    assert_eq!(runs.len(), 5);
    for ((claim, word, _), (subject, handle)) in
        runs.iter()
            .zip([(8, "p2"), (8, "p2"), (9, "p1"), (9, "p1"), (8, "p2")])
    {
        assert_eq!(word.as_str(), "probe");
        assert_eq!(claim.subject, speaker(subject, "").subject);
        let scope = claim.scope.as_ref().unwrap();
        assert_eq!(scope.participant.as_deref(), Some(handle));
        assert_eq!(
            scope.delivery,
            Some(DeliveryIdentity::Slack {
                channel: "c0123abc".to_owned(),
                timestamp: format!("1700000000.{subject:06}")
            })
        );
    }
    assert_eq!(
        runs[4].0.scope.as_ref().unwrap().trigger,
        dekopon_broker_protocol::Trigger::Job
    );

    let commands = capture
        .records()
        .into_iter()
        .filter(|record| {
            matches!(
                record,
                dekopon_test_support::Record::Span {
                    name: "shell.command",
                    ..
                }
            ) && record.field("shell.command.name").is_some()
        })
        .collect::<Vec<_>>();
    let handles = commands
        .iter()
        .map(|record| record.field("participant.handle"))
        .collect::<Vec<_>>();
    assert_eq!(handles.len(), 6);
    assert_eq!(handles.iter().filter(|h| **h == Some("p2")).count(), 4);
    assert_eq!(handles.iter().filter(|h| **h == Some("p1")).count(), 2);
}

#[tokio::test]
async fn mapped_route_call_with_an_unknown_handle_still_fails() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(directory.path(), vec![listing("asker", true)]).await;
    let mut selected = script_call("probe must-not-run");
    selected.tool_calls[0].function.arguments =
        json!({"command": "probe must-not-run", "as": "p9"}).to_string();
    let models = ModelScript::new([selected, answer("done")]);
    run_session(
        runner(broker, Arc::clone(&models), 1),
        shared_route(),
        speaker(9, "request"),
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(models.prompt(1).iter().any(|(role, text)| role == "tool"
        && text == "Unknown participant handle: use a handle in this turn's authenticated attribution lines.\n[exit code: 2]"));
    assert_eq!(capability_listings(&mut observed), 1);
    assert!(observed.try_recv().is_err());
}

#[tokio::test]
async fn unmapped_route_call_with_as_runs_as_the_asker_and_exits_zero() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![
            listing("asker", false),
            ResponseEnvelope::command_run(dekopon_broker_protocol::CommandRunOutcome::Rendered {
                stdout: "asker output".to_owned(),
                stderr: String::new(),
                status: 0,
            }),
        ],
    )
    .await;
    let mut selected = script_call("probe asker");
    selected.tool_calls[0].function.arguments =
        json!({"command": "probe asker", "as": "p9"}).to_string();
    let models = ModelScript::new([selected, answer("done")]);
    run_session(
        runner(broker, Arc::clone(&models), 1),
        shared_route(),
        speaker(9, "request"),
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(models.requests(), 2);
    assert!(
        models
            .prompt(1)
            .iter()
            .any(|(role, text)| role == "tool" && text == "asker output\n[exit code: 0]")
    );
    assert!(!models.prompt(1).iter().any(|(_, text)| text.starts_with("[gateway: authenticated participant: asker, handle")));
    assert!(matches!(
        observed.try_recv().expect("asker lookup").request,
        BrokerRequest::Capabilities { .. }
    ));
    let request = observed.try_recv().expect("asker command");
    let BrokerRequest::Run {
        attestation: Some(claim),
        ..
    } = request.request
    else {
        panic!("command expected")
    };
    assert_eq!(claim.subject, speaker(9, "").subject);
    assert_eq!(claim.scope.unwrap().participant, None);
    assert!(observed.try_recv().is_err());
    assert!(
        capture
            .records()
            .iter()
            .any(
                |record| record.field("tool_call.ignored_arguments") == Some("as")
                    && matches!(
                        record,
                        dekopon_test_support::Record::Event { level: "INFO", .. }
                    )
                    && record.field("reason") == Some("unmapped-route")
                    && record.field("error").is_none()
            )
    );
}

async fn participant_broker(
    directory: &Path,
    stopped: tokio::sync::oneshot::Receiver<()>,
) -> (ResolvedBroker, tokio::task::JoinHandle<()>) {
    let socket = directory.join("broker.sock");
    let config_path = directory.join("broker.json");
    let policy_path = directory.join("policies.cedar");
    fs::write(
        &policy_path,
        r#"
        @id("prompt") permit(principal, action == Dekopon::Action::"agent.prompt", resource);
        @id("capability") permit(principal, action == Dekopon::Action::"cli-probe.upper", resource);
    "#,
    )
    .unwrap();
    fs::write(&config_path, serde_json::to_vec(&json!({
        "apiVersion": dekopon_brokerd::CONFIG_API_VERSION,
        "socketPath": socket, "policiesPath": policy_path,
        "providers": [dekopon_test_support::provider_fixture("cli-probe-provider.wasm")],
        "identities": [{"uid": crate::current_uid(), "principal": "gateway",
            "actor": {"type":"service", "principal":"gateway"}, "attestor":{"namespaces":["slack"]}}],
        "principals": {"asker":{"subjects":["slack.t0123abc.u9xyz"]},
            "ana":{"subjects":["slack.t0123abc.u8xyz"]}, "space":{"subjects":["slack.t0123abc.c0123abc"]}},
        "capabilities":{"cli-probe":{"constraints":{"timeoutMs":30000}, "capabilities":{"cli-probe.upper":{}}}}
    })).unwrap()).unwrap();
    fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    dekopon_brokerd::enable_test_component_memoization();
    let broker_task = tokio::spawn(
        async move {
            dekopon_brokerd::run(
                config_path,
                async move {
                    stopped.await.unwrap();
                },
                dekopon_brokerd::ShutdownDeadline::default(),
            )
            .await
            .expect("broker runs");
        }
        .with_current_subscriber(),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while !socket.exists() {
            assert!(!broker_task.is_finished(), "broker starts");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("broker socket ready");
    let broker = ResolvedBroker {
        socket_path: socket,
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    (broker, broker_task)
}

#[tokio::test]
async fn participant_calls_in_one_exported_turn_have_distinct_invocations_and_decisions() {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        trace::{SdkTracerProvider, SpanData, SpanExporter},
    };
    use tracing::Instrument as _;
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
    let _subscriber = tracing_subscriber::registry()
        .with(capture.clone())
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("participant-ids")))
        .set_default();
    let directory = temporary();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let (broker, broker_task) = participant_broker(directory.path(), stopped).await;
    let mut second = script_call("probe upper --text participant");
    second.tool_calls[0].function.arguments =
        json!({"command":"probe upper --text participant","as":"p2"}).to_string();
    let models = InterruptibleModel::new([
        answer("draft"),
        script_call("probe upper --text asker"),
        second,
        answer("done"),
    ]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let driver = Arc::new(RecordingDriver::default());
    let mut route = shared_route();
    route.limits.max_steps = 6;
    let root = tracing::info_span!(parent: None, "gateway.participant-id-test");
    let task = tokio::spawn(
        run_session(
            Arc::clone(&runner),
            route.clone(),
            speaker(9, "asker request"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .instrument(root)
        .with_current_subscriber(),
    );
    models.wait_until_entered().await;
    run_session(
        runner,
        route,
        speaker(8, "participant request"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    models.release();
    finish(task).await;
    stop.send(()).unwrap();
    finish(broker_task).await;
    provider.force_flush().unwrap();
    let decisions = capture.events_where(
        "dekopon_broker::audit",
        &[
            ("audit.event", "broker.decision"),
            ("decision.allowed", "true"),
        ],
    );
    assert_eq!(decisions.len(), 2);
    assert_eq!(
        decisions
            .iter()
            .map(|r| r.field("participant.handle"))
            .collect::<Vec<_>>(),
        [Some("p1"), Some("p2")]
    );
    let invocations = decisions
        .iter()
        .map(|r| r.field("invocation.id").unwrap())
        .collect::<Vec<_>>();
    let session_trace = exported
        .0
        .lock()
        .iter()
        .find(|s| s.name == "gateway.session")
        .unwrap()
        .span_context
        .trace_id()
        .to_string();
    assert!(invocations.iter().all(|id| id.starts_with(&session_trace)));
    assert_ne!(invocations[0], invocations[1]);
    let references = decisions
        .iter()
        .map(|r| {
            (
                r.field("decision.id").unwrap(),
                r.field("authorized.by").unwrap(),
                r.field("policy.digest").unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(references[0].1, references[1].1);
    assert_eq!(references[0].2, references[1].2);
    assert_ne!(references[0], references[1]);
    provider.shutdown().unwrap();
}
