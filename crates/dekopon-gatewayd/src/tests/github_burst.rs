use super::*;
use crate::gateway::{CurrentGeneration, Gateway};
use crate::transport::github::{GithubTransport, tests::signed};
use dekopon_broker_protocol::ResponseEnvelope;
use dekopon_test_support::{BrokerFixture, CaptureLayer};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

#[derive(Clone, Default)]
struct Admissions(Arc<Mutex<Vec<Value>>>);

impl<S: tracing::Subscriber> Layer<S> for Admissions {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
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
        self.0.lock().push(Value::Object(fields.0));
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    broker: BrokerFixture,
    transport: GithubTransport,
    gateway: Gateway,
}

async fn fixture(models: Arc<dyn ModelFactory>, maximum: usize) -> Fixture {
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
    let generation = CurrentGeneration::boot(routes, Arc::default());
    let transport = GithubTransport::new(
        "github".into(),
        "127.0.0.1:9080".parse().unwrap(),
        "secret".into(),
        generation.clone(),
        None,
    );
    let gateway = Gateway::serving(
        runner,
        generation,
        Arc::default(),
        Arc::new(BTreeMap::from([("github".into(), transport.driver())])),
    );
    Fixture {
        _directory: directory,
        broker,
        transport,
        gateway,
    }
}

fn body(event: &str, number: u64, actor: u64) -> Value {
    let mut body = crate::transport::github::tests::pr();
    body["sender"]["id"] = json!(actor);
    body["number"] = json!(number);
    body["pull_request"]["number"] = json!(number);
    body["issue"] = json!({"number":number,"pull_request":{}});
    body["check_run"] = json!({"pull_requests":[{"number":number,"base":{"repo":{"id":10}}}]});
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
}

#[tokio::test(start_paused = true)]
async fn a_repositoryless_event_is_acknowledged_and_ignored() {
    let mut fixture = fixture(Arc::new(ModelScript::forbidden()), 8).await;
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
