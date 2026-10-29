use super::*;
use crate::gateway::{CurrentGeneration, Gateway};
use crate::transport::github::{GithubTransport, tests::signed};
use dekopon_broker_protocol::ResponseEnvelope;
use dekopon_test_support::{BrokerFixture, CaptureLayer};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

#[derive(Clone, Default)]
struct Admissions(Arc<Mutex<Vec<Value>>>);

impl<S> Layer<S> for Admissions
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        if event.metadata().target() != "dekopon_gatewayd::audit" {
            return;
        }
        #[derive(Default)]
        struct Fields(serde_json::Map<String, Value>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().into(), json!(value));
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().into(), json!(value));
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().into(), json!(format!("{value:?}")));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.get("audit.event") != Some(&json!("gateway.admission")) {
            return;
        }
        let native = opentelemetry::Context::current();
        let array = native
            .get::<dekopon_telemetry::EventStringArray>()
            .expect("GitHub admission has native ids");
        assert_eq!(array.name, "github.deliveries");
        fields.0.insert(array.name.into(), json!(array.values));
        if let Some(span) = context.event_span(event) {
            fields
                .0
                .insert("parent.span.id".into(), json!(span.id().into_u64()));
        }
        self.0.lock().push(Value::Object(fields.0));
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    broker: BrokerFixture,
    transport: GithubTransport,
    gateway: Gateway,
    gate: crate::session::SessionGate,
}

async fn fixture(models: Arc<dyn ModelFactory>, maximum: usize) -> Fixture {
    fixture_with_subscriptions(models, maximum, &[]).await
}

