use super::*;

#[tokio::test(start_paused = true)]
async fn an_unchanged_configuration_emits_no_indicator_records_on_reload_ticks() {
    use dekopon_broker_protocol::{BrokerResponse, GenerationOutcome, ProtocolVersion};
    use dekopon_test_support::BrokerFixture;

    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    let path = write_config(root.path(), &config);
    let (capture, _guard) = capture_spans();
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let expected = candidate
        .config
        .routes
        .iter()
        .map(|route| route.indicators.len())
        .sum::<usize>();
    assert!(expected > 0);
    let mut broker = BrokerFixture::answering(
        root.path(),
        (0..3).map(|_| ResponseEnvelope {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Generation {
                digest: "b".repeat(64),
                outcome: GenerationOutcome::Active,
            },
        }),
    );
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path, candidate, "b".repeat(64))
    .unwrap();
    let records = || {
        capture.events_where(
            "dekopon_gatewayd",
            &[("event", "gateway_indicators_effective")],
        )
    };
    assert_eq!(records().len(), expected);
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.reset();
    for _ in 0..3 {
        tick.tick().await;
        gateway.reconcile().await;
        assert!(matches!(
            broker.request().await.request,
            BrokerRequest::PrepareGeneration { .. }
        ));
        assert_eq!(records().len(), expected);
    }
}

#[tokio::test]
async fn session_spans_keep_the_generation_activated_for_each_turn() {
    use dekopon_broker_protocol::{BrokerResponse, GenerationOutcome, ProtocolVersion};
    use dekopon_test_support::{BrokerFixture, Record};

    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    config["routes"][0]["conversation"]["kind"] = json!(["directMessage", "groupDirectMessage"]);
    let mut channel_route = config["routes"][0].clone();
    channel_route["conversation"]["kind"] = json!(["channel", "thread"]);
    config["routes"].as_array_mut().unwrap().push(channel_route);
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let mut broker = BrokerFixture::answering(
        root.path(),
        [ResponseEnvelope {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Generation {
                digest: "b".repeat(64),
                outcome: GenerationOutcome::Active,
            },
        }],
    );
    let turn_root = temporary();
    let (session_broker, _) = stub_broker(
        turn_root.path(),
        vec![
            ResponseEnvelope::capabilities(Vec::new(), Vec::new(), BTreeMap::new()),
            ResponseEnvelope::capabilities(Vec::new(), Vec::new(), BTreeMap::new()),
        ],
    )
    .await;
    let expected_indicators = candidate
        .config
        .routes
        .iter()
        .map(|route| route.indicators.len())
        .sum::<usize>();
    let runner = runner(
        session_broker,
        ModelScript::new([answer("old"), answer("new")]),
        4,
    );
    let driver = Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>;
    let mut drivers = BTreeMap::new();
    drivers.insert("dev".to_owned(), driver);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(drivers),
        Arc::new(Vec::new()),
    )
    .with_reload(path.clone(), candidate, "b".repeat(64))
    .unwrap();
    let old_digest = gateway.digest_for_test();
    let old_generation = gateway.generation_for_test();
    let (capture, _guard) = capture_spans();
    let mut sessions = tokio::task::JoinSet::new();
    gateway.start_with_generation_for_test(&mut sessions, &old_generation, message("old"));
    sessions.join_next().await.unwrap().unwrap();
    config["routes"][0]["steering"] = json!("boundary");
    for route in config["routes"].as_array_mut().unwrap() {
        route["indicators"] = json!({"mode": "off"});
    }
    write_config(root.path(), &config);
    gateway.reconcile().await;
    assert!(matches!(
        broker.request().await.request,
        BrokerRequest::PrepareGeneration { .. }
    ));
    let indicators = capture.events_where(
        "dekopon_gatewayd",
        &[("event", "gateway_indicators_effective")],
    );
    assert_eq!(indicators.len(), expected_indicators);
    assert!(
        indicators
            .iter()
            .all(|record| record.field("mode") == Some("Off"))
    );
    let new_digest = gateway.digest_for_test();
    assert_ne!(old_digest, new_digest);
    let new_generation = gateway.generation_for_test();
    gateway.start_with_generation_for_test(&mut sessions, &new_generation, message("new"));
    sessions.join_next().await.unwrap().unwrap();
    let spans = capture
        .records()
        .into_iter()
        .filter(|record| {
            matches!(
                record,
                Record::Span {
                    name: "gateway.session",
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(spans.len(), 2);
    assert_eq!(
        spans[0].field("generation.digest"),
        Some(old_digest.as_str())
    );
    assert_eq!(
        spans[1].field("generation.digest"),
        Some(new_digest.as_str())
    );
}

#[tokio::test]
async fn unsafe_reload_refusal_and_monitor_failure_are_not_activation() {
    use dekopon_test_support::BrokerFixture;
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path.clone(), candidate, "b".repeat(64))
    .unwrap();
    let old_digest = gateway.digest_for_test();
    const SENTINEL: &str = "vendor-config-secret-sentinel-397";
    config["routes"][0]["agent"] = json!(SENTINEL);
    write_config(root.path(), &config);
    let (capture, _guard) = capture_spans();
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].field("applied"), Some("false"));
    assert_eq!(refused[0].field("problems"), Some("1"));
    assert!(refused[0].field("error").unwrap().len() <= 512);
    assert!(!format!("{refused:?}").contains(SENTINEL));
    assert_eq!(gateway.digest_for_test(), old_digest);
    assert!(
        capture
            .events_where("dekopon_gatewayd", &[("event", "gateway_reload_applied")])
            .is_empty()
    );
    config["routes"][0]["agent"] = json!("reviewer");
    config["routes"][0]["steering"] = json!("boundary");
    write_config(root.path(), &config);
    let _broker = BrokerFixture::answering(root.path(), []);
    gateway.reconcile().await;
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[("event", "gateway_reload_monitor_failed")]
            )
            .len(),
        1
    );
    assert!(
        capture
            .events_where("dekopon_gatewayd", &[("event", "gateway_reload_applied")])
            .is_empty()
    );
    assert_eq!(gateway.digest_for_test(), old_digest);
    gateway.reconcile().await;
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[("event", "gateway_reload_monitor_failed")]
            )
            .len(),
        1
    );
    assert!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[("event", "gateway_indicators_effective")]
            )
            .is_empty()
    );
}

