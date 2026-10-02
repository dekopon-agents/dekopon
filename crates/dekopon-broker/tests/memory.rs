#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use std::sync::Arc;

use dekopon_broker::{
    Attestation, AttestorGrant, Broker, BrokerBuildError, BrokerLimits, CapabilityRoute,
    ChatMemoryConfig, ChatTransportKind, ConstraintCatalog, ConstraintSet, Conversation,
    ConversationKind, CredentialStore, IdentityDirectory, InMemoryAuditLog, PolicyEngine,
    PolicyWorld, RouteConflict,
};

use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerProviderRegistry,
};
use dekopon_broker_protocol::{ChatScopeClaim, InvocationRequest};
use dekopon_capability::{
    EffectKind, HttpConstraints, StorageAccess, StorageConstraints, StorageInterface, StorageScope,
};
use dekopon_core::{
    Actor, AgentId, ExternalSubject, InvocationId, PrincipalId, RiskLevel, TransportId,
};
use dekopon_storage_host::{ContinuityPolicy, StorageHost, StorageLimits};
use dekopon_test_support::provider_fixture;
use serde_json::json;

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

fn stdout_assets() -> (
    dekopon_broker_host::asset::AssetInputs,
    std::thread::JoinHandle<Vec<u8>>,
) {
    let (host, mut reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let captured = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut bytes).unwrap();
        bytes
    });
    (
        dekopon_broker_host::asset::AssetInputs {
            streams: Some(dekopon_broker_host::Streams {
                stdin: None,
                stdout: host.into(),
            }),
            ..Default::default()
        },
        captured,
    )
}

fn memory_config() -> ChatMemoryConfig {
    ChatMemoryConfig {
        continuity_policy: ContinuityPolicy::AuthorityBound,
        enabled_agents: vec!["reviewer".parse().expect("agent")],
        max_lookback_turns: 200,
        max_recent_turns: 20,
        max_search_results: 20,
        max_query_bytes: 256,
        max_result_bytes: 65_536,
        max_turn_bytes: 32_768,
        compaction_target_bytes: 8_388_608,
        compaction_threshold_bytes: 12_582_912,
    }
}

fn constraints_with_http_credential(credential: Option<&str>) -> ConstraintCatalog {
    let mut entries = [
        (
            "memory.chat.record",
            CapabilityRoute::ChatMemoryRecord,
            EffectKind::LocalWrite,
            RiskLevel::Medium,
            StorageAccess::ReadWrite,
        ),
        (
            "memory.chat.recent",
            CapabilityRoute::ChatMemoryRecent,
            EffectKind::ReadOnly,
            RiskLevel::High,
            StorageAccess::ReadOnly,
        ),
        (
            "memory.chat.search",
            CapabilityRoute::ChatMemorySearch,
            EffectKind::ReadOnly,
            RiskLevel::High,
            StorageAccess::ReadOnly,
        ),
    ]
    .into_iter()
    .map(|(id, route, effect, risk, access)| {
        (
            id.parse().expect("capability"),
            memory_constraint(route, effect, risk, access),
        )
    })
    .collect::<Vec<_>>();
    entries.push((
        "storage-probe.run".parse().expect("capability"),
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "storage-probe".parse().expect("provider"),
            effect: EffectKind::LocalWrite,
            risk: RiskLevel::Medium,
            credential: None,
            constraints: dekopon_capability::ExecutionConstraints {
                asset: None,
                timeout_ms: 10_000,
                http: None,
                storage: Some(StorageConstraints {
                    interface: StorageInterface::DurableFiles,
                    access: StorageAccess::ReadWrite,
                    scope: StorageScope::PrivateConversation,
                    retention: Default::default(),
                }),
                secret_use: None,
            },
        },
    ));
    if let Some(credential) = credential {
        entries.push((
            "http-probe.fetch".parse().expect("capability"),
            ConstraintSet {
                route: CapabilityRoute::Generic,
                provider: "http-probe".parse().expect("provider"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                credential: Some(credential.to_owned()),
                constraints: dekopon_capability::ExecutionConstraints {
                    asset: None,
                    timeout_ms: 10_000,
                    http: Some(HttpConstraints {
                        allowed_hosts: vec!["127.0.0.1:1".to_owned()],
                        allowed_methods: vec!["GET".to_owned()],
                        max_requests: 1,
                        max_request_bytes: 4_096,
                        max_response_bytes: 4_096,
                        allow_plaintext_loopback: true,
                        propagate_trace: false,
                    }),
                    storage: None,
                    secret_use: None,
                },
            },
        ));
    }
    ConstraintCatalog::new(entries).expect("constraints")
}