async fn fixture_with_subscriptions(
    models: Arc<dyn ModelFactory>,
    maximum: usize,
    anchors: &[InboundMessage],
) -> Fixture {
    let directory = temporary();
    fs::write(
        directory.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let mut doc = document(directory.path());
    doc["transports"][0] =
        json!({"kind":"githubWebhook","name":"github","bind":"127.0.0.1:9080","secretEnv":"PATH"});
    doc["routes"][0]["transport"] = json!("github");
    doc["routes"][0]["memory"] = json!({"mode":"oneShot"});
    doc["routes"][0]["conversation"] =
        json!({"kind":["thread"],"container":"owner","ids":["owner.repo"]});
    doc["routes"][0]["githubEvents"] = json!([
        "pull_request",
        "workflow_run",
        "issue_comment",
        "pull_request_review",
        "check_run",
        "push"
    ]);
    if !anchors.is_empty() {
        doc["sessions"]["wakes"] = json!({"path":directory.path().join("wakes/wakes.jsonl")});
        doc["transports"].as_array_mut().unwrap().push(json!({"name":"scientist-slack","kind":"slackSocketMode","appTokenEnv":"APP","botTokenEnv":"BOT"}));
        doc["routes"].as_array_mut().unwrap().push(json!({"transport":"scientist-slack","agent":"reviewer","conversation":{"kind":["thread"]},"memory":{"mode":"oneShot"},"wakes":true}));
    }
    let config = resolved(directory.path(), &doc).await;
    let routes = Arc::new(RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap());
    let broker = BrokerFixture::scripted(directory.path(), |_, _| {
        Some((
            ResponseEnvelope::chat_capabilities(
                vec![capability("repo.read")],
                vec![],
                BTreeMap::new(),
                None,
                Some("repo-principal".parse().unwrap()),
            ),
            Vec::new(),
        ))
    });
    let runner = runner_with(
        ResolvedBroker {
            socket_path: broker.socket().to_owned(),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        models,
        maximum,
    );
    let mut runner = Arc::into_inner(runner).expect("sole runner owner");
    if !anchors.is_empty() {
        let store = wakes::wake_store(directory.path());
        let target = crate::pull_request_links::PullRequestLinkContext::parse(
            "https://github.com/owner/repo/pull/7",
        )
        .unwrap()
        .unwrap()
        .targets
        .remove(0);
        for message in anchors {
            store
                .subscribe(
                    crate::wake::Anchor::from_inbound(message, &"reviewer".parse().unwrap())
                        .unwrap()
                        .as_space()
                        .unwrap(),
                    target.clone(),
                    SystemTime::now(),
                )
                .unwrap();
        }
        runner.wakes = Some(store);
    }
    let runner = Arc::new(runner);
    let gate = runner.gate.clone();
    let generation = CurrentGeneration::boot(routes, Arc::default());
    let transport = GithubTransport::new(
        "github".into(),
        "127.0.0.1:9080".parse().unwrap(),
        "secret".into(),
        generation.clone(),
        None,
    );
    let mut drivers = BTreeMap::from([("github".into(), transport.driver())]);
    if !anchors.is_empty() {
        drivers.insert(
            "scientist-slack".into(),
            Arc::new(RecordingDriver::default()),
        );
    }
    let gateway = Gateway::serving(runner, generation, Arc::default(), Arc::new(drivers));
    Fixture {
        _directory: directory,
        broker,
        transport,
        gateway,
        gate,
    }
}

fn body(event: &str, number: u64, actor: u64) -> Value {
    let mut body = crate::transport::github::tests::sample(event);
    body["sender"]["id"] = json!(actor);
    body["number"] = json!(number);
    body["pull_request"]["number"] = json!(number);
    body["issue"]["number"] = json!(number);
    body["issue"]["pull_request"] = json!({});
    body["check_run"]["pull_requests"] = json!([{"number":number,"base":{"repo":{"id":10}}}]);
    body["action"] = json!(if event == "pull_request" {
        "opened"
    } else {
        "created"
    });
    body
}

async fn post(
    fixture: &mut Fixture,
    sessions: &mut tokio::task::JoinSet<()>,
    id: &str,
    event: &str,
    body: &Value,
) -> axum::http::StatusCode {
    let bytes = serde_json::to_vec(body).unwrap();
    let request = fixture
        .transport
        .request_for_test(signed(&bytes, b"secret", id, event));
    let deliver = async {
        let TransportEvent::Delivery(message, ticket) = fixture.transport.delivery_for_test().await
        else {
            panic!("delivery")
        };
        fixture
            .gateway
            .dispatch_delivery(sessions, *message, ticket);
    };
    let (response, ()) = tokio::join!(request, deliver);
    response.status()
}

#[tokio::test(start_paused = true)]
async fn a_burst_at_one_pull_request_runs_as_one_coalesced_turn() {
    let models = InterruptibleModel::new((0..5).map(|_| answer("report")));
    let mut fixture = fixture(Arc::new(Arc::clone(&models)), 8).await;
    let capture = CaptureLayer::default();
    let admissions = Admissions::default();
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(capture.clone())
            .with(admissions.clone()),
    );
    let mut sessions = tokio::task::JoinSet::new();
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "first",
            "issue_comment",
            &body("issue_comment", 7, 1)
        )
        .await,
        axum::http::StatusCode::OK
    );
    models.wait_until_entered().await;
    for n in 0..10 {
        let event = if n % 2 == 0 {
            "issue_comment"
        } else {
            "pull_request_review"
        };
        assert_eq!(
            post(
                &mut fixture,
                &mut sessions,
                &format!("person-{n}"),
                event,
                &body(event, 7, 1)
            )
            .await,
            axum::http::StatusCode::OK
        );
    }
    for n in 0..5 {
        assert_eq!(
            post(
                &mut fixture,
                &mut sessions,
                &format!("bot-{n}"),
                "check_run",
                &body("check_run", 7, 2)
            )
            .await,
            axum::http::StatusCode::OK
        );
    }
    for number in [8, 9] {
        assert_eq!(
            post(
                &mut fixture,
                &mut sessions,
                &format!("pr-{number}"),
                "issue_comment",
                &body("issue_comment", number, 1)
            )
            .await,
            axum::http::StatusCode::OK
        );
    }
    while models.requests() < 3 {
        sessions.join_next().await.unwrap().unwrap();
    }
    assert_eq!(models.requests(), 3, "other PRs run while #7 waits");
    let queued = admissions
        .0
        .lock()
        .iter()
        .rfind(|row| row["outcome"] == "queued" && row["conversation.id"] == "owner.repo:7")
        .unwrap()
        .clone();
    assert_eq!(queued["queue.depth"], 2);
    assert_eq!(
        queued["github.deliveries"],
        json!((0..5).map(|n| format!("bot-{n}")).collect::<Vec<_>>())
    );
    models.release();
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    assert_eq!(models.requests(), 5);
    assert_eq!(models.interrupted.load(Ordering::SeqCst), 0);
    for (index, prefix, count) in [(3, "person", 10), (4, "bot", 5)] {
        let note = models
            .prompt(index)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n");
        let positions = (0..count)
            .map(|n| note.find(&format!("delivery {prefix}-{n}.")).unwrap())
            .collect::<Vec<_>>();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
    assert!(
        capture
            .events_where(
                "dekopon_agent",
                &[
                    ("audit.event", "accounting.model.turn"),
                    ("outcome", "steered")
                ]
            )
            .is_empty()
    );
    let bytes = serde_json::to_vec(&body("issue_comment", 10, 1)).unwrap();
    let original =
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "reserved", "issue_comment"));
    tokio::pin!(original);
    let event = tokio::select! { biased; _ = &mut original => panic!("must await ticket"), event = fixture.transport.delivery_for_test() => event };
    assert_eq!(
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "reserved", "issue_comment"))
            .await
            .status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let TransportEvent::Delivery(_, ticket) = event else {
        panic!("ticket")
    };
    ticket.answer(crate::transport::github::Acked::Ignored);
    assert_eq!(original.await.status(), axum::http::StatusCode::OK);
    sessions.abort_all();
    while sessions.join_next().await.is_some() {}
}

