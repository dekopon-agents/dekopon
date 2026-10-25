use super::*;
use crate::transport::github::GithubTransport;
use dekopon_broker_protocol::{BrokerRequest, ResponseEnvelope, Trigger};
use dekopon_test_support::{BrokerFixture, CaptureLayer};
use tracing_subscriber::prelude::*;

fn github_message(owner: &str, repo: &str, guid: &str, number: u64) -> InboundMessage {
    let mut inbound = message(
        "[gateway: unverified GitHub workflow_run claim] associated PR None; report only; read current checks before acting.",
    );
    inbound.transport = "github".into();
    inbound.transport_kind = dekopon_broker_protocol::ChatTransportKind::Github;
    inbound.subject = ExternalSubject::github(owner, repo).unwrap();
    inbound.conversation = Conversation {
        kind: ConversationKind::Thread,
        container: Some(owner.into()),
        id: format!("{owner}.{repo}"),
        thread: Some(number.to_string()),
    };
    inbound.message_id = MessageId::Github(Box::new(crate::transport::github::GithubBatch::new(
        crate::transport::github::GithubEntry {
            delivery: guid.into(),
            event: crate::config::GithubEvent::WorkflowRun,
            action: Some("completed".into()),
            actor: dekopon_core::github::GithubId::new(1).unwrap(),
            thread: number.to_string().parse().unwrap(),
            head_sha: Some("a".repeat(40)),
            run: Some(dekopon_core::github::GithubId::new(5).unwrap()),
            conclusion: Some("success".into()),
        },
    )));
    inbound.addressed = Some(true);
    inbound.reply = ReplyTarget::GithubTrace;
    inbound
}

fn github_route() -> crate::routes::BoundRoute {
    crate::routes::BoundRoute {
        transport: "github".into(),
        conversation: ConversationMatch {
            kind: ConversationKindMatch::Kinds(vec![ConversationKind::Thread]),
            container: Some("owner".into()),
            ids: Some(vec!["owner.repo".into()]),
        },
        github_events: Some(vec![crate::config::GithubEvent::WorkflowRun]),
        ..route(model_config())
    }
}

fn github_driver() -> Arc<dyn ChatDriver> {
    GithubTransport::new(
        "github".into(),
        "127.0.0.1:0".parse().unwrap(),
        "test-secret".into(),
        crate::gateway::CurrentGeneration::boot(
            Arc::new(crate::routes::RoutingTable::default()),
            Arc::default(),
        ),
        None,
    )
    .driver()
}

#[test]
fn github_admission_keys_separate_repositories_and_numbers() {
    let gate = SessionGate::new(4);
    let route = github_route();
    let first = github_message("owner", "repo", "guid-one", 7);
    let same_guid_other_repo = github_message("other", "repo", "guid-one", 7);
    let other_guid = github_message("owner", "repo", "guid-two", 8);
    let other_owner_same_repo = github_message("owner", "other", "guid-one", 7);
    assert_ne!(
        first.conversation.key(),
        same_guid_other_repo.conversation.key()
    );
    assert_ne!(first.conversation.key(), other_guid.conversation.key());
    assert_ne!(
        first.conversation.key(),
        other_owner_same_repo.conversation.key()
    );
    let admissions = [
        first,
        same_guid_other_repo,
        other_guid,
        other_owner_same_repo,
    ]
    .into_iter()
    .map(|inbound| gate.admit(&route, inbound, crate::collection::Dispositions(vec![])))
    .collect::<Vec<_>>();
    assert!(
        admissions
            .iter()
            .all(|admit| matches!(admit, crate::session::Admit::Admitted(..)))
    );
}

