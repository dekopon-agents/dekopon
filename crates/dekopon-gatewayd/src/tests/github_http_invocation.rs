use super::*;
use crate::{
    gateway::{CurrentGeneration, Gateway},
    transport::{TransportEvent, github::GithubTransport},
};
use dekopon_broker_protocol::{BrokerRequest, ResponseEnvelope, Trigger};
use dekopon_test_support::{BrokerFixture, CaptureLayer};
use tracing_subscriber::prelude::*;

#[tokio::test]
async fn signed_repo_delivery_reaches_only_its_fresh_broker_authorized_silent_turn() {
    let directory = temporary();
    fs::write(
        directory.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .unwrap();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let mut doc = document(directory.path());
    doc["transports"][0] = json!({"kind":"githubWebhook", "name":"github", "bind":address.to_string(), "secretEnv":"PATH"});
    doc["routes"][0]["transport"] = json!("github");
    doc["routes"][0]["memory"] = json!({"mode":"oneShot"});
    doc["routes"][0]["conversation"] =
        json!({"kind":["thread"], "container":"owner", "ids":["owner.repo"]});
    doc["routes"][0]["githubEvents"] = json!(["workflow_run"]);
    let mut second = doc["routes"][0].clone();
    second["conversation"]["ids"] = json!(["owner.other"]);
    doc["routes"].as_array_mut().unwrap().push(second);
    let mut denied = doc["routes"][0].clone();
    denied["conversation"]["ids"] = json!(["owner.denied"]);
    doc["routes"].as_array_mut().unwrap().push(denied);
    let config = resolved(directory.path(), &doc).await;
    let routes = Arc::new(RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap());
    let mut broker = BrokerFixture::scripted(directory.path(), |request, _| {
        let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = &request.request
        else {
            panic!("no other broker operation permitted")
        };
        assert_eq!(claim.agent.as_str(), "reviewer");
        let response = match claim.subject.canonical().as_str() {
            "github.owner.denied" => ResponseEnvelope::error(
                dekopon_broker_protocol::ERROR_UNAUTHENTICATED,
                "agent-denied",
            ),
            "github.owner.repo" => ResponseEnvelope::chat_capabilities(
                vec![capability("repo.read")],
                vec![],
                BTreeMap::new(),
                None,
                Some("repo-principal".parse().unwrap()),
            ),
            "github.owner.other" => ResponseEnvelope::chat_capabilities(
                vec![capability("repo.read"), capability("turso.exec")],
                vec![],
                BTreeMap::new(),
                None,
                Some("other-principal".parse().unwrap()),
            ),
            _ => ResponseEnvelope::error(
                dekopon_broker_protocol::ERROR_UNAUTHENTICATED,
                "unmapped-subject",
            ),
        };
        Some((response, Vec::new()))
    });
    let models = ModelScript::new([answer("Report one."), answer("Report two.")]);
    let runner = runner(
        ResolvedBroker {
            socket_path: broker.socket().to_path_buf(),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        Arc::clone(&models),
        4,
    );
    let generation = CurrentGeneration::boot(Arc::clone(&routes), Arc::default());
    let mut transport = GithubTransport::new(
        "github".into(),
        address,
        "secret".into(),
        generation.clone(),
        None,
    );
    let driver = transport.driver();
    transport.connect().await.unwrap();
    let gateway = Gateway::serving(
        runner,
        generation,
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::from([("github".into(), driver)])),
    );
    let capture = CaptureLayer::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let client = reqwest::Client::new();
    let mut sessions = tokio::task::JoinSet::new();
    for (guid, repo) in [
        ("first", "REPO"),
        ("second", "OTHER"),
        ("no-prompt", "DENIED"),
    ] {
        let send = signed_reload_delivery(&client, address, guid, "workflow_run", repo);
        let dispatch = async {
            let TransportEvent::Delivery(message, ticket) = transport.next().await.unwrap() else {
                panic!("signed delivery queued")
            };
            assert_eq!(message.conversation.thread.as_deref(), Some("repository"));
            gateway.dispatch_delivery(&mut sessions, *message, ticket);
        };
        let (status, ()) = tokio::join!(send, dispatch);
        assert_eq!(status, reqwest::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(5), sessions.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert_eq!(models.requests(), 2, "the denied prompt cannot infer");
    let requests = broker.recorded();
    assert_eq!(
        requests.len(),
        3,
        "no automatic fix or note invocation from reported runs"
    );
    for (request, (_guid, principal_subject)) in requests.iter().zip([
        ("first", "github.owner.repo"),
        ("second", "github.owner.other"),
        ("no-prompt", "github.owner.denied"),
    ]) {
        let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = &request.request
        else {
            panic!("fresh capabilities required")
        };
        assert_eq!(claim.subject.canonical(), principal_subject);
        assert_eq!(
            claim.scope.as_ref().unwrap().conversation.thread.as_deref(),
            Some("repository")
        );
        assert_eq!(claim.scope.as_ref().unwrap().trigger, Trigger::Message);
    }
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[
                    ("event", "gateway_session_rejected"),
                    ("reason", "attestation-refused")
                ]
            )
            .len(),
        1
    );
}