#[tokio::test(start_paused = true)]
async fn a_delivery_past_the_global_limit_is_refused_and_redelivers() {
    let model = Arc::new(dekopon_test_support::ScriptedStreamModel::scripted(
        Vec::new(),
        answer("report"),
    ));
    let mut fixture = fixture(Arc::new(Arc::clone(&model)), 8).await;
    let admissions = Admissions::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(admissions.clone()));
    let mut sessions = tokio::task::JoinSet::new();
    for n in 1..=8 {
        assert_eq!(
            post(
                &mut fixture,
                &mut sessions,
                &format!("pr-{n}"),
                "issue_comment",
                &body("issue_comment", n, 1)
            )
            .await,
            axum::http::StatusCode::OK
        );
        model.wait_until_asked().await;
    }
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "ninth",
            "issue_comment",
            &body("issue_comment", 9, 1)
        )
        .await,
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    sessions.join_next().await.unwrap().unwrap();
    model.release_next();
    sessions.join_next().await.unwrap().unwrap();
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "ninth",
            "issue_comment",
            &body("issue_comment", 9, 1)
        )
        .await,
        axum::http::StatusCode::OK
    );
    for _ in 0..8 {
        model.release_next();
    }
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    assert_eq!(
        fixture
            .transport
            .request_for_test(signed(
                &serde_json::to_vec(&body("issue_comment", 9, 1)).unwrap(),
                b"secret",
                "ninth",
                "issue_comment"
            ))
            .await
            .status(),
        axum::http::StatusCode::OK
    );
    assert_eq!(
        fixture.broker.recorded().len(),
        9,
        "same GUID executes exactly once after redelivery"
    );
    let records = admissions.0.lock();
    assert!(
        records
            .iter()
            .any(|row| row["outcome"] == "busy" && row["github.deliveries"] == json!(["ninth"]))
    );
    assert!(
        records
            .iter()
            .any(|row| row["outcome"] == "started" && row["github.deliveries"] == json!(["pr-1"]))
    );
}

#[tokio::test(start_paused = true)]
async fn a_repositoryless_event_is_acknowledged_and_ignored() {
    let mut fixture = fixture(Arc::new(ModelScript::forbidden()), 8).await;
    let admissions = Admissions::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(admissions.clone()));
    for (event, body, status) in [
        ("ping", json!({}), axum::http::StatusCode::OK),
        (
            "new_event",
            json!({"repository":{}}),
            axum::http::StatusCode::OK,
        ),
        (
            "pull_request",
            json!({"repository":{"id":10,"owner":{"login":"owner"},"name":"repo"},"sender":{"id":1},"action":"opened","pull_request":{"head":{"sha":"a".repeat(40)}}}),
            axum::http::StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            fixture
                .transport
                .request_for_test(signed(
                    &serde_json::to_vec(&body).unwrap(),
                    b"secret",
                    event,
                    event
                ))
                .await
                .status(),
            status
        );
    }
    let ignored = admissions.0.lock();
    assert_eq!(ignored.len(), 2);
    assert!(ignored.iter().all(|row| row["outcome"] == "ignored"));
    assert_eq!(ignored[0]["github.deliveries"], json!(["ping"]));
    assert_eq!(ignored[1]["github.deliveries"], json!(["new_event"]));
    assert!(fixture.broker.recorded().is_empty());
    assert!(fixture.transport.try_delivery_for_test().is_err());
}