fn gateway() -> dekopon_broker::AuthenticatedContext {
    dekopon_broker::AuthenticatedContext::new(
        "gateway".parse().expect("principal"),
        Actor::Service {
            principal: "gateway".parse().expect("principal"),
        },
    )
    .expect("context")
}

fn claim() -> Attestation {
    claim_for("c0123abc:1712345678.000100")
}

fn slack_conversation(key: &str) -> Conversation {
    let (id, thread) = key
        .split_once(':')
        .map_or((key, None), |(id, thread)| (id, Some(thread.to_owned())));
    Conversation {
        kind: ConversationKind::Thread,
        container: Some("t0123abc".to_owned()),
        id: id.to_owned(),
        thread,
    }
}

fn claim_for(conversation: &str) -> Attestation {
    Attestation::for_chat(
        "slack.t0123abc.u9xyz"
            .parse::<ExternalSubject>()
            .expect("subject"),
        "reviewer".parse::<AgentId>().expect("agent"),
        ChatScopeClaim {
            transport: "scientist-slack".parse::<TransportId>().expect("transport"),
            kind: ChatTransportKind::Slack,
            conversation: slack_conversation(conversation),
            trigger: dekopon_broker::Trigger::Message,
        },
    )
}

fn attestor_grant() -> AttestorGrant {
    AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fetched_memory_chat_0_3_0_fixture_is_refused_before_any_storage_effect() {
    let error = BrokerProviderRegistry::load(
        [provider_fixture("memory-chat-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect_err("a returned-string invoke cannot load as a process");
    assert!(
        matches!(error, BrokerHostError::Instantiate { .. }),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one long test scenario")]
async fn reserved_looking_names_without_a_declared_route_are_ordinary_capabilities() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("memory-reservation-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("malicious fixture loads before broker reservation");
    let world = PolicyWorld::new(
        [
            "gateway".parse::<PrincipalId>().expect("gateway"),
            "maintainer".parse().expect("maintainer"),
        ],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("policy world");
    let policy = PolicyEngine::new(
        r#"
        @id("maintainer-prompt")
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context.via == "gateway" };
        @id("maintainer-reserved-looking-names")
        permit(principal == Dekopon::Principal::"maintainer",
               action in [Dekopon::Action::"ordinary.escape",
                          Dekopon::Action::"memory.chat.export"],
               resource == Dekopon::Provider::"memory-chat")
        when { context.via == "gateway"
            && context.agent == "reviewer" };
        "#,
        &world,
    )
    .expect("policy");
    let constraints =
        ConstraintCatalog::new(["ordinary.escape", "memory.chat.export"].map(|identifier| {
            (
                identifier.parse().expect("capability"),
                reserved_read_constraint(),
            )
        }))
        .expect("constraints");
    let broker = Broker::new(
        registry,
        "broker".parse().expect("broker"),
        "reservation-policy".to_owned(),
        policy,
        constraints,
        CredentialStore::empty(),
        IdentityDirectory::new([(
            "slack.t0123abc.u9xyz".parse().expect("subject"),
            "maintainer".parse().expect("principal"),
        )])
        .expect("identities"),
        Arc::new(InMemoryAuditLog::new(32).expect("audit")),
        BrokerLimits::default(),
    )
    .expect("broker");
    let gateway = gateway();
    let grant = AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    };
    let claim = claim();
    let (listed, _, _, _) = broker
        .capability_surface(
            &gateway,
            Some(&grant),
            Some(&Attestation::for_subject(
                claim.subject.clone(),
                claim.agent.clone(),
            )),
        )
        .expect("legacy attestation is honored");
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.capability.id.as_str())
            .collect::<Vec<_>>(),
        ["memory.chat.export", "ordinary.escape"],
        "an undeclared route hides nothing, however the capability is spelled"
    );
    let (listed, words, help, memory) = broker
        .capability_surface(&gateway, Some(&grant), Some(&claim))
        .expect("ordinary chat remains available");
    assert_eq!(listed.len(), 2);
    assert_eq!(words, ["recall"]);
    assert!(help.contains_key("recall"));
    assert!(memory.is_none(), "no route means no memory surface");
    broker
        .run_command(&gateway, Some(&grant), Some(&claim), "recall", &[], false)
        .await
        .expect("chat resolution reserves nothing either");
    let chat_id = "unrouted-chat".parse::<InvocationId>().expect("invocation");
    let (streams, stdout) = stdout_assets();
    let chat_result = broker
        .invoke(
            &gateway,
            Some(&grant),
            Some(&claim.bound_to(chat_id.clone())),
            InvocationRequest {
                id: chat_id,
                capability: "ordinary.escape".parse().expect("capability"),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input: json!({}),
                secret_use: None,
            },
            streams,
        )
        .await
        .expect("chat invocation is audited");
    assert_eq!(stdout.join().unwrap(), b"{\"escaped\":true}\n");
    assert_eq!(
        chat_result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{chat_result:?}"
    );

    for capability in ["ordinary.escape", "memory.chat.export"] {
        let (streams, stdout) = stdout_assets();
        let id = format!("unrouted-attested-{capability}")
            .parse::<InvocationId>()
            .expect("invocation");
        let result = broker
            .invoke(
                &gateway,
                Some(&grant),
                Some(
                    &Attestation::for_subject(claim.subject.clone(), claim.agent.clone())
                        .bound_to(id.clone()),
                ),
                InvocationRequest {
                    id,
                    capability: capability.parse().expect("capability"),
                    trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                    input: json!({}),
                    secret_use: None,
                },
                streams,
            )
            .await
            .expect("attested invocation is audited");
        assert_eq!(stdout.join().unwrap(), b"{\"escaped\":true}\n");
        assert_eq!(
            result.result.outcome,
            dekopon_capability::InvocationOutcome::Succeeded,
            "{result:?}"
        );
    }
    drop(broker);

    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let storage = StorageHost::open(&root, StorageLimits::default()).expect("storage host");
    let registry = BrokerProviderRegistry::load_with_storage(
        [provider_fixture("memory-reservation-probe-provider.wasm")],
        BrokerHostLimits::default(),
        Some(storage),
    )
    .await
    .expect("malicious fixture loads with storage disabled inside the guest");
    let world = PolicyWorld::new(
        ["caller".parse::<PrincipalId>().expect("caller")],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("world");
    let policy = PolicyEngine::new("", &world).expect("empty policy");
    let catalog = constraints_with_http_credential(None);
    let constraints = ConstraintCatalog::new(
        catalog
            .iter()
            .filter(|(_, set)| set.route != CapabilityRoute::Generic)
            .map(|(id, set)| (id.clone(), set.clone()))
            .chain(
                ["ordinary.escape", "memory.chat.export"]
                    .map(|id| (id.parse().expect("capability"), reserved_read_constraint())),
            ),
    )
    .expect("malicious constraints");
    let broker = Broker::new(
        registry,
        "broker".parse().expect("broker"),
        "composition-policy".to_owned(),
        policy,
        constraints,
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(8).expect("audit")),
        BrokerLimits::default(),
    )
    .expect("broker without chat memory");
    assert!(
        broker.with_chat_memory(memory_config()).is_err(),
        "only the exact three-capability routed provider may enable memory"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one long test scenario")]
async fn a_declared_memory_route_is_hidden_and_denied_regardless_of_provider_name() {
    for (fixture, provider, capability, constraint) in [
        (
            "memory-reservation-probe-provider.wasm",
            "memory-chat",
            "ordinary.escape",
            memory_constraint(
                CapabilityRoute::ChatMemoryRecent,
                EffectKind::ReadOnly,
                RiskLevel::Low,
                StorageAccess::ReadOnly,
            ),
        ),
        (
            "storage-probe-provider.wasm",
            "storage-probe",
            "storage-probe.run",
            ConstraintSet {
                provider: "storage-probe".parse().expect("provider"),
                ..memory_constraint(
                    CapabilityRoute::ChatMemoryRecord,
                    EffectKind::LocalWrite,
                    RiskLevel::Medium,
                    StorageAccess::ReadWrite,
                )
            },
        ),
    ] {
        let route = constraint.route;
        let temporary = tempfile::tempdir().expect("tempdir");
        let directory = temporary.path().canonicalize().expect("canonical tempdir");
        let root = directory.join("provider-storage");
        let storage = StorageHost::open(&root, StorageLimits::default()).expect("storage host");
        let registry = BrokerProviderRegistry::load_with_storage(
            [provider_fixture(fixture)],
            BrokerHostLimits::default(),
            Some(storage),
        )
        .await
        .expect("routed fixture loads");
        let world = PolicyWorld::new(
            [
                "gateway".parse::<PrincipalId>().expect("gateway"),
                "maintainer".parse().expect("maintainer"),
            ],
            registry
                .capabilities()
                .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
        )
        .expect("policy world");
        let policy = PolicyEngine::new(
            r#"
        @id("maintainer-prompt")
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context.via == "gateway" };
        @id("maintainer-routed-capability")
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"$CAPABILITY",
               resource == Dekopon::Provider::"$PROVIDER")
        when { context.via == "gateway"
            && context.agent == "reviewer" };
        "#
            .replace("$CAPABILITY", capability)
            .replace("$PROVIDER", provider)
            .as_str(),
            &world,
        )
        .expect("policy");
        let constraints =
            ConstraintCatalog::new([(capability.parse().expect("capability"), constraint)])
                .expect("constraints");
        let broker = Broker::new(
            registry,
            "broker".parse().expect("broker"),
            "renamed-route-policy".to_owned(),
            policy,
            constraints,
            CredentialStore::empty(),
            IdentityDirectory::new([(
                "slack.t0123abc.u9xyz".parse().expect("subject"),
                "maintainer".parse().expect("principal"),
            )])
            .expect("identities"),
            Arc::new(InMemoryAuditLog::new(32).expect("audit")),
            BrokerLimits::default(),
        )
        .expect("broker");
        let gateway = gateway();
        let grant = attestor_grant();
        let claim = claim();
        if route == CapabilityRoute::ChatMemoryRecent {
            let attestation = Attestation::for_subject(claim.subject, claim.agent);
            let (_, words, _, _) = broker
                .capability_surface(&gateway, Some(&grant), Some(&attestation))
                .expect("the attestation is honored");
            assert!(words.is_empty(), "a reserved word is not in the vocabulary");
            let refused = broker
                .run_command(
                    &gateway,
                    Some(&grant),
                    Some(&attestation),
                    "recall",
                    &["--help".to_owned()],
                    false,
                )
                .await
                .expect_err("a reserved word never reaches its guest");
            assert!(
                matches!(&refused, BrokerHostError::UnknownCommandWord { word } if word == "recall"),
                "{refused:?}"
            );
            continue;
        }
        let (listed, words, help, _memory) = broker
            .capability_surface(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(
                    claim.subject.clone(),
                    claim.agent.clone(),
                )),
            )
            .expect("legacy attestation is honored");
        assert!(listed.is_empty() && words.is_empty() && help.is_empty());
        let (listed, words, help, memory) = broker
            .capability_surface(&gateway, Some(&grant), Some(&claim))
            .expect("ordinary chat remains available");
        assert!(listed.is_empty() && words.is_empty() && help.is_empty() && memory.is_none());
        assert!(
            broker
                .run_command(
                    &gateway,
                    Some(&grant),
                    Some(&claim),
                    "storageprobe",
                    &[],
                    false
                )
                .await
                .is_err()
        );

        let chat_id = "renamed-chat".parse::<InvocationId>().expect("invocation");
        let chat_result = broker
            .invoke(
                &gateway,
                Some(&grant),
                Some(&claim.bound_to(chat_id.clone())),
                InvocationRequest {
                    id: chat_id,
                    capability: "storage-probe.run".parse().expect("capability"),
                    trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                    input: json!({}),
                    secret_use: None,
                },
                Default::default(),
            )
            .await
            .expect("chat reserved denial is audited");
        assert_eq!(
            chat_result.result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
        assert_eq!(
            chat_result.result.error.as_deref(),
            Some("record-operation-required"),
            "the record route is unreachable from the generic chat invoke path"
        );

        let id = "renamed-attested"
            .parse::<InvocationId>()
            .expect("invocation");
        let result = broker
            .invoke(
                &gateway,
                Some(&grant),
                Some(&Attestation::for_subject(claim.subject, claim.agent).bound_to(id.clone())),
                InvocationRequest {
                    id,
                    capability: "storage-probe.run".parse().expect("capability"),
                    trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                    input: json!({}),
                    secret_use: None,
                },
                Default::default(),
            )
            .await
            .expect("attested reserved denial is audited");
        assert_eq!(
            result.result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
        assert_eq!(result.result.error.as_deref(), Some("chat-scope-required"));
    }
}

fn memory_constraint(
    route: CapabilityRoute,
    effect: EffectKind,
    risk: RiskLevel,
    access: StorageAccess,
) -> ConstraintSet {
    ConstraintSet {
        route,
        provider: "memory-chat".parse().expect("provider"),
        effect,
        risk,
        credential: None,
        constraints: dekopon_capability::ExecutionConstraints {
            asset: None,
            timeout_ms: 10_000,
            http: None,
            storage: Some(StorageConstraints {
                interface: StorageInterface::Jsonl,
                access,
                scope: StorageScope::PrivateConversation,
                retention: Default::default(),
            }),
            secret_use: None,
        },
    }
}

fn reserved_read_constraint() -> ConstraintSet {
    ConstraintSet {
        route: CapabilityRoute::Generic,
        provider: "memory-chat".parse().expect("provider"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        credential: None,
        constraints: dekopon_capability::ExecutionConstraints::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_declared_route_conflict_is_reported_at_startup() {
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("cli-probe fixture loads");
    let world = PolicyWorld::new(
        ["caller".parse::<PrincipalId>().expect("caller")],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("policy world");
    let routed = |route, provider: &str, storage| ConstraintSet {
        route,
        provider: provider.parse().expect("provider"),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        credential: None,
        constraints: dekopon_capability::ExecutionConstraints {
            asset: None,
            timeout_ms: 10_000,
            http: None,
            storage,
            secret_use: None,
        },
    };
    let read_only = Some(StorageConstraints {
        interface: StorageInterface::Jsonl,
        access: StorageAccess::ReadOnly,
        scope: StorageScope::PrivateConversation,
        retention: Default::default(),
    });
    let constraints = ConstraintCatalog::new([
        (
            "cli-probe.count".parse().expect("capability"),
            routed(
                CapabilityRoute::ChatMemoryRecent,
                "cli-probe",
                read_only.clone(),
            ),
        ),
        (
            "cli-probe.second".parse().expect("capability"),
            routed(CapabilityRoute::ChatMemoryRecent, "cli-probe", read_only),
        ),
        (
            "cli-probe.third".parse().expect("capability"),
            routed(CapabilityRoute::ChatMemorySearch, "elsewhere", None),
        ),
    ])
    .expect("catalog");
    let error = Broker::new(
        registry,
        "broker".parse().expect("broker"),
        "route-conflict-policy".to_owned(),
        PolicyEngine::new("", &world).expect("empty policy"),
        constraints,
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(8).expect("audit")),
        BrokerLimits::default(),
    )
    .expect_err("conflicting routes refuse startup");
    let rendered = error.to_string();
    let BrokerBuildError::ConflictingRoutes { conflicts } = error else {
        panic!("route conflicts must be their own build error: {rendered}");
    };
    assert_eq!(
        conflicts,
        vec![
            RouteConflict::DuplicateRole {
                route: CapabilityRoute::ChatMemoryRecent,
                capabilities: vec![
                    "cli-probe.count".parse().expect("capability"),
                    "cli-probe.second".parse().expect("capability"),
                ],
            },
            RouteConflict::SplitProvider {
                providers: vec![
                    "cli-probe".parse().expect("provider"),
                    "elsewhere".parse().expect("provider"),
                ],
            },
            RouteConflict::MissingChatStorage {
                capability: "cli-probe.third".parse().expect("capability"),
                route: CapabilityRoute::ChatMemorySearch,
                access: StorageAccess::ReadOnly,
            },
        ],
        "one run must report every route mistake: {rendered}"
    );
    for fragment in ["cli-probe.second", "elsewhere", "cli-probe.third"] {
        assert!(
            rendered.contains(fragment),
            "the message names {fragment}: {rendered}"
        );
    }
}
