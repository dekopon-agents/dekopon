#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use dekopon_broker::{
    Attestation, AttestorGrant, AuthenticatedContext, Broker, BrokerLimits, CapabilityRoute,
    ChatScopeClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet, Conversation,
    ConversationKind, CredentialStore, IdentityDirectory, InMemoryAuditLog, InvocationRequest,
    PolicyEngine, PolicyWorld, Trigger,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_capability::{
    EffectKind, ExecutionConstraints, InvocationOutcome, StorageAccess, StorageConstraints,
    StorageInterface, StorageRetention, StorageScope,
};
use dekopon_core::{Actor, ExternalSubject, PrincipalId};
use dekopon_storage_host::{StorageHost, StorageLimits};
use dekopon_test_support::provider_fixture;
use serde_json::json;
use std::{
    fs::{self, File, FileTimes},
    sync::Arc,
    time::{Duration, SystemTime},
};

async fn build_broker(
    root: &std::path::Path,
    permit_bob: bool,
) -> (Broker<InMemoryAuditLog>, StorageHost) {
    let storage = StorageHost::open(root, StorageLimits::default()).unwrap();
    let registry = BrokerProviderRegistry::load_with_storage(
        [provider_fixture("storage-probe-provider.wasm")],
        BrokerHostLimits::default(),
        Some(storage.clone()),
    )
    .await
    .unwrap();
    let capability: dekopon_core::CapabilityId = "storage-probe.run".parse().unwrap();
    let provider: dekopon_core::ProviderId = "storage-probe".parse().unwrap();
    let alice: ExternalSubject = "slack.t0123abc.u9xyz".parse().unwrap();
    let bob: ExternalSubject = "slack.t0123abc.u8xyz".parse().unwrap();
    let world = PolicyWorld::new(
        [
            "alice".parse::<PrincipalId>().unwrap(),
            "bob".parse().unwrap(),
            "gateway".parse().unwrap(),
        ],
        [(capability.clone(), provider.clone())],
    )
    .unwrap();
    let policy = PolicyEngine::new(
        &format!(r#"
@id("both-can-prompt")
permit(principal,
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"reviewer")
when {{ (principal == Dekopon::Principal::"alice" || principal == Dekopon::Principal::"bob") && context.via == "gateway" }};
@id("alice-storage")
permit(principal == Dekopon::Principal::"alice",
       action == Dekopon::Action::"storage-probe.run",
       resource == Dekopon::Provider::"storage-probe")
when {{ context.via == "gateway" && context.agent == "reviewer" }};
{}
"#, if permit_bob { r#"
@id("bob-storage")
permit(principal == Dekopon::Principal::"bob",
       action == Dekopon::Action::"storage-probe.run",
       resource == Dekopon::Provider::"storage-probe")
when { context.via == "gateway" && context.agent == "reviewer" };
"# } else { "" }),
        &world,
    )
    .unwrap();
    let broker = Broker::new(
        registry,
        "broker-test".parse().unwrap(),
        "retention-test".to_owned(),
        policy,
        ConstraintCatalog::new([(
            capability,
            ConstraintSet {
                route: CapabilityRoute::Generic,
                provider,
                effect: EffectKind::LocalWrite,
                risk: dekopon_core::RiskLevel::Medium,
                credential: None,
                constraints: ExecutionConstraints {
                    storage: Some(StorageConstraints {
                        interface: StorageInterface::DurableFiles,
                        access: StorageAccess::ReadWrite,
                        scope: StorageScope::Agent,
                        retention: StorageRetention::IdleTtl(Duration::from_secs(30)),
                    }),
                    ..Default::default()
                },
            },
        )])
        .unwrap(),
        CredentialStore::empty(),
        IdentityDirectory::new([
            (alice.clone(), "alice".parse().unwrap()),
            (bob.clone(), "bob".parse().unwrap()),
        ])
        .unwrap(),
        Arc::new(InMemoryAuditLog::new(12).unwrap()),
        BrokerLimits::default(),
    )
    .unwrap();
    (broker, storage)
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_agent_storage_caller_does_not_refresh_the_shared_resource() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let (first_broker, first_storage) = build_broker(&root, true).await;
    let alice: ExternalSubject = "slack.t0123abc.u9xyz".parse().unwrap();
    let bob: ExternalSubject = "slack.t0123abc.u8xyz".parse().unwrap();
    let gateway = AuthenticatedContext::new(
        "gateway".parse().unwrap(),
        Actor::Service {
            principal: "gateway".parse().unwrap(),
        },
    )
    .unwrap();
    let invoke = |index, subject| {
        let request = InvocationRequest {
            id: format!("storage-retention-{index}").parse().unwrap(),
            capability: "storage-probe.run".parse().unwrap(),
            trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
                .parse()
                .unwrap(),
            input: json!({"mode":"quota-denial"}),
            secret_use: None,
        };
        let claim = Attestation::for_chat(
            subject,
            "reviewer".parse().unwrap(),
            ChatScopeClaim {
                transport: "scientist-slack".parse().unwrap(),
                kind: ChatTransportKind::Slack,
                conversation: Conversation {
                    kind: ConversationKind::Thread,
                    container: Some("t0123abc".to_owned()),
                    id: "c0123abc".to_owned(),
                    thread: Some("1712345678.000100".to_owned()),
                },
                trigger: Trigger::Message,
            },
        )
        .bound_to(request.id.clone());
        (request, claim)
    };
    let grant = AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    };
    let (request, claim) = invoke(0, bob.clone());
    let formerly_allowed = first_broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&claim),
            request,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        formerly_allowed.result.outcome,
        InvocationOutcome::Failed,
        "{formerly_allowed:?}"
    );
    drop(first_broker);
    drop(first_storage);
    let (broker, storage) = build_broker(&root, false).await;
    let (request, claim) = invoke(1, alice);
    let admitted = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&claim),
            request,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        admitted.result.outcome,
        InvocationOutcome::Failed,
        "{admitted:?}"
    );
    let path = fs::read_dir(root.join("namespaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("last-used");
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(60)))
        .unwrap();
    let aged = fs::metadata(&path).unwrap().modified().unwrap();
    let (request, claim) = invoke(2, bob);
    let denied = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&claim),
            request,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(denied.result.outcome, InvocationOutcome::Denied);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), aged);
    let (request, claim) = invoke(3, "slack.t0123abc.u9xyz".parse().unwrap());
    let admitted = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&claim),
            request,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        admitted.result.outcome,
        InvocationOutcome::Failed,
        "{admitted:?}"
    );
    assert!(fs::metadata(&path).unwrap().modified().unwrap() > aged);
    assert_eq!(broker.storage_retention_policies().len(), 1);
    assert_eq!(
        storage
            .sweep(&broker.storage_retention_policies())
            .unwrap()
            .deleted,
        0
    );
}
