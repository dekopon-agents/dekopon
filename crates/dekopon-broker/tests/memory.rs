#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use dekopon_broker::{
    Attestation, AttestorGrant, AuditEvent, AuthenticatedContext, Broker, BrokerBuildError,
    BrokerError, BrokerLimits, CapabilityRoute, ChatMemoryConfig, ChatTransportKind,
    ConstraintCatalog, ConstraintSet, Conversation, ConversationKind, CredentialStore,
    DeliveredAnswer, DeliveredTurnRequest, DeliveryIdentity, IdentityDirectory, InMemoryAuditLog,
    PolicyEngine, PolicyWorld, RouteConflict,
};

const MEMORY_RECORD: &str = "memory.chat.record";
const MEMORY_RECENT: &str = "memory.chat.recent";
use dekopon_broker_host::{
    BoundCredential, BrokerHostError, BrokerHostLimits, BrokerHostOptions, BrokerProviderRegistry,
};
use dekopon_broker_protocol::{ChatScopeClaim, InvocationRequest};
use dekopon_capability::{
    EffectKind, HttpConstraints, StorageAccess, StorageConstraints, StorageInterface, StorageScope,
};
use dekopon_core::{
    Actor, AgentId, ExternalSubject, InvocationId, PrincipalId, Redacted, RiskLevel, TransportId,
};
use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits};
use dekopon_test_support::{CaptureLayer, Record, provider_fixture};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt as _;

const TRACE_PARENT: &str = "00-0000000000000000000000000000f1c7-00000000000000f1-00";

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