#[tokio::test]
async fn github_report_claim_requires_a_fresh_prompt_grant_and_never_posts() {
    let directory = temporary();
    let mut broker = BrokerFixture::answering(
        directory.path(),
        [
            ResponseEnvelope::chat_capabilities(vec![], vec![], BTreeMap::new(), None, None),
            ResponseEnvelope::chat_capabilities(
                vec![capability("repo.read")],
                vec![],
                BTreeMap::new(),
                None,
                Some("repo-principal".parse().unwrap()),
            ),
        ],
    );
    let models = ModelScript::new([answer("Report only.")]);
    let driver = github_driver();
    let route = github_route();
    let subject = ExternalSubject::github("owner", "repo").unwrap();
    let capture = CaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let runner = runner(
        ResolvedBroker {
            socket_path: broker.socket().to_path_buf(),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        Arc::clone(&models),
        4,
    );
    let denied = github_message("owner", "repo", "guid-one", 7);
    let accepted = github_message("owner", "repo", "guid-two", 7);
    run_session(
        Arc::clone(&runner),
        route.clone(),
        denied,
        Arc::clone(&driver),
    )
    .await;
    assert_eq!(models.requests(), 0);
    run_session(runner, route, accepted, driver).await;
    assert_eq!(models.requests(), 1);
    let requests = broker.recorded();
    assert_eq!(requests.len(), 2);
    for (request, _guid) in requests.iter().zip(["guid-one", "guid-two"]) {
        let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = &request.request
        else {
            panic!("fresh attested capabilities required")
        };
        assert_eq!(claim.subject, subject);
        let scope = claim.scope.as_ref().unwrap();
        assert_eq!(scope.conversation.thread.as_deref(), Some("7"));
        assert_eq!(scope.trigger, Trigger::Message);
    }
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[
                    ("event", "gateway_session_rejected"),
                    ("reason", "unauthorized")
                ]
            )
            .len(),
        1
    );
}

#[tokio::test]
async fn github_unmapped_and_busy_deliveries_never_infer_or_post() {
    let directory = temporary();
    let mut broker = BrokerFixture::answering(
        directory.path(),
        [ResponseEnvelope::error(
            dekopon_broker_protocol::ERROR_UNAUTHENTICATED,
            "unmapped-subject",
        )],
    );
    let models = ModelScript::forbidden();
    let driver = github_driver();
    let runner = runner(
        ResolvedBroker {
            socket_path: broker.socket().to_path_buf(),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        Arc::clone(&models),
        1,
    );
    run_session(
        Arc::clone(&runner),
        github_route(),
        github_message("owner", "repo", "unmapped", 7),
        Arc::clone(&driver),
    )
    .await;
    assert_eq!(models.requests(), 0);
    let _held = runner
        .gate
        .admit_probe(("other".into(), "other".into()))
        .unwrap();
    run_session(
        runner,
        github_route(),
        github_message("owner", "repo", "busy", 7),
        driver,
    )
    .await;
    assert_eq!(models.requests(), 0);
    let requests = broker.recorded();
    assert_eq!(requests.len(), 1, "busy never connects to the broker");
    assert!(matches!(
        requests[0].request,
        BrokerRequest::Capabilities { .. }
    ));
}

#[tokio::test]
async fn github_model_failure_does_not_post_or_invoke_a_fix() {
    let directory = temporary();
    let mut broker = BrokerFixture::answering(
        directory.path(),
        [ResponseEnvelope::chat_capabilities(
            vec![capability("repo.read")],
            vec![],
            BTreeMap::new(),
            None,
            Some("repo-principal".parse().unwrap()),
        )],
    );
    let models = ModelScript::scripted([None]);
    run_session(
        runner(
            ResolvedBroker {
                socket_path: broker.socket().to_path_buf(),
                server_uid: crate::current_uid(),
                frame: FrameLimits::default(),
            },
            Arc::clone(&models),
            1,
        ),
        github_route(),
        github_message("owner", "repo", "failed", 7),
        github_driver(),
    )
    .await;
    assert_eq!(models.requests(), 1);
    let requests = broker.recorded();
    assert_eq!(
        requests.len(),
        1,
        "model failure after the capability listing makes no broker invocation"
    );
    assert!(matches!(
        requests[0].request,
        BrokerRequest::Capabilities { .. }
    ));
}