#[tokio::test]
async fn repeated_invalid_gateway_sources_emit_once_until_edit_or_repair() {
    use dekopon_broker_protocol::{BrokerResponse, GenerationOutcome, ProtocolVersion};
    use dekopon_test_support::BrokerFixture;
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path, candidate, "b".repeat(64))
    .unwrap();
    let active = gateway.digest_for_test();
    let (capture, _guard) = capture_spans();
    config["routes"][0]["agent"] = json!("unknown-first");
    write_config(root.path(), &config);
    for _ in 0..3 {
        gateway.reconcile().await;
    }
    let refused =
        || capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused().len(), 1);
    config["routes"][0]["agent"] = json!("unknown-second");
    write_config(root.path(), &config);
    gateway.reconcile().await;
    assert_eq!(refused().len(), 2);
    config["routes"][0]["agent"] = json!("reviewer");
    write_config(root.path(), &config);
    let mut broker = BrokerFixture::answering(
        root.path(),
        [ResponseEnvelope {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Generation {
                digest: "b".repeat(64),
                outcome: GenerationOutcome::Active,
            },
        }],
    );
    gateway.reconcile().await;
    assert!(matches!(
        broker.request().await.request,
        BrokerRequest::PrepareGeneration { .. }
    ));
    let applied = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_applied")]);
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].field("category.count"), Some("0"));
    assert_eq!(applied[0].field("generation.digest"), Some(active.as_str()));
    config["routes"][0]["agent"] = json!("unknown-second");
    write_config(root.path(), &config);
    gateway.reconcile().await;
    assert_eq!(refused().len(), 3);
    assert_eq!(gateway.digest_for_test(), active);
}