#[tokio::test(start_paused = true)]
async fn three_pushes_share_the_repository_and_coalesce() {
    let models = InterruptibleModel::new([answer("first"), answer("follow-up")]);
    let mut fixture = fixture(Arc::new(Arc::clone(&models)), 8).await;
    let mut sessions = tokio::task::JoinSet::new();
    for (index, id) in ["push-1", "push-2", "push-3"].into_iter().enumerate() {
        assert_eq!(
            post(&mut fixture, &mut sessions, id, "push", &body("push", 7, 1)).await,
            axum::http::StatusCode::OK
        );
        if index == 0 {
            models.wait_until_entered().await;
        }
    }
    models.release();
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    assert_eq!(models.requests(), 2);
    let note = models
        .prompt(1)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(note.contains("repository; delivery push-2."));
    assert!(note.find("delivery push-2.").unwrap() < note.find("delivery push-3.").unwrap());
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_delivery_can_commit_late_and_a_dropped_ticket_redelivers() {
    let mut fixture = fixture(Arc::new(ModelScript::forbidden()), 8).await;
    let bytes = serde_json::to_vec(&body("issue_comment", 7, 1)).unwrap();
    let first =
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "late", "issue_comment"));
    tokio::pin!(first);
    let event = tokio::select! {biased; _ = &mut first => panic!("must wait"), event = fixture.transport.delivery_for_test() => event};
    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(
        first.await.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "late", "issue_comment"))
            .await
            .status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let TransportEvent::Delivery(_, ticket) = event else {
        panic!("ticket")
    };
    ticket.answer(crate::transport::github::Acked::Admitted);
    assert_eq!(
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "late", "issue_comment"))
            .await
            .status(),
        axum::http::StatusCode::OK
    );
    let dropped =
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "drop", "issue_comment"));
    tokio::pin!(dropped);
    let event = tokio::select! {biased; _ = &mut dropped => panic!("must wait"), event = fixture.transport.delivery_for_test() => event};
    drop(event);
    assert_eq!(
        dropped.await.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let retry =
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "drop", "issue_comment"));
    tokio::pin!(retry);
    let event = tokio::select! {biased; _ = &mut retry => panic!("must wait"), event = fixture.transport.delivery_for_test() => event};
    let TransportEvent::Delivery(_, ticket) = event else {
        panic!("ticket")
    };
    ticket.answer(crate::transport::github::Acked::Ignored);
    assert_eq!(retry.await.status(), axum::http::StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn a_batch_stops_at_sixteen_and_never_crosses_an_actor_boundary() {
    let models = InterruptibleModel::new((0..5).map(|_| answer("report")));
    let mut fixture = fixture(Arc::new(Arc::clone(&models)), 8).await;
    let mut sessions = tokio::task::JoinSet::new();
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "first",
            "issue_comment",
            &body("issue_comment", 7, 1)
        )
        .await,
        axum::http::StatusCode::OK
    );
    models.wait_until_entered().await;
    for n in 0..17 {
        assert_eq!(
            post(
                &mut fixture,
                &mut sessions,
                &format!("person-{n}"),
                "issue_comment",
                &body("issue_comment", 7, 1)
            )
            .await,
            axum::http::StatusCode::OK
        );
    }
    for (id, actor) in [("bot", 2), ("person-after-bot", 1)] {
        assert_eq!(
            post(
                &mut fixture,
                &mut sessions,
                id,
                "issue_comment",
                &body("issue_comment", 7, actor)
            )
            .await,
            axum::http::StatusCode::OK
        );
    }
    models.release();
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    assert_eq!(models.requests(), 5);
    for (index, count) in [(1, 16), (2, 1), (3, 1), (4, 1)] {
        let note = models
            .prompt(index)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(note.matches("[gateway: unverified").count(), count);
    }
}

