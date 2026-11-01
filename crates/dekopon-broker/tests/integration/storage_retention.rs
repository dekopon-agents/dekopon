#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

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
use dekopon_core::{Actor, PrincipalId, Subject};
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
    build_scoped_broker(root, permit_bob, StorageScope::Agent).await
}

async fn build_scoped_broker(
    root: &std::path::Path,
    permit_bob: bool,
    scope: StorageScope,
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
    let alice: Subject = "drn:slack:user:t0123abc:u9xyz".parse().unwrap();
    let bob: Subject = "drn:slack:user:t0123abc:u8xyz".parse().unwrap();
    let world = PolicyWorld::new(
        [
            "alice".parse::<PrincipalId>().unwrap(),
            "bob".parse().unwrap(),
            "gateway".parse().unwrap(),
            "space".parse().unwrap(),
        ],
        [(capability.clone(), provider.clone())],
    )
    .unwrap();
    let policy = PolicyEngine::new(
        &format!(r#"
@id("space") permit(principal == Dekopon::Principal::"space", action, resource);
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
        policy,
        ConstraintCatalog::new([(
            capability,
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider,
                effect: EffectKind::LocalWrite,
                risk: dekopon_core::RiskLevel::Medium,
                credential: None,
                constraints: ExecutionConstraints {
                    storage: Some(StorageConstraints {
                        interface: StorageInterface::DurableFiles,
                        access: StorageAccess::ReadWrite,
                        scope,
                        retention: StorageRetention::IdleTtl(Duration::from_secs(30)),
                        namespace: None,
                    }),
                    ..Default::default()
                },
            },
        )])
        .unwrap(),
        CredentialStore::empty(),
        IdentityDirectory::new([
            (alice.clone(), "alice".parse().unwrap()),
            (
                "drn:discord:user::123".parse().unwrap(),
                "alice".parse().unwrap(),
            ),
            (
                "drn:discord:channel:456:789".parse().unwrap(),
                "space".parse().unwrap(),
            ),
            (bob.clone(), "bob".parse().unwrap()),
            (
                "drn:slack:channel:t0123abc:cspace".parse().unwrap(),
                "space".parse().unwrap(),
            ),
            (
                "drn:slack:channel:t0123abc:cspace2".parse().unwrap(),
                "space".parse().unwrap(),
            ),
        ])
        .unwrap(),
        Arc::new(InMemoryAuditLog::new(12).unwrap()),
        BrokerLimits::default(),
    )
    .unwrap();
    (broker, storage)
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_scope_is_filtered_and_refused_on_every_frame_outside_its_audience() {
    let gateway = AuthenticatedContext::new(
        "gateway".parse().unwrap(),
        Actor::Service {
            principal: "gateway".parse().unwrap(),
        },
    )
    .unwrap();
    let grant = AttestorGrant {
        namespaces: Some(vec!["drn:slack:*:t0123abc:*".parse().unwrap()]),
    };
    for scope in [StorageScope::Agent, StorageScope::Space] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap().join("storage");
        let (broker, _) = build_scoped_broker(&root, false, scope).await;
        for (index, (kind, channel, mapped)) in [
            (ConversationKind::DirectMessage, "d0123abc", false),
            (ConversationKind::Channel, "cunmapped", false),
            (ConversationKind::Channel, "cspace", true),
            (ConversationKind::Thread, "cspace", true),
            (ConversationKind::Channel, "cspace2", true),
        ]
        .into_iter()
        .enumerate()
        {
            let request = InvocationRequest {
                id: format!("scope-check-{index}").parse().unwrap(),
                capability: "storage-probe.run".parse().unwrap(),
                trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
                    .parse()
                    .unwrap(),
                input: json!({"mode":"success"}),
                secret_use: None,
            };
            let claim = Attestation::for_chat(
                dekopon_core::AttestedSubject::one(
                    "drn:slack:user:t0123abc:u9xyz".parse().expect("subject"),
                )
                .unwrap(),
                "reviewer".parse().unwrap(),
                ChatScopeClaim {
                    participant: None,
                    github: None,
                    transport: "scientist-slack".parse().unwrap(),
                    kind: ChatTransportKind::Slack,
                    conversation: Conversation {
                        kind,
                        container: Some("t0123abc".to_owned()),
                        id: channel.to_owned(),
                        thread: Some("1712345678.000100".to_owned()),
                    },
                    trigger: Trigger::Message,
                    delivery: None,
                },
            )
            .bound_to(request.id.clone());
            let allowed = match scope {
                StorageScope::Agent => !mapped,
                StorageScope::Space => mapped,
                StorageScope::PrivateConversation | StorageScope::SharedConversation => {
                    unreachable!()
                }
            };
            let surface = broker
                .session_surface(&gateway, Some(&grant), Some(&claim))
                .unwrap_or_else(|_| panic!("session surface refused"));
            assert_eq!(surface.0.len(), usize::from(allowed));
            assert_eq!(
                broker.space_grants(&gateway, Some(&grant), Some(&claim)),
                mapped.then(|| if allowed {
                    vec!["storage-probe.run".parse().unwrap()]
                } else {
                    vec![]
                })
            );
            let (host, mut stdout) = std::os::unix::net::UnixStream::pair().unwrap();
            let capture = tokio::task::spawn_blocking(move || {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut stdout, &mut bytes).unwrap();
                bytes
            });
            if mapped && scope == StorageScope::Space {
                let mut other = claim.clone();
                other.subjects = dekopon_core::AttestedSubject::one(
                    "drn:slack:user:t0123abc:u8xyz".parse().unwrap(),
                )
                .unwrap();
                assert!(
                    broker
                        .session_surface(&gateway, Some(&grant), Some(&other))
                        .unwrap_or_else(|_| panic!("surface"))
                        .0
                        .is_empty()
                );
                assert_eq!(
                    broker.space_grants(&gateway, Some(&grant), Some(&other)),
                    Some(vec!["storage-probe.run".parse().unwrap()])
                );
            }
            let result = broker
                .invoke(
                    &gateway,
                    Some(&grant),
                    Some(&claim),
                    request,
                    dekopon_broker_host::asset::AssetInputs {
                        streams: Some(dekopon_broker_host::Streams {
                            stdin: None,
                            stdout: host.into(),
                        }),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let output = capture.await.unwrap();
            assert_eq!(result.result.outcome == InvocationOutcome::Denied, !allowed);
            if scope == StorageScope::Space && kind == ConversationKind::Channel && mapped {
                assert_eq!(
                    result.result.outcome,
                    InvocationOutcome::Succeeded,
                    "{result:?}"
                );
            }
            if index == 0 && scope == StorageScope::Agent {
                assert_eq!(
                    result.result.outcome,
                    InvocationOutcome::Succeeded,
                    "{result:?}"
                );
                let output: serde_json::Value = serde_json::from_slice(&output).unwrap();
                assert_eq!(output["shortReadBytes"], 3);
            }
        }
        assert!(
            broker
                .session_surface(&gateway, None, None)
                .unwrap_or_else(|_| panic!("console surface refused"))
                .0
                .is_empty()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_agent_storage_caller_does_not_refresh_the_shared_resource() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let (first_broker, first_storage) = build_broker(&root, true).await;
    let alice: Subject = "drn:slack:user:t0123abc:u9xyz".parse().unwrap();
    let bob: Subject = "drn:slack:user:t0123abc:u8xyz".parse().unwrap();
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
                participant: None,
                github: None,
                transport: "scientist-slack".parse().unwrap(),
                kind: ChatTransportKind::Slack,
                conversation: Conversation {
                    kind: ConversationKind::Thread,
                    container: Some("t0123abc".to_owned()),
                    id: "c0123abc".to_owned(),
                    thread: Some("1712345678.000100".to_owned()),
                },
                delivery: None,
                trigger: Trigger::Message,
            },
        )
        .bound_to(request.id.clone());
        (request, claim)
    };
    let grant = AttestorGrant {
        namespaces: Some(vec!["drn:slack:*:t0123abc:*".parse().unwrap()]),
    };
    let (request, claim) = invoke(
        0,
        dekopon_core::AttestedSubject::one(bob.clone()).expect("one-form subject"),
    );
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
    let (request, claim) = invoke(
        1,
        dekopon_core::AttestedSubject::one(alice).expect("one-form subject"),
    );
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
    let (request, claim) = invoke(
        2,
        dekopon_core::AttestedSubject::one(bob).expect("one-form subject"),
    );
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
    let (request, claim) = invoke(
        3,
        dekopon_core::AttestedSubject::one(
            "drn:slack:user:t0123abc:u9xyz".parse().expect("subject"),
        )
        .unwrap(),
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn an_allowed_job_write_joins_its_job_authorization_by_invocation_id() {
    use dekopon_test_support::{CaptureLayer, Record};
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::prelude::*;

    let capture = CaptureLayer::workspace();
    let subscriber = tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()));
    async {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap().join("storage");
        let (broker, _storage) = build_broker(&root, false).await;
        let gateway = AuthenticatedContext::new(
            "gateway".parse().unwrap(),
            Actor::Service {
                principal: "gateway".parse().unwrap(),
            },
        )
        .unwrap();
        let request = InvocationRequest {
            id: "job-write-join".parse().unwrap(),
            capability: "storage-probe.run".parse().unwrap(),
            trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
                .parse()
                .unwrap(),
            input: json!({"mode":"quota-denial"}),
            secret_use: None,
        };
        let claim = Attestation::for_chat(
            dekopon_core::AttestedSubject::one(
                "drn:slack:user:t0123abc:u9xyz".parse().expect("subject"),
            )
            .unwrap(),
            "reviewer".parse().unwrap(),
            ChatScopeClaim {
                participant: None,
                github: None,
                transport: "scientist-slack".parse().unwrap(),
                kind: ChatTransportKind::Slack,
                conversation: Conversation {
                    kind: ConversationKind::Thread,
                    container: Some("t0123abc".to_owned()),
                    id: "c0123abc".to_owned(),
                    thread: Some("1712345678.000100".to_owned()),
                },
                delivery: None,
                trigger: Trigger::Job,
            },
        )
        .bound_to(request.id.clone());
        let grant = AttestorGrant {
            namespaces: Some(vec!["drn:slack:*:t0123abc:*".parse().unwrap()]),
        };
        let result = broker
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
            result.result.outcome,
            InvocationOutcome::Failed,
            "{result:?}"
        );
    }
    .with_subscriber(subscriber)
    .await;
    let records = capture.records();
    let authorize: Vec<_> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                Record::Span {
                    name: "broker.authorize",
                    ..
                }
            ) && r.field("invocation").is_some()
        })
        .collect();
    assert_eq!(authorize.len(), 1, "{records:?}");
    for (field, value) in [("trigger", "job"), ("outcome", "allowed")] {
        let updates: Vec<_> = records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    Record::Span {
                        name: "broker.authorize",
                        ..
                    }
                )
            })
            .filter_map(|r| r.field(field))
            .collect();
        assert_eq!(updates, [value]);
    }
    assert_eq!(authorize[0].field("invocation"), Some("job-write-join"));
    assert_eq!(authorize[0].field("capability"), Some("storage-probe.run"));
    for name in ["broker.decision", "broker.execution"] {
        let rows: Vec<_> = records
            .iter()
            .filter(|r| r.field("audit.event") == Some(name))
            .collect();
        assert_eq!(rows.len(), 1, "{records:?}");
        assert_eq!(rows[0].field("invocation.id"), Some("job-write-join"));
        assert_eq!(rows[0].field("capability.id"), Some("storage-probe.run"));
        if name == "broker.decision" {
            assert_eq!(rows[0].field("decision.allowed"), Some("true"));
            assert_eq!(rows[0].parent(), Some("broker.execute"));
        } else {
            assert_eq!(rows[0].field("effect"), Some("LocalWrite"));
        }
    }
}