#[tokio::test]
async fn rejected_budget_and_routine_values_stay_out_of_reload_and_boot_diagnostics() {
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path.clone(), candidate, "b".repeat(64))
    .unwrap();
    const KEY: &str = "untrusted budget key sentinel";
    const MODEL: &str = "untrusted-budget-model-sentinel";
    const ROUTINE: &str = "untrusted-routine-model-sentinel";
    let meter = json!({"meters": [{"kind": "fixed", "limit": 10, "period": "1d"}]});
    config["metering"] = json!({"budgets": {
        "reviewer": {"models": [MODEL], "meters": [{"kind": "fixed", "limit": 10, "period": "1d"}]}
    }});
    config["metering"]["budgets"][KEY] = meter;
    config["routes"][0]["memory"] = json!({"mode": "persistent", "policy": {
        "capture": {"user": "uncapped", "assistant": "uncapped", "toolRequest": "uncapped", "toolResult": 262144},
        "routines": [{"id": "summary", "kind": "progressiveOneShot", "model": ROUTINE,
            "parts": [{"id": "short", "maxBytes": 100}, {"id": "long", "maxBytes": 200}]}],
        "retain": ["summary"], "selection": {"mode": "fixed", "routine": "summary"}
    }});
    write_config(root.path(), &config);
    let report = crate::check(&path, None, None).await;
    assert!(report.problems.iter().any(|problem| matches!(
        problem,
        crate::GatewaydError::Config(config::ConfigError::Invalid { .. })
    )));
    let boot = dekopon_core::error_chain(&crate::GatewaydError::from_problems(report.problems));
    let (capture, _guard) = capture_spans();
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 1);
    let reason = refused[0].field("error").unwrap();
    for sentinel in [KEY, MODEL, ROUTINE] {
        assert!(!reason.contains(sentinel), "reload echoed {sentinel}");
        assert!(!boot.contains(sentinel), "boot echoed {sentinel}");
    }
    assert!(reason.contains("metering.budgets entry #"), "{reason}");
    assert!(
        reason.contains("memory routine names an unknown model"),
        "{reason}"
    );

    assert!(
        reason.contains("(agent reviewer) names an unknown model at models[0]"),
        "{reason}"
    );

    const VALID_UNSERVED: &str = "unserved-agent-sentinel";
    config["metering"] = json!({"budgets": {}});
    config["metering"]["budgets"][KEY] = json!({"models": [], "meters": []});
    config["metering"]["budgets"][VALID_UNSERVED] = json!({"models": ["missing-model"], "meters": [
        {"kind": "credit", "capacity": 10, "refill": 10, "per": "30s", "initial": 11}
    ]});
    config["routes"][0]
        .as_object_mut()
        .unwrap()
        .remove("memory");
    write_config(root.path(), &config);
    let report = crate::check(&path, None, None).await;
    let boot = dekopon_core::error_chain(&crate::GatewaydError::from_problems(report.problems));
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 2);
    let reason = refused[1].field("error").unwrap();
    assert!(
        reason.contains("entry #1 names an unknown model at models[0]"),
        "{reason}"
    );
    assert!(reason.contains("entry #2\" lists no meters"), "{reason}");
    assert!(reason.contains("entry #2\" lists no models"), "{reason}");
    assert!(
        reason.contains("meter 0 starts with more credit"),
        "{reason}"
    );
    assert!(
        reason.contains("period shorter than one minute"),
        "{reason}"
    );
    for sentinel in [KEY, VALID_UNSERVED] {
        assert!(!reason.contains(sentinel), "reload echoed {sentinel}");
        assert!(!boot.contains(sentinel), "boot echoed {sentinel}");
    }

    config["metering"]["budgets"][VALID_UNSERVED]["meters"] = json!([
        {"kind": "fixed", "limit": 0, "period": "400d"}
    ]);
    write_config(root.path(), &config);
    let report = crate::check(&path, None, None).await;
    let boot = dekopon_core::error_chain(&crate::GatewaydError::from_problems(report.problems));
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 3);
    let reason = refused[2].field("error").unwrap();
    for diagnostic in [
        "meter 0 must have a limit above zero",
        "meter 0 has a period longer than 366 days",
    ] {
        assert!(reason.contains(diagnostic), "reload missing {diagnostic}");
        assert!(boot.contains(diagnostic), "boot missing {diagnostic}");
    }
    for sentinel in [KEY, VALID_UNSERVED] {
        assert!(!reason.contains(sentinel), "reload echoed {sentinel}");
        assert!(!boot.contains(sentinel), "boot echoed {sentinel}");
    }
}

