use super::*;

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
    write_config(root.path(), &config);
    gateway.reconcile().await;
    assert!(matches!(
        broker.request().await.request,
        BrokerRequest::PrepareGeneration { .. }
    ));
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

    // Both a malformed key and a syntactically valid but unserved key must remain private
    // across every validation branch, including empty lists and invalid meter specifications.
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
