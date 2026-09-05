use super::*;
use dekopon_broker::{AuditEvent, AuditLog, BrokerError, ChatScopeGrant, FileAuditLog};
use dekopon_broker_protocol::{
    ChatScopeClaim, ChatTransportKind, ControlOutcome, ControlProposal, ControlScope, ControlTarget,
};
use dekopon_core::{Effort, ModelSelection, SurfaceEpoch};

fn selection(model: &str, effort: Effort) -> ModelSelection {
    ModelSelection {
        model: model.parse().unwrap(),
        effort,
    }
}
fn targets() -> Vec<ControlTarget> {
    ["baseline", "gpt-5.6-sol"]
        .into_iter()
        .map(|model| ControlTarget {
            model: model.parse().unwrap(),
            efforts: vec![Effort::ProviderDefault, Effort::Low, Effort::High],
        })
        .collect()
}
fn scope(agent_name: &str) -> ControlScope {
    ControlScope {
        agent: agent(agent_name),
        job: "job-one".parse().unwrap(),
        session: "session-one".parse().unwrap(),
        request: "request-one".parse().unwrap(),
        generation: "generation-one".parse().unwrap(),
    }
}
fn proposal(id: &str, epoch: &SurfaceEpoch, agent_name: &str) -> ControlProposal {
    ControlProposal {
        id: id.parse().unwrap(),
        scope: scope(agent_name),
        sequence: 1,
        surface_epoch: epoch.clone(),
        from: selection("baseline", Effort::Low),
        to: selection("gpt-5.6-sol", Effort::Low),
        trace: "trace-control".parse().unwrap(),
        trace_parent: None,
    }
}
fn chat_scope() -> ChatScopeClaim {
    ChatScopeClaim {
        transport: "slack-work".parse().unwrap(),
        kind: ChatTransportKind::Slack,
        channel: "c123".into(),
        conversation: "c123:1700000000.000001".into(),
    }
}
fn claim() -> Attestation {
    Attestation::for_chat(subject(), agent("chat-agent"), chat_scope())
}
fn chat_grant() -> AttestorGrant {
    AttestorGrant {
        namespaces: vec!["slack.t0123abc".into()],
        chat_scopes: vec![ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Slack,
            transport: "slack-work".parse().unwrap(),
            channel: chat_scope().channel,
            conversation: chat_scope().conversation,
            local_subject_service: None,
        }],
    }
}
fn policies(model: bool, effort: bool) -> String {
    let mut text = String::new();
    for (who, agent_name, via) in [
        ("caller", "brokerd-test", false),
        ("cpetersen", "chat-agent", true),
    ] {
        for action in [
            Some("agent.prompt"),
            model.then_some("agent.model.select"),
            effort.then_some("agent.effort.set"),
        ]
        .into_iter()
        .flatten()
        {
            let condition = if via {
                "context has via && context.via == \"caller\""
            } else {
                "!(context has via)"
            };
            text.push_str(&format!("permit(principal == Dekopon::Principal::\"{who}\", action == Dekopon::Action::\"{action}\", resource == Dekopon::Agent::\"{agent_name}\") when {{ {condition} }};\n"));
        }
    }
    text
}
async fn build<A: AuditLog>(
    policy: &str,
    audit: Arc<A>,
    replay: Vec<InvocationId>,
    max: usize,
) -> Broker<A> {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("echo-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .unwrap();
    Broker::new_with_replay_ids(
        registry,
        "broker".parse().unwrap(),
        "revision".into(),
        echo_engine(policy, ["caller", "cpetersen"]),
        echo_catalog(),
        CredentialStore::empty(),
        IdentityDirectory::new([(subject(), "cpetersen".parse().unwrap())]).unwrap(),
        audit,
        BrokerLimits {
            max_replay_ids: max,
            ..BrokerLimits::default()
        },
        replay,
    )
    .unwrap()
    .with_control_targets(targets())
    .unwrap()
}
async fn send(
    path: &Path,
    proposal: ControlProposal,
    attestation: Option<Attestation>,
) -> BrokerResponse {
    let mut stream = UnixStream::connect(path).await.unwrap();
    write_frame(
        &mut stream,
        &RequestEnvelope {
            api_version: ProtocolVersion::V1Alpha3,
            request: BrokerRequest::AuthorizeControl {
                proposal,
                attestation,
            },
        },
        server_limits().frame,
    )
    .await
    .unwrap();
    read_frame::<_, ResponseEnvelope>(&mut stream, server_limits().frame)
        .await
        .unwrap()
        .response
}
fn outcome(response: BrokerResponse) -> ControlOutcome {
    let BrokerResponse::ControlDecision { decision } = response else {
        panic!("{response:?}")
    };
    decision.outcome
}

#[tokio::test]
async fn controls_authenticated_direct_and_attested_scopes_are_fresh_and_atomic() {
    let audit = Arc::new(InMemoryAuditLog::new(64).unwrap());
    let broker = Arc::new(build(&policies(true, false), audit.clone(), vec![], 64).await);
    let epoch = broker.surface_epoch().clone();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("control.sock");
    let listener = bind_fixture(&path);
    let server = BrokerServer::new(
        broker,
        BTreeMap::from([(
            current_uid(),
            MappedPeer {
                context: context("caller"),
                attestor: Some(chat_grant()),
            },
        )]),
        server_limits(),
    )
    .unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.serve(listener, shutdown_on(stopped)));
    let client = BrokerClient::new(&path, current_uid(), server_limits().frame).unwrap();
    assert_eq!(client.session_surface(None).await.unwrap().3, epoch);
    for (n, attestation, agent_name) in
        [(0, None, "brokerd-test"), (1, Some(claim()), "chat-agent")]
    {
        let mut live = client
            .control_client(scope(agent_name), epoch.clone(), attestation, 0)
            .unwrap();
        let admitted = live
            .authorize(
                1,
                format!("allowed-{n}").parse().unwrap(),
                selection("baseline", Effort::Low),
                selection("gpt-5.6-sol", Effort::Low),
                "trace-control".parse().unwrap(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(admitted.proposal().scope, scope(agent_name));
        assert!(admitted.decision_ref().starts_with("sha256:"));
        assert_eq!(admitted.consume(), ControlOutcome::Admitted);
        // Both changed dimensions need independent permits, not the prior model-only admission.
        let denied = live
            .authorize(
                2,
                format!("partial-{n}").parse().unwrap(),
                selection("baseline", Effort::Low),
                selection("gpt-5.6-sol", Effort::High),
                "trace-control".parse().unwrap(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(denied.consume(), ControlOutcome::Denied);
        let denied = live
            .authorize(
                3,
                format!("effort-{n}").parse().unwrap(),
                selection("baseline", Effort::Low),
                selection("baseline", Effort::High),
                "trace-control".parse().unwrap(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(denied.consume(), ControlOutcome::Denied);
    }
    let mut cross_agent = proposal("cross-agent", &epoch, "foreign-agent");
    assert_eq!(
        outcome(send(&path, cross_agent.clone(), None).await),
        ControlOutcome::Denied
    );
    cross_agent.id = "cross-claim".parse().unwrap();
    assert!(
        matches!(send(&path, cross_agent.clone(), Some(claim().bound_to(cross_agent.id))).await,
        BrokerResponse::Error { code, .. } if code == ERROR_INVALID_REQUEST)
    );
    let p = proposal("bad-binding", &epoch, "chat-agent");
    assert!(
        matches!(send(&path, p, Some(claim().bound_to("different".parse().unwrap()))).await,
        BrokerResponse::Error { code, .. } if code == ERROR_INVALID_REQUEST)
    );
    let p = proposal("foreign-scope", &epoch, "chat-agent");
    let mut foreign = claim().bound_to(p.id.clone());
    foreign.scope.as_mut().unwrap().conversation = "c123:1700000000.000002".into();
    assert_eq!(
        outcome(send(&path, p, Some(foreign)).await),
        ControlOutcome::Denied
    );
    let p = proposal("unmapped", &epoch, "chat-agent");
    let mut unknown = claim().bound_to(p.id.clone());
    unknown.subject = "slack.t0123abc.uunknown".parse().unwrap();
    assert_eq!(
        outcome(send(&path, p, Some(unknown)).await),
        ControlOutcome::Denied
    );
    // ID collision with an admitted control never returns cached admission.
    assert_eq!(
        outcome(send(&path, proposal("allowed-0", &epoch, "brokerd-test"), None).await),
        ControlOutcome::Denied
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    let records = audit.records().await;
    dekopon_broker::verify_audit_chain(&records).unwrap();
    assert!(
        records
            .iter()
            .all(|record| matches!(record.event, AuditEvent::ControlDecision { .. }))
    );
    assert_eq!(records.len(), 10); // malformed bindings did not reserve or audit.
}

#[tokio::test]
async fn controls_effort_only_both_forbid_error_unknown_and_no_scope_fail_closed() {
    for (model, effort, expected_model, expected_effort, expected_both) in [
        (false, true, false, true, false),
        (true, true, true, true, true),
        (false, false, false, false, false),
    ] {
        let audit = Arc::new(InMemoryAuditLog::new(32).unwrap());
        let broker = build(&policies(model, effort), audit, vec![], 32).await;
        for (i, to, allowed) in [
            (0, selection("gpt-5.6-sol", Effort::Low), expected_model),
            (1, selection("baseline", Effort::High), expected_effort),
            (2, selection("gpt-5.6-sol", Effort::High), expected_both),
        ] {
            let mut p = proposal(&format!("case-{i}"), broker.surface_epoch(), "brokerd-test");
            p.to = to;
            assert_eq!(
                broker
                    .authorize_control(&context("caller"), None, None, p)
                    .await
                    .unwrap()
                    .outcome
                    == ControlOutcome::Admitted,
                allowed
            );
        }
    }
    let mut extra = vec![
        "forbid(principal, action == Dekopon::Action::\"agent.model.select\", resource);".to_owned(),
        "permit(principal, action == Dekopon::Action::\"agent.model.select\", resource) when { context.toModel == \"never\" || (9223372036854775807 + 1) > 0 };".to_owned(),
    ];
    for (i, rule) in extra.drain(..).enumerate() {
        let audit = Arc::new(InMemoryAuditLog::new(32).unwrap());
        let broker = build(
            &format!("{}\n{rule}", policies(true, true)),
            audit.clone(),
            vec![],
            32,
        )
        .await;
        let p = proposal("forbid-error", broker.surface_epoch(), "brokerd-test");
        assert_eq!(
            broker
                .authorize_control(&context("caller"), None, None, p)
                .await
                .unwrap()
                .outcome,
            ControlOutcome::Denied
        );
        let records = audit.records().await;
        let AuditEvent::ControlDecision { reason, .. } = &records[0].event else {
            panic!()
        };
        assert_eq!(
            reason.as_deref(),
            Some(if i == 0 {
                "policy-denied"
            } else {
                "policy-error"
            })
        );
    }
    let audit = Arc::new(InMemoryAuditLog::new(32).unwrap());
    let broker = build(&policies(true, true), audit, vec![], 32).await;
    for (id, model, effort) in [
        ("unknown", "unknown-model", Effort::Low),
        ("unsupported", "baseline", Effort::Medium),
        ("noop", "baseline", Effort::Low),
    ] {
        let mut p = proposal(id, broker.surface_epoch(), "brokerd-test");
        p.to = selection(model, effort);
        assert_eq!(
            broker
                .authorize_control(&context("caller"), None, None, p)
                .await
                .unwrap()
                .outcome,
            ControlOutcome::Denied
        );
    }
    for (id, grant, attested) in [
        ("legacy", Some(attestor_grant()), claim()),
        ("missing-grant", None, claim()),
        (
            "subject-only",
            Some(chat_grant()),
            Attestation::for_subject(subject(), agent("chat-agent")),
        ),
    ] {
        let p = proposal(id, broker.surface_epoch(), "chat-agent");
        let attested = attested.bound_to(p.id.clone());
        assert_eq!(
            broker
                .authorize_control(&context("caller"), grant.as_ref(), Some(&attested), p)
                .await
                .unwrap()
                .outcome,
            ControlOutcome::Denied
        );
    }
    let disabled = broker.with_control_targets(vec![]).unwrap();
    let p = proposal("disabled", disabled.surface_epoch(), "brokerd-test");
    assert_eq!(
        disabled
            .authorize_control(&context("caller"), None, None, p)
            .await
            .unwrap()
            .outcome,
        ControlOutcome::Denied
    );
}

#[tokio::test]
async fn controls_durable_replay_restart_changed_policy_and_global_invocation_collision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audit.jsonl");
    let audit = Arc::new(FileAuditLog::open(&path, 32, 65536).await.unwrap());
    let first = build(&policies(true, true), audit.clone(), vec![], 32).await;
    let epoch = first.surface_epoch().clone();
    let p = proposal("replay", &epoch, "brokerd-test");
    assert_eq!(
        first
            .authorize_control(&context("caller"), None, None, p.clone())
            .await
            .unwrap()
            .outcome,
        ControlOutcome::Admitted
    );
    let mut denied = p.clone();
    denied.id = "denied-replay".parse().unwrap();
    denied.to.model = "unknown".parse().unwrap();
    assert_eq!(
        first
            .authorize_control(&context("caller"), None, None, denied.clone())
            .await
            .unwrap()
            .outcome,
        ControlOutcome::Denied
    );
    drop(first);
    drop(audit);
    let audit = Arc::new(FileAuditLog::open(&path, 32, 65536).await.unwrap());
    let ids = audit.take_replay_ids().await;
    assert_eq!(ids.len(), 2);
    let second = build(&policies(false, false), audit.clone(), ids, 32).await;
    assert_ne!(second.surface_epoch(), &epoch);
    for mut p in [p, denied] {
        p.surface_epoch = second.surface_epoch().clone();
        assert_eq!(
            second
                .authorize_control(&context("caller"), None, None, p)
                .await
                .unwrap()
                .outcome,
            ControlOutcome::Denied
        );
    }
    // A cached earlier surface/admission is never used after permissions changed at restart.
    let p = proposal("fresh-after-change", second.surface_epoch(), "brokerd-test");
    assert_eq!(
        second
            .authorize_control(&context("caller"), None, None, p)
            .await
            .unwrap()
            .outcome,
        ControlOutcome::Denied
    );
    let inv = second
        .invoke(&context("caller"), None, None, request("replay"))
        .await
        .unwrap();
    assert_eq!(inv.outcome, InvocationOutcome::Denied);
    let p = proposal("provider-first", second.surface_epoch(), "brokerd-test");
    second
        .invoke(&context("caller"), None, None, request("provider-first"))
        .await
        .unwrap();
    assert_eq!(
        second
            .authorize_control(&context("caller"), None, None, p)
            .await
            .unwrap()
            .outcome,
        ControlOutcome::Denied
    );
    dekopon_brokerd::verify_audit_file(&path).unwrap();
}

#[tokio::test]
async fn controls_capacity_and_audit_failure_never_return_admission() {
    let audit = Arc::new(InMemoryAuditLog::new(1).unwrap());
    let broker = build(&policies(true, true), audit, vec![], 32).await;
    let p = proposal("first", broker.surface_epoch(), "brokerd-test");
    broker
        .authorize_control(&context("caller"), None, None, p)
        .await
        .unwrap();
    let p = proposal("audit-full", broker.surface_epoch(), "brokerd-test");
    let error = broker
        .authorize_control(&context("caller"), None, None, p)
        .await
        .unwrap_err();
    assert!(matches!(error, BrokerError::DecisionAudit { .. }));
    assert!(error.capacity_failure_code().is_some());
    let audit = Arc::new(InMemoryAuditLog::new(32).unwrap());
    let broker = build(&policies(true, true), audit, vec![], 1).await;
    let p = proposal("first", broker.surface_epoch(), "brokerd-test");
    broker
        .authorize_control(&context("caller"), None, None, p)
        .await
        .unwrap();
    let p = proposal("replay-full", broker.surface_epoch(), "brokerd-test");
    assert!(matches!(
        broker
            .authorize_control(&context("caller"), None, None, p)
            .await
            .unwrap_err(),
        BrokerError::ReplayLedgerFull { .. }
    ));
}

#[tokio::test]
async fn controls_unmapped_socket_and_old_envelopes_never_dispatch() {
    for mapped in [false, true] {
        let audit = Arc::new(InMemoryAuditLog::new(8).unwrap());
        let broker = Arc::new(build(&policies(true, true), audit.clone(), vec![], 8).await);
        let p = proposal("unmapped-peer", broker.surface_epoch(), "brokerd-test");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("socket");
        let listener = bind_fixture(&path);
        let identities = if mapped {
            BTreeMap::from([(
                current_uid(),
                MappedPeer {
                    context: context("caller"),
                    attestor: None,
                },
            )])
        } else {
            BTreeMap::new()
        };
        let server = BrokerServer::new(broker, identities, server_limits()).unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.serve(listener, shutdown_on(stopped)));
        if mapped {
            for version in ["dekopon.dev/broker/v1alpha1", "dekopon.dev/broker/v1alpha2"] {
                let mut stream = UnixStream::connect(&path).await.unwrap();
                write_frame(&mut stream, &json!({"apiVersion": version, "request": {"operation": "authorizeControl", "proposal": p}}), server_limits().frame).await.unwrap();
                let response: ResponseEnvelope = read_frame(&mut stream, server_limits().frame)
                    .await
                    .unwrap();
                assert!(
                    matches!(response.response, BrokerResponse::Error { code, .. } if code == ERROR_INVALID_REQUEST)
                );
            }
        } else {
            assert!(
                matches!(send(&path, p, None).await, BrokerResponse::Error { code, .. } if code == ERROR_UNAUTHENTICATED)
            );
        }
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(audit.records().await.is_empty());
    }
}

#[tokio::test]
async fn controls_real_service_checkpoint_precedes_admission_and_failure_poisons_followups() {
    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = directory.path().join("broker.yaml");
    let socket_path = directory.path().join("broker.sock");
    let audit_path = directory.path().join("audit.jsonl");
    let checkpoint_path = directory.path().join("checkpoint.json");
    let policy_path = directory.path().join("policy.cedar");
    write_owner_only(&policy_path, policies(true, true).as_bytes());
    let config = json!({
        "apiVersion": CONFIG_API_VERSION, "socketPath": socket_path, "auditPath": audit_path,
        "checkpointPath": checkpoint_path, "checkpointLockPath": directory.path().join("checkpoint.lock"),
        "brokerPrincipal": "broker", "policyRevision": "control-test", "policiesPath": policy_path,
        "providers": [provider_fixture("echo-provider.wasm")], "controlTargets": targets(),
        "identities": [{"uid": current_uid(), "principal": "caller", "actor": {"type": "agent", "agent": "brokerd-test"}}],
        "identityMappings": [{"subject": subject(), "principal": "cpetersen"}],
        "constraintSets": {"echo.echo": echo_constraint_set()},
    });
    write_owner_only(&config_path, &serde_json::to_vec(&config).unwrap());
    let (stop, stopped) = oneshot::channel();
    let mut task = tokio::spawn(run(config_path, shutdown_on(stopped)));
    wait_for_socket(&socket_path, &mut task).await;
    let client = BrokerClient::new(&socket_path, current_uid(), FrameLimits::default()).unwrap();
    let epoch = client.session_surface(None).await.unwrap().3;
    let mut live = client
        .control_client(scope("brokerd-test"), epoch.clone(), None, 0)
        .unwrap();
    let accepted = live
        .authorize(
            1,
            "persisted".parse().unwrap(),
            selection("baseline", Effort::Low),
            selection("gpt-5.6-sol", Effort::Low),
            "trace-control".parse().unwrap(),
            None,
        )
        .await
        .unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(&checkpoint_path).unwrap()).unwrap();
    assert_eq!(saved["records"], 1);
    let verification = dekopon_brokerd::verify_audit_file(&audit_path).unwrap();
    assert_eq!(verification.head.as_deref(), saved["head"].as_str());
    assert_eq!(accepted.consume(), ControlOutcome::Admitted);
    let obstruction = directory.path().join("checkpoint.json.tmp");
    fs::create_dir(&obstruction).unwrap();
    let failed = live
        .authorize(
            2,
            "not-admitted".parse().unwrap(),
            selection("baseline", Effort::Low),
            selection("gpt-5.6-sol", Effort::Low),
            "trace-control".parse().unwrap(),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(failed, ClientError::Remote { code, .. } if code == ERROR_BROKER_UNAVAILABLE));
    assert_eq!(
        dekopon_brokerd::verify_audit_file(&audit_path)
            .unwrap()
            .records,
        2
    );
    let saved: Value = serde_json::from_slice(&fs::read(&checkpoint_path).unwrap()).unwrap();
    assert_eq!(
        saved["records"], 1,
        "failed persistence did not pretend to checkpoint"
    );
    // Even a new client cannot escape the poisoned deployed audit wrapper.
    let mut fresh = client
        .control_client(scope("brokerd-test"), epoch, None, 0)
        .unwrap();
    assert!(
        matches!(fresh.authorize(1, "after-poison".parse().unwrap(), selection("baseline", Effort::Low),
        selection("gpt-5.6-sol", Effort::Low), "trace-control".parse().unwrap(), None).await,
        Err(ClientError::Remote { code, .. }) if code == ERROR_BROKER_UNAVAILABLE)
    );
    fs::remove_dir(obstruction).unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn controls_audit_fields_are_correlated_admission_only_and_prompt_permission_is_required() {
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::prelude::*;
    let capture = dekopon_test_support::CaptureLayer::with_target_prefix("dekopon_broker");
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let audit = Arc::new(InMemoryAuditLog::new(8).unwrap());
    let policy = policies(true, true)
        .lines()
        .filter(|line| !line.contains("agent.prompt"))
        .collect::<Vec<_>>()
        .join("\n");
    let broker = build(&policy, audit.clone(), vec![], 8).await;
    let p = proposal("prompt-required", broker.surface_epoch(), "brokerd-test");
    let denied = broker
        .authorize_control(&context("caller"), None, None, p.clone())
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert_eq!(denied.outcome, ControlOutcome::Denied);
    let records = audit.records().await;
    let AuditEvent::ControlDecision {
        proposal: bound,
        reason,
        decision_ref,
        allowed,
        ..
    } = &records[0].event
    else {
        panic!()
    };
    assert_eq!(bound, &p);
    assert!(!allowed);
    assert_eq!(reason.as_deref(), Some("agent-denied"));
    assert_eq!(decision_ref, &denied.decision_ref);
    let events = capture.events_text();
    for field in [
        "broker.control.decision",
        "control=prompt-required",
        "job=job-one",
        "request=request-one",
        "session=session-one",
        "generation=generation-one",
        "sequence=1",
        "from_model=baseline",
        "to_model=gpt-5.6-sol",
        "from_effort=low",
        "to_effort=low",
        "admitted=false",
        "reason=\"agent-denied\"",
        "decision_ref=sha256:",
    ] {
        assert!(events.contains(field), "missing {field}: {events}");
    }
    for forbidden in [
        "credential",
        "endpoint",
        "provider",
        "input_tokens",
        "output_tokens",
    ] {
        assert!(!events.contains(forbidden));
    }
}