#[tokio::test]
async fn a_monitor_outcome_does_not_mask_a_later_refusal_for_the_same_sources() {
    use dekopon_broker_protocol::{BrokerResponse, GenerationOutcome, ProtocolVersion};
    use dekopon_test_support::BrokerFixture;
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path, candidate, "b".repeat(64))
    .unwrap();
    config["routes"][0]["steering"] = json!("boundary");
    write_config(root.path(), &config);
    let (capture, _guard) = capture_spans();
    gateway.reconcile().await;
    let response = ResponseEnvelope {
        api_version: ProtocolVersion::V1Alpha2,
        response: BrokerResponse::Generation {
            digest: "b".repeat(64),
            outcome: GenerationOutcome::Refused,
        },
    };
    let mut broker = BrokerFixture::answering(root.path(), [response.clone(), response]);
    gateway.reconcile().await;
    assert!(matches!(
        broker.request().await.request,
        BrokerRequest::PrepareGeneration { .. }
    ));
    gateway.reconcile().await;
    assert!(matches!(
        broker.request().await.request,
        BrokerRequest::PrepareGeneration { .. }
    ));
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[("event", "gateway_reload_monitor_failed")]
            )
            .len(),
        1
    );
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].field("reason"), Some("broker-prepare"));
}

#[tokio::test]
async fn twelve_invalid_budgets_render_eight_individual_reasons() {
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path, candidate, "b".repeat(64))
    .unwrap();
    let budgets = (0..12)
        .map(|index| {
            (
                format!("unknown-{index:02}"),
                json!({"meters": [{"kind": "fixed", "limit": 10, "period": "1d"}]}),
            )
        })
        .collect::<serde_json::Map<String, Value>>();
    config["metering"] = json!({"budgets": budgets});
    write_config(root.path(), &config);
    let (capture, _guard) = capture_spans();
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].field("problems"), Some("12"));
    let reason = refused[0].field("error").unwrap();
    assert_eq!(
        reason.matches("metering.budgets entry #").count(),
        8,
        "{reason}"
    );
    assert!(reason.ends_with("and 4 more"), "{reason}");
}

#[tokio::test]
async fn gateway_refusal_identifies_each_unknown_route_without_echoing_its_value() {
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path, candidate, "b".repeat(64))
    .unwrap();
    let mut second = config["routes"][0].clone();
    second["agent"] = json!("unknown-second-sentinel");
    config["routes"][0]["agent"] = json!("unknown-first-sentinel");
    config["routes"].as_array_mut().unwrap().push(second);
    write_config(root.path(), &config);
    let (capture, _guard) = capture_spans();
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 1);
    let reason = refused[0].field("error").unwrap();
    assert!(reason.contains("routes[0]"), "{reason}");
    assert!(reason.contains("routes[1]"), "{reason}");
    assert!(!reason.contains("unknown-first-sentinel"));
    assert!(!reason.contains("unknown-second-sentinel"));
}