#[tokio::test(start_paused = true)]
async fn a_closed_delivery_channel_returns_service_unavailable() {
    let fixture = fixture(Arc::new(ModelScript::forbidden()), 8).await;
    let bytes = serde_json::to_vec(&body("issue_comment", 7, 1)).unwrap();
    let request =
        fixture
            .transport
            .request_for_test(signed(&bytes, b"secret", "closed", "issue_comment"));
    drop(fixture.transport);
    assert_eq!(
        request.await.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test(start_paused = true)]
async fn a_dispatcher_ignored_delivery_keeps_its_receipt_trace() {
    use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let admissions = Admissions::default();
    let _subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("ignored-delivery")))
        .with(admissions.clone())
        .set_default();
    let models = ModelScript::new([]);
    let mut fixture = fixture(Arc::new(Arc::clone(&models)), 8).await;
    let bytes = serde_json::to_vec(&body("pull_request", 7, 1)).unwrap();
    let request = fixture.transport.request_for_test(signed(
        &bytes,
        b"secret",
        "removed-route",
        "pull_request",
    ));
    let (sender, receiver) = mpsc::channel(1);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let deliver = async {
        let event = fixture.transport.delivery_for_test().await;
        let TransportEvent::Delivery(message, _) = &event else {
            panic!("delivery")
        };
        let trace = message
            .receive_span
            .context()
            .span()
            .span_context()
            .trace_id();
        assert_ne!(trace, opentelemetry::trace::TraceId::INVALID);
        let receipt = message.receive_span.id().unwrap().into_u64();
        let doc = document(fixture._directory.path());
        let config = resolved(fixture._directory.path(), &doc).await;
        let routes =
            Arc::new(RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap());
        let gateway = Gateway::serving(
            Arc::clone(fixture.gateway.runner_for_test()),
            CurrentGeneration::boot(routes, Arc::default()),
            Arc::default(),
            Arc::new(BTreeMap::from([(
                "github".into(),
                fixture.transport.driver(),
            )])),
        );
        sender.send(event).await.unwrap();
        gateway
            .serve(
                receiver,
                async {
                    stopped.await.unwrap();
                },
                Duration::from_secs(1),
                crate::collection::Collector::new(&[], 4),
            )
            .await;
        receipt
    };
    let respond = async {
        let response = request.await;
        stop.send(()).unwrap();
        response
    };
    let (receipt, response) = tokio::join!(deliver, respond);
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let records = admissions.0.lock();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["outcome"], "ignored");
    assert_eq!(records[0]["github.deliveries"], json!(["removed-route"]));
    assert_eq!(records[0]["parent.span.id"], receipt);
}