async fn build_broker(root: &Path, audit: Arc<InMemoryAuditLog>) -> Broker<InMemoryAuditLog> {
    build_broker_with(
        root,
        audit,
        memory_config(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await
}

async fn build_broker_with(
    root: &Path,
    audit: Arc<InMemoryAuditLog>,
    memory: ChatMemoryConfig,
    storage_limits: StorageLimits,
    host_limits: BrokerHostLimits,
    reverse_provider_order: bool,
) -> Broker<InMemoryAuditLog> {
    build_broker_with_principal(
        root,
        audit,
        memory,
        storage_limits,
        host_limits,
        reverse_provider_order,
        "maintainer",
        None,
        false,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the integration fixture keeps each independently rotated authority input explicit"
)]
async fn build_broker_with_principal(
    root: &Path,
    audit: Arc<InMemoryAuditLog>,
    memory: ChatMemoryConfig,
    storage_limits: StorageLimits,
    host_limits: BrokerHostLimits,
    reverse_provider_order: bool,
    mapped_principal: &str,
    authority_credential: Option<(&str, &str)>,
    permit_generic_storage: bool,
) -> Broker<InMemoryAuditLog> {
    build_broker_with_options(
        root,
        audit,
        memory,
        storage_limits,
        host_limits,
        reverse_provider_order,
        mapped_principal,
        authority_credential,
        permit_generic_storage,
        &BrokerHostOptions::default(),
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the integration fixture keeps authority inputs separate from operational cache options"
)]
async fn build_broker_with_options(
    root: &Path,
    audit: Arc<InMemoryAuditLog>,
    memory: ChatMemoryConfig,
    storage_limits: StorageLimits,
    host_limits: BrokerHostLimits,
    reverse_provider_order: bool,
    mapped_principal: &str,
    authority_credential: Option<(&str, &str)>,
    permit_generic_storage: bool,
    options: &BrokerHostOptions,
) -> Broker<InMemoryAuditLog> {
    let storage = StorageHost::open(root, storage_limits).expect("storage host");
    let mut providers = vec![
        provider_fixture("memory-chat-provider.wasm"),
        provider_fixture("cli-probe-provider.wasm"),
        provider_fixture("storage-probe-provider.wasm"),
    ];
    if authority_credential.is_some() {
        providers.push(provider_fixture("http-probe-provider.wasm"));
    }
    if reverse_provider_order {
        providers.reverse();
    }
    let registry =
        BrokerProviderRegistry::load_with_options(providers, host_limits, Some(storage), options)
            .await
            .expect("memory provider loads");
    let world = PolicyWorld::new(
        [
            "gateway".parse::<PrincipalId>().expect("gateway"),
            mapped_principal.parse().expect("mapped principal"),
        ],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("world");
    let mut policy_source = r#"
        @id("prompt")
        permit(principal == Dekopon::Principal::"$PRINCIPAL",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context.via == "gateway"
            && context has transportKind && context.transportKind == "slack"
            && context has transport && context.transport == "scientist-slack"
            && context has conversation && context.conversation.id == "c0123abc"
            && ["channel", "thread"].contains(context.conversation.kind) };

        @id("memory")
        permit(principal == Dekopon::Principal::"$PRINCIPAL",
               action in [Dekopon::Action::"memory.chat.record",
                          Dekopon::Action::"memory.chat.recent",
                          Dekopon::Action::"memory.chat.search"],
               resource == Dekopon::Provider::"memory-chat")
        when { context.via == "gateway"
            && context.agent == "reviewer"
            && context has transportKind && context.transportKind == "slack"
            && context has transport && context.transport == "scientist-slack"
            && context has conversation && context.conversation.id == "c0123abc"
            && ["channel", "thread"].contains(context.conversation.kind) };
        "#
    .replace("$PRINCIPAL", mapped_principal);
    if permit_generic_storage {
        policy_source.push_str(
            r#"
        @id("generic-storage-prompt")
        permit(principal == Dekopon::Principal::"$PRINCIPAL",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer");

        @id("generic-storage")
        permit(principal == Dekopon::Principal::"$PRINCIPAL",
               action == Dekopon::Action::"storage-probe.run",
               resource == Dekopon::Provider::"storage-probe");
        "#,
        );
        policy_source = policy_source.replace("$PRINCIPAL", mapped_principal);
    }
    if authority_credential.is_some() {
        policy_source.push_str(
            r#"
        @id("effective-http")
        permit(principal == Dekopon::Principal::"$PRINCIPAL",
               action == Dekopon::Action::"http-probe.fetch",
               resource == Dekopon::Provider::"http-probe")
        when { context.via == "gateway"
            && context.agent == "reviewer" };
        "#,
        );
        policy_source = policy_source.replace("$PRINCIPAL", mapped_principal);
    }
    let policy = PolicyEngine::new(&policy_source, &world).expect("policy");
    let credentials = authority_credential.map_or_else(CredentialStore::empty, |(name, value)| {
        CredentialStore::new([(
            name.to_owned(),
            BoundCredential::bearer(
                "Bearer",
                Redacted::new(value.to_owned()),
                vec!["127.0.0.1:1".to_owned()],
            )
            .expect("credential"),
        )])
        .expect("credential store")
    });
    Broker::new(
        registry,
        "broker".parse().expect("broker"),
        "memory-policy".to_owned(),
        policy,
        constraints_with_http_credential(authority_credential.map(|(name, _)| name)),
        credentials,
        IdentityDirectory::new([(
            "slack.t0123abc.u9xyz".parse().expect("subject"),
            mapped_principal.parse().expect("principal"),
        )])
        .expect("identities"),
        audit,
        BrokerLimits::default(),
    )
    .expect("broker")
    .with_chat_memory(memory)
    .expect("memory composition")
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
async fn authorization_audit_failure_precedes_every_storage_tree_mutation() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let audit = Arc::new(InMemoryAuditLog::new(1).expect("one-record audit"));
    let broker = build_broker_with(
        &root,
        Arc::clone(&audit),
        memory_config(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;

    let denied = query_memory_result(&broker, "fill-audit", MEMORY_RECENT, json!({"last": 0}))
        .await
        .0;
    assert_eq!(
        denied.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(audit.records().len(), 1);
    let before = snapshot_tree_bytes(&root);

    let id = "audit-full-record"
        .parse::<InvocationId>()
        .expect("invocation");
    let attestor = attestor_grant();
    let session = claim();
    let error = broker
        .record_delivered_turn(
            &gateway(),
            Some(&attestor),
            &session.bound_to(id.clone()),
            DeliveredTurnRequest::new(
                id,
                TRACE_PARENT.parse().expect("valid traceparent fixture"),
                DeliveryIdentity::Slack {
                    channel: "c0123abc".to_owned(),
                    timestamp: "1712345678.000101".to_owned(),
                },
                "must remain unmaterialized".to_owned(),
                DeliveredAnswer::accepted_by_transport("audit failed".to_owned()),
            ),
        )
        .await
        .expect_err("full authorization audit refuses before storage materialization");
    assert!(matches!(error, BrokerError::DecisionAudit { .. }));
    assert_eq!(
        snapshot_tree_bytes(&root),
        before,
        "audit failure created a namespace, lifecycle marker, generation, or current pointer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn generic_storage_surfaces_require_an_effective_chat_scope() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let broker = build_broker_with_principal(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        memory_config(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        None,
        true,
    )
    .await;
    let storage_id = "storage-probe.run";
    let storage_word = "storageprobe";
    let direct = AuthenticatedContext::new(
        "maintainer".parse().expect("principal"),
        Actor::Service {
            principal: "maintainer".parse().expect("principal"),
        },
    )
    .expect("direct context");
    assert!(
        broker
            .capabilities(&direct)
            .iter()
            .all(|entry| entry.capability.id.as_str() != storage_id)
    );
    assert!(
        !broker
            .command_words(&direct)
            .iter()
            .any(|word| word == storage_word)
    );
    assert_eq!(
        broker.capability_view(&direct),
        (
            broker.capabilities(&direct),
            broker.command_words(&direct),
            broker.command_word_help(&direct)
        )
    );

    let session = claim();
    let legacy_grant = AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    };
    let (legacy_capabilities, legacy_words, _legacy_help, _memory) = broker
        .capability_surface(
            &gateway(),
            Some(&legacy_grant),
            Some(&Attestation::for_subject(
                session.subject.clone(),
                session.agent.clone(),
            )),
        )
        .expect("legacy subject-only chat remains authorized");
    assert!(
        legacy_capabilities
            .iter()
            .all(|entry| entry.capability.id.as_str() != storage_id)
    );
    assert!(!legacy_words.iter().any(|word| word == storage_word));

    let (scoped_capabilities, scoped_words, scoped_help, _) = broker
        .capability_surface(&gateway(), Some(&attestor_grant()), Some(&session))
        .expect("scoped chat is authorized");
    assert!(
        scoped_capabilities
            .iter()
            .any(|entry| entry.capability.id.as_str() == storage_id)
    );
    assert!(scoped_words.iter().any(|word| word == storage_word));
    assert!(
        scoped_help.keys().all(|word| scoped_words.contains(word)),
        "help never advertises a word that is not also listed"
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn a_watch_probe_is_neither_shown_nor_granted_a_write() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let broker = build_broker(
        &directory.join("provider-storage"),
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
    )
    .await;
    let mut probe = claim();
    if let Some(scope) = probe.scope.as_mut() {
        scope.trigger = dekopon_broker::Trigger::Probe;
    }

    let (capabilities, words, _help, _) = broker
        .capability_surface(&gateway(), Some(&attestor_grant()), Some(&probe))
        .expect("a probe is an authorized chat session");
    assert!(
        capabilities
            .iter()
            .all(|entry| entry.capability.id.as_str() != "storage-probe.run")
    );
    assert!(!words.iter().any(|word| word == "storageprobe"));

    let id = "probe-write".parse::<InvocationId>().expect("invocation");
    let result = broker
        .invoke(
            &gateway(),
            Some(&attestor_grant()),
            Some(&probe.bound_to(id.clone())),
            InvocationRequest {
                id,
                capability: "storage-probe.run".parse().expect("capability"),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input: json!({"mode": "quota-denial"}),
                secret_use: None,
            },
            Default::default(),
        )
        .await
        .expect("the refusal is accounted");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.result.error.as_deref(), Some("probe-write"));
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
            Default::default(),
        )
        .await
        .expect("chat invocation is audited");
    assert_eq!(
        chat_result.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{chat_result:?}"
    );

    for capability in ["ordinary.escape", "memory.chat.export"] {
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
                Default::default(),
            )
            .await
            .expect("attested invocation is audited");
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