#[tokio::test]
async fn a_refused_proxy_rebuild_names_the_model_and_variable_it_lacks() {
    use crate::proxy::tests::{JAIL, pki};
    use dekopon_model_token_governor::Metering;
    let pki = pki();
    let tls = pki.directory.path();
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    config["models"].as_array_mut().unwrap().push(json!({
        "name": "claude",
        "kind": "anthropic",
        "model": "claude-opus-4-1",
        "apiKeyEnv": "DEKOPON_RELOAD_WITNESS_FIRST_KEY"
    }));
    config["proxy"] = json!({
        "bind": "127.0.0.1:0",
        "tls": {
            "certFile": tls.join("tls.crt"),
            "keyFile": tls.join("tls.key"),
            "clientCaFile": tls.join("ca.crt")
        },
        "jailIdentity": JAIL,
        "guests": {"dekopon:reviewer-vm": {"agent": "reviewer", "models": ["claude"]}}
    });
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let metering = Arc::new(Metering::new(Vec::new(), Metering::system_clock()));
    let proxy = dekopon_model_proxy::ModelProxy::new(
        Vec::new(),
        std::collections::HashMap::new(),
        Arc::clone(&metering),
    )
    .unwrap();
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload_owners(
        path,
        candidate,
        "b".repeat(64),
        crate::gateway::ResidentModels {
            credentials: Arc::new(crate::session::ConfiguredModels::default()),
            metering,
            proxy: Some(Arc::new(proxy)),
        },
    )
    .unwrap();
    let old_digest = gateway.digest_for_test();
    config["models"][1]["apiKeyEnv"] = json!("DEKOPON_RELOAD_WITNESS_SECOND_KEY");
    write_config(root.path(), &config);
    let (capture, _guard) = capture_spans();
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].field("reason"), Some("proxy-model"));
    let error = refused[0]
        .field("error")
        .expect("the refusal names its reason");
    assert!(error.contains("\"claude\""), "{error}");
    assert!(
        error.contains("DEKOPON_RELOAD_WITNESS_SECOND_KEY"),
        "{error}"
    );
    const SENTINEL: &str = "auth-file-secret-sentinel";
    let auth = root.path().join("auth.json");
    fs::write(&auth, format!("{{\"version\":\"{SENTINEL}\"}}")).unwrap();
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
    config["models"][1] = json!({
        "name": "claude",
        "kind": "chatgptSubscription",
        "model": "gpt",
        "authFile": auth,
        "timeoutMs": 2000
    });
    write_config(root.path(), &config);
    gateway.reconcile().await;
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 2);
    assert_eq!(refused[1].field("reason"), Some("proxy-model"));
    let error = refused[1].field("error").unwrap();
    assert!(
        error.contains("\"claude\" credential could not be opened"),
        "{error}"
    );
    assert!(!capture.text().contains(SENTINEL), "{}", capture.text());
    assert_eq!(gateway.digest_for_test(), old_digest);
}

#[tokio::test]
async fn boot_never_echoes_auth_file_content() {
    use crate::proxy::tests::{JAIL, pki};
    const WORD: &str = "auth-file-boot-sentinel";
    const NUMBER: u32 = 3_141_592_653;
    let valid = json!({
        "version": 1,
        "access": "access-fixture",
        "refresh": "refresh-fixture",
        "expiresAt": 1,
        "accountId": "account-fixture"
    });
    let planted = [
        ("version", json!(NUMBER), NUMBER.to_string()),
        ("version", json!(WORD), WORD.to_owned()),
        ("access", json!(NUMBER), NUMBER.to_string()),
        ("expiresAt", json!(WORD), WORD.to_owned()),
    ];
    let pki = pki();
    let tls = pki.directory.path();
    for (field, value, sentinel) in planted {
        let root = temporary();
        fs::write(
            root.path().join("dekopon.yaml"),
            catalog_text(true, Some("reasoning")),
        )
        .unwrap();
        let auth = root.path().join("auth.json");
        let mut credential = valid.clone();
        credential[field] = value;
        fs::write(&auth, credential.to_string()).unwrap();
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
        let mut responses = listings(2, &[]);
        responses[1] = ResponseEnvelope {
            api_version: dekopon_broker_protocol::ProtocolVersion::V1Alpha2,
            response: dekopon_broker_protocol::BrokerResponse::Generation {
                digest: "b".repeat(64),
                outcome: dekopon_broker_protocol::GenerationOutcome::Active,
            },
        };
        let (broker, _observed) = stub_broker(root.path(), responses).await;
        let mut config = document(root.path());
        config["broker"]["serverUid"] = json!(broker.server_uid);
        config["models"].as_array_mut().unwrap().push(json!({
            "name": "codex",
            "kind": "chatgptSubscription",
            "model": "gpt",
            "authFile": auth,
            "timeoutMs": 2000
        }));
        config["proxy"] = json!({
            "bind": "127.0.0.1:0",
            "tls": {
                "certFile": tls.join("tls.crt"),
                "keyFile": tls.join("tls.key"),
                "clientCaFile": tls.join("ca.crt")
            },
            "jailIdentity": JAIL,
            "guests": {"dekopon:reviewer-vm": {"agent": "reviewer", "models": ["codex"]}}
        });
        let path = write_config(root.path(), &config);
        let (capture, guard) = capture_spans();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            crate::run(&path, std::future::pending()),
        )
        .await
        .expect("boot is bounded")
        .expect_err("an unusable auth file refuses boot");
        drop(guard);
        let rendered = dekopon_core::error_chain(&error);
        assert!(!rendered.contains(&sentinel), "{field}: {rendered}");
        let text = capture.text();
        assert!(!text.contains(&sentinel), "{field}: {text}");
        assert!(rendered.contains(auth.to_str().unwrap()), "{rendered}");
        assert!(rendered.contains(&format!("`{field}`")), "{rendered}");
    }
}