#[tokio::test(start_paused = true)]
async fn wake_reason_returns_the_batch_the_claim_carries() {
    use dekopon_core::github::GithubEvent;
    let models = InterruptibleModel::new([
        answer("lead"),
        super::wake_reason::call(),
        answer("reported"),
    ]);
    let mut fixture = fixture(Arc::new(Arc::clone(&models)), 1).await;
    let mut sessions = tokio::task::JoinSet::new();
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "lead",
            "issue_comment",
            &body("issue_comment", 7, 1)
        )
        .await,
        axum::http::StatusCode::OK
    );
    models.wait_until_entered().await;
    for (id, event) in [
        ("first", "issue_comment"),
        ("second", "pull_request_review"),
    ] {
        assert_eq!(
            post(&mut fixture, &mut sessions, id, event, &body(event, 7, 1)).await,
            axum::http::StatusCode::OK
        );
    }
    models.release();
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    let (_, encoded) = models
        .prompt(2)
        .into_iter()
        .rev()
        .find(|(role, _)| role == "tool")
        .expect("the result reaches the model");
    let value: Value = serde_json::from_str(&encoded).expect("wake_reason returns JSON");
    assert_eq!(value["trigger"], "webhook");
    assert_eq!(value["source"], "github");
    let events: Vec<GithubEvent> = serde_json::from_value(value["events"].clone()).unwrap();
    let claims: Vec<_> = fixture
        .broker
        .recorded()
        .into_iter()
        .filter_map(|request| match request.request {
            BrokerRequest::Capabilities {
                attestation: Some(claim),
            } => claim.scope.and_then(|scope| scope.github),
            BrokerRequest::Capabilities { attestation: None }
            | BrokerRequest::Run { .. }
            | BrokerRequest::Invoke { .. }
            | BrokerRequest::RecordDeliveredTurn { .. }
            | BrokerRequest::UpcallResult { .. }
            | BrokerRequest::PrepareGeneration { .. }
            | BrokerRequest::CommitGeneration { .. }
            | BrokerRequest::AbortGeneration { .. }
            | BrokerRequest::ObserveGeneration => None,
        })
        .collect();
    assert_eq!(claims.len(), 2, "inspection opens no extra broker leg");
    let frame_events: Vec<GithubEvent> =
        serde_json::from_value(serde_json::to_value(&claims[1]).unwrap()).unwrap();
    assert_eq!(events, frame_events);
    assert_eq!(
        events
            .iter()
            .map(|event| event.delivery.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    let note = models
        .prompt(1)
        .into_iter()
        .filter(|(role, _)| role == "user")
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(note.find("delivery first.").unwrap() < note.find("delivery second.").unwrap());
    assert!(note.ends_with("Read current state through a broker-granted capability before acting. Call wake_reason for every fact."));
}

#[tokio::test]
async fn a_full_anchored_mailbox_returns_503_and_leaves_the_subscription_guid_retryable() {
    use crate::{
        collection::Dispositions,
        session::{Admit, MAILBOX_CAPACITY},
    };
    let first = owned_slack_message("first anchor", false);
    let mut second = owned_slack_message("second anchor", false);
    second.conversation.thread = Some("1700000001.000001".into());
    second.reply = ReplyTarget::Slack {
        channel: second.conversation.id.clone(),
        thread_ts: second.conversation.thread.clone(),
    };
    let anchors = [first, second];
    let models = ModelScript::new([decline_reply(), decline_reply()]);
    let mut fixture = fixture_with_subscriptions(Arc::new(Arc::clone(&models)), 4, &anchors).await;
    let mut bound = route(model_config());
    bound.transport = "scientist-slack".into();
    bound.wakes = true;
    let mut held = Vec::new();
    for anchor in &anchors {
        let mut message = owned_slack_message("held", false);
        message.conversation = anchor.conversation.clone();
        message.reply = anchor.reply.clone();
        let Admit::Admitted(admission, ..) =
            fixture
                .gate
                .admit(&bound, message, Dispositions(Vec::new()))
        else {
            panic!("anchor admitted")
        };
        held.push(admission);
    }
    for _ in 0..MAILBOX_CAPACITY {
        assert!(matches!(
            fixture.gate.admit(
                &bound,
                owned_slack_message("queued", false),
                Dispositions(Vec::new())
            ),
            Admit::Steered(_)
        ));
    }
    let mut sessions = tokio::task::JoinSet::new();
    let mut push = body("pull_request", 7, 1);
    push["action"] = json!("synchronize");
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "anchored-full",
            "pull_request",
            &push
        )
        .await,
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    assert_eq!(models.requests(), 0);
    drop(held);
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "anchored-full",
            "pull_request",
            &push
        )
        .await,
        axum::http::StatusCode::OK
    );
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    assert_eq!(models.requests(), 2);
    assert_eq!(
        fixture
            .transport
            .request_for_test(signed(
                &serde_json::to_vec(&push).unwrap(),
                b"secret",
                "anchored-full",
                "pull_request"
            ))
            .await
            .status(),
        axum::http::StatusCode::OK
    );
    assert!(fixture.transport.try_delivery_for_test().is_err());
}

#[tokio::test]
async fn a_subscription_without_an_anchor_driver_is_ignored_and_commits_its_guid() {
    let models = ModelScript::new([]);
    let mut fixture = fixture_with_subscriptions(
        Arc::new(Arc::clone(&models)),
        4,
        &[owned_slack_message("anchor", false)],
    )
    .await;
    fixture.gateway = Gateway::serving(
        Arc::clone(fixture.gateway.runner_for_test()),
        CurrentGeneration::boot(
            Arc::clone(&fixture.gateway.generation_for_test().routes),
            Arc::default(),
        ),
        Arc::default(),
        Arc::new(BTreeMap::from([(
            "github".into(),
            fixture.transport.driver(),
        )])),
    );
    let mut sessions = tokio::task::JoinSet::new();
    let mut push = body("pull_request", 7, 1);
    push["action"] = json!("synchronize");
    assert_eq!(
        post(
            &mut fixture,
            &mut sessions,
            "no-anchor-driver",
            "pull_request",
            &push
        )
        .await,
        axum::http::StatusCode::OK
    );
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
    assert_eq!(models.requests(), 0);
    assert_eq!(
        fixture
            .transport
            .request_for_test(signed(
                &serde_json::to_vec(&push).unwrap(),
                b"secret",
                "no-anchor-driver",
                "pull_request"
            ))
            .await
            .status(),
        axum::http::StatusCode::OK
    );
    assert!(fixture.transport.try_delivery_for_test().is_err());
}
