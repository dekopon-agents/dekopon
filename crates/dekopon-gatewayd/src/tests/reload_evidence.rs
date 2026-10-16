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
}