#[tokio::test]
async fn replacing_a_model_ca_at_the_same_path_activates_new_client_trust() {
    use crate::session::ModelFactory as _;
    use dekopon_broker_protocol::{BrokerResponse, GenerationOutcome, ProtocolVersion};
    use dekopon_test_support::BrokerFixture;
    let root = temporary();
    fs::write(
        root.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let first = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .unwrap()
        .cert
        .pem();
    let second = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
        .unwrap()
        .cert
        .pem();
    let ca = root.path().join("roots.pem");
    fs::write(&ca, &first).unwrap();
    let mut config = document(root.path());
    config["broker"]["serverUid"] = json!(crate::current_uid());
    config["extraCABundles"] = json!([ca]);
    let path = write_config(root.path(), &config);
    let (candidate, _) = crate::boot_candidate(&path).await.unwrap();
    let sources = gateway_sources(&candidate.config).await;
    let runner = runner(candidate.config.broker.clone(), ModelScript::forbidden(), 4);
    let mut gateway = crate::gateway::Gateway::new(
        runner,
        Arc::new(candidate.routes.clone()),
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(Vec::new()),
    )
    .with_reload(path.clone(), candidate, "b".repeat(64))
    .unwrap();
    let original = gateway.generation_for_test();
    let before = gateway.digest_for_test();
    let original_model = original.routes.models.values().next().unwrap();
    let factory = crate::session::ConfiguredModels::default();
    let runtime = tokio::runtime::Handle::current();
    let cancel = tokio::sync::watch::channel(false).1;
    let _old = factory
        .build_bound(original_model, runtime.clone(), cancel.clone())
        .unwrap();
    let old_client = factory.bound_client_for_test(original_model).unwrap();
    assert_eq!(original_model.ca_bundles()[0].expose(), &first);
    let (capture, _guard) = capture_spans();
    fs::write(&ca, &second).unwrap();
    assert!(
        !sources
            .still_current(
                &path,
                &root.path().join("dekopon.yaml"),
                crate::current_uid()
            )
            .await
            .unwrap()
    );
    let mut broker = BrokerFixture::answering(
        root.path(),
        [ResponseEnvelope {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Generation {
                digest: "b".repeat(64),
                outcome: GenerationOutcome::Active,
            },
        }],
    );
    gateway.reconcile().await;
    assert!(matches!(
        broker.request().await.request,
        BrokerRequest::PrepareGeneration { .. }
    ));
    assert_ne!(gateway.digest_for_test(), before);
    let current = gateway.generation_for_test();
    let model = current.routes.models.values().next().unwrap();
    assert_eq!(model.ca_bundles()[0].expose(), &second);
    assert_eq!(original_model.ca_bundles()[0].expose(), &first);
    let _new = factory.build_bound(model, runtime, cancel).unwrap();
    assert!(!Arc::ptr_eq(
        &old_client,
        &factory.bound_client_for_test(model).unwrap()
    ));
    let applied = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_applied")]);
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].field("categories"), Some("extraCABundles"));
    let debug = format!(
        "{:?}",
        gateway_sources(
            &crate::config::load(&path, crate::current_uid())
                .await
                .unwrap()
        )
        .await
    );
    assert!(!debug.contains("BEGIN CERTIFICATE"));
    fs::write(ca, "SENTINEL-BAD-PEM").unwrap();
    let active = gateway.digest_for_test();
    gateway.reconcile().await;
    assert_eq!(gateway.digest_for_test(), active);
    let refused = capture.events_where("dekopon_gatewayd", &[("event", "gateway_reload_refused")]);
    assert_eq!(refused.len(), 1);
    assert!(!format!("{refused:?}").contains("SENTINEL"));
}