#[tokio::test]
async fn a_discord_thread_claim_derives_its_parent_channels_space_key() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let (broker, _) = build_scoped_broker(&root, false, StorageScope::Space).await;
    let gateway = AuthenticatedContext::new(
        "gateway".parse().unwrap(),
        Actor::Service {
            principal: "gateway".parse().unwrap(),
        },
    )
    .unwrap();
    let grant = AttestorGrant {
        namespaces: Some(vec!["drn:discord:*:*:*".parse().unwrap()]),
    };
    let claim = |kind, thread| {
        Attestation::for_chat(
            dekopon_core::AttestedSubject::one("drn:discord:user::123".parse().expect("subject"))
                .unwrap(),
            "reviewer".parse().unwrap(),
            ChatScopeClaim {
                participant: None,
                github: None,
                transport: "scientist-discord".parse().unwrap(),
                kind: ChatTransportKind::Discord,
                conversation: Conversation {
                    kind,
                    container: Some("456".to_owned()),
                    id: "789".to_owned(),
                    thread,
                },
                trigger: Trigger::Message,
                delivery: None,
            },
        )
    };
    let mut keys = Vec::new();
    for claim in [
        claim(ConversationKind::Channel, None),
        claim(ConversationKind::Thread, Some("101112".to_owned())),
    ] {
        let (context, refusal) = broker.resolve_context(&gateway, Some(&grant), &claim);
        assert!(refusal.is_none());
        let binding = context.space_binding().expect("mapped parent channel");
        assert_eq!(binding.principal.as_str(), "space");
        keys.push(binding.key.encode());
    }
    assert_eq!(keys[0], keys[1]);
    assert_eq!(
        keys[0],
        dekopon_core::SpaceKey {
            agent: "reviewer".parse().unwrap(),
            transport: "scientist-discord".to_owned().into(),
            workspace: Some("456".to_owned().into()),
            channel: "789".to_owned().into(),
        }
        .encode()
    );
}
