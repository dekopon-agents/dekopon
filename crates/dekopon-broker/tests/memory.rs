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
    DeliveredTurnRequest, DeliveryIdentity, IdentityDirectory, InMemoryAuditLog, PolicyEngine,
    PolicyWorld, RouteConflict,
};

const MEMORY_RECORD: &str = "memory.chat.record";
const MEMORY_RECENT: &str = "memory.chat.recent";
use dekopon_broker_host::{
    BoundCredential, BrokerHostError, BrokerHostLimits, BrokerHostOptions, BrokerProviderRegistry,
};
use dekopon_broker_protocol::{ChatScopeClaim, InvocationRequest};
use dekopon_capability::{
    EffectKind, HttpConstraints, StorageAccess, StorageConstraints, StorageInterface,
    StorageNamespace,
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
        max_dedup_records: 16_000,
        max_dedup_bytes: 4_194_304,
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
        let capability = id.parse().expect("capability");
        (
            capability,
            ConstraintSet {
                route,
                provider: "memory-chat".parse().expect("provider"),
                effect,
                risk,
                credential: None,
                constraints: dekopon_capability::ExecutionConstraints {
                    asset: None,
                    timeout_ms: 10_000,
                    max_output_bytes: 131_072,
                    http: None,
                    storage: Some(StorageConstraints {
                        interface: StorageInterface::Jsonl,
                        access,
                        namespace: StorageNamespace::Chat,
                    }),
                    secret_use: None,
                },
            },
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
                max_output_bytes: 131_072,
                http: None,
                storage: Some(StorageConstraints {
                    interface: StorageInterface::DurableFiles,
                    access: StorageAccess::ReadWrite,
                    namespace: StorageNamespace::Chat,
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
                    max_output_bytes: 131_072,
                    http: Some(HttpConstraints {
                        allowed_hosts: vec!["127.0.0.1:1".to_owned()],
                        allowed_methods: vec!["GET".to_owned()],
                        max_requests: 1,
                        max_request_bytes: 4_096,
                        max_response_bytes: 4_096,
                        allow_plaintext_loopback: true,
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

    let denied =
        query_memory_result(&broker, "fill-audit", MEMORY_RECENT, json!({"last": 0})).await;
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
            DeliveredTurnRequest {
                id,
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                delivery: DeliveryIdentity::Slack {
                    channel: "c0123abc".to_owned(),
                    timestamp: "1712345678.000101".to_owned(),
                },
                user: "must remain unmaterialized".to_owned(),
                assistant: "audit failed".to_owned(),
            },
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
        (broker.capabilities(&direct), broker.command_words(&direct))
    );

    let session = claim();
    let legacy_grant = AttestorGrant {
        namespaces: Some(vec!["slack.t0123abc".to_owned()]),
    };
    let (legacy_capabilities, legacy_words, _memory) = broker
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

    let (scoped_capabilities, scoped_words, _) = broker
        .capability_surface(&gateway(), Some(&attestor_grant()), Some(&session))
        .expect("scoped chat is authorized");
    assert!(
        scoped_capabilities
            .iter()
            .any(|entry| entry.capability.id.as_str() == storage_id)
    );
    assert!(scoped_words.iter().any(|word| word == storage_word));
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

    let (capabilities, words, _) = broker
        .capability_surface(&gateway(), Some(&grant()), Some(&probe))
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
            Some(&grant()),
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

/// The reserved chat-memory surface is determined by the declared route, not a provider's name or
/// capability naming, so mimicking the shipped provider gains or loses nothing.
#[tokio::test(flavor = "multi_thread")]
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
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context.via == "gateway" };
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
    let (listed, _, _) = broker
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
    let (listed, words, memory) = broker
        .capability_surface(&gateway, Some(&grant), Some(&claim))
        .expect("ordinary chat remains available");
    assert_eq!(listed.len(), 2);
    assert_eq!(words, ["recall"]);
    assert!(memory.is_none(), "no route means no memory surface");
    broker
        .run_command(&gateway, Some(&grant), Some(&claim), "recall", &[], None)
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
    let constraints = ConstraintCatalog::new([
        (
            "memory.chat.record".parse().expect("capability"),
            memory_constraint(
                CapabilityRoute::ChatMemoryRecord,
                EffectKind::LocalWrite,
                RiskLevel::Medium,
                StorageAccess::ReadWrite,
            ),
        ),
        (
            "memory.chat.recent".parse().expect("capability"),
            memory_constraint(
                CapabilityRoute::ChatMemoryRecent,
                EffectKind::ReadOnly,
                RiskLevel::High,
                StorageAccess::ReadOnly,
            ),
        ),
        (
            "memory.chat.search".parse().expect("capability"),
            memory_constraint(
                CapabilityRoute::ChatMemorySearch,
                EffectKind::ReadOnly,
                RiskLevel::High,
                StorageAccess::ReadOnly,
            ),
        ),
        (
            "ordinary.escape".parse().expect("capability"),
            reserved_read_constraint(),
        ),
        (
            "memory.chat.export".parse().expect("capability"),
            reserved_read_constraint(),
        ),
    ])
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
async fn a_rendered_page_never_reaches_a_reserved_memory_route() {
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
    .expect("rendering fixture loads");
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
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context.via == "gateway" };
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"ordinary.escape",
               resource == Dekopon::Provider::"memory-chat")
        when { context.via == "gateway" && context.agent == "reviewer" };
        "#,
        &world,
    )
    .expect("policy");
    let constraints = ConstraintCatalog::new([(
        "ordinary.escape".parse().expect("capability"),
        ConstraintSet {
            route: CapabilityRoute::ChatMemoryRecent,
            provider: "memory-chat".parse().expect("provider"),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Low,
            credential: None,
            constraints: dekopon_capability::ExecutionConstraints {
                asset: None,
                timeout_ms: 10_000,
                max_output_bytes: 131_072,
                http: None,
                storage: Some(StorageConstraints {
                    interface: StorageInterface::Jsonl,
                    access: StorageAccess::ReadOnly,
                    namespace: StorageNamespace::Chat,
                }),
                secret_use: None,
            },
        },
    )])
    .expect("constraints");
    let broker = Broker::new(
        registry,
        "broker".parse().expect("broker"),
        "reserved-render-policy".to_owned(),
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
    let attestation = Attestation::for_subject(claim.subject, claim.agent);
    let (_, words, _) = broker
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
            None,
        )
        .await
        .expect_err("a reserved word never reaches its guest");
    assert!(
        matches!(&refused, BrokerHostError::UnknownCommandWord { word } if word == "recall"),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_provider_carrying_a_declared_route_is_still_hidden_and_denied() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let storage = StorageHost::open(&root, StorageLimits::default()).expect("storage host");
    let registry = BrokerProviderRegistry::load_with_storage(
        [provider_fixture("storage-probe-provider.wasm")],
        BrokerHostLimits::default(),
        Some(storage),
    )
    .await
    .expect("storage probe fixture loads");
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
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context.via == "gateway" };
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"storage-probe.run",
               resource == Dekopon::Provider::"storage-probe")
        when { context.via == "gateway"
            && context.agent == "reviewer" };
        "#,
        &world,
    )
    .expect("policy");
    let constraints = ConstraintCatalog::new([(
        "storage-probe.run".parse().expect("capability"),
        ConstraintSet {
            route: CapabilityRoute::ChatMemoryRecord,
            provider: "storage-probe".parse().expect("provider"),
            effect: EffectKind::LocalWrite,
            risk: RiskLevel::Medium,
            credential: None,
            constraints: dekopon_capability::ExecutionConstraints {
                asset: None,
                timeout_ms: 10_000,
                max_output_bytes: 131_072,
                http: None,
                storage: Some(StorageConstraints {
                    interface: StorageInterface::Jsonl,
                    access: StorageAccess::ReadWrite,
                    namespace: StorageNamespace::Chat,
                }),
                secret_use: None,
            },
        },
    )])
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
    let (listed, words, _memory) = broker
        .capability_surface(
            &gateway,
            Some(&grant),
            Some(&Attestation::for_subject(
                claim.subject.clone(),
                claim.agent.clone(),
            )),
        )
        .expect("legacy attestation is honored");
    assert!(listed.is_empty() && words.is_empty());
    let (listed, words, memory) = broker
        .capability_surface(&gateway, Some(&grant), Some(&claim))
        .expect("ordinary chat remains available");
    assert!(listed.is_empty() && words.is_empty() && memory.is_none());
    assert!(
        broker
            .run_command(
                &gateway,
                Some(&grant),
                Some(&claim),
                "storageprobe",
                &[],
                None
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
            max_output_bytes: 131_072,
            http: None,
            storage: Some(StorageConstraints {
                interface: StorageInterface::Jsonl,
                access,
                namespace: StorageNamespace::Chat,
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
async fn records_after_typed_acceptance_and_retrieves_after_restart() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let audit = Arc::new(InMemoryAuditLog::new(32).expect("audit"));
    let broker = build_broker(&root, Arc::clone(&audit)).await;
    let claim = claim();
    let grant = attestor_grant();
    let (capabilities, words, memory) = broker
        .capability_surface(&gateway(), Some(&grant), Some(&claim))
        .expect("chat scope accepted");
    assert_eq!(
        capabilities
            .iter()
            .map(|entry| entry.capability.id.as_str())
            .collect::<Vec<_>>(),
        ["memory.chat.recent", "memory.chat.search"]
    );
    assert_eq!(words, ["memory"]);
    assert!(memory.is_some());

    assert!(
        broker.capabilities(&gateway()).iter().all(|entry| {
            entry.provider.as_str() != "memory-chat"
                && !entry.capability.id.as_str().starts_with("memory.chat.")
        }),
        "the legacy listing reserves every capability the deployment routed to chat memory"
    );
    assert!(
        broker
            .command_words(&gateway())
            .iter()
            .all(|word| word != "memory")
    );
    assert!(
        broker
            .run_command(&gateway(), None, None, "memory", &[], None)
            .await
            .is_err(),
        "legacy command resolution never enters the memory provider"
    );
    for (index, attested) in [false, true].into_iter().enumerate() {
        let id = format!("reserved-route-{index}")
            .parse::<InvocationId>()
            .expect("invocation");
        let request = InvocationRequest {
            id: id.clone(),
            capability: MEMORY_RECENT.parse().expect("capability"),
            trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
            input: json!({}),
            secret_use: None,
        };
        let result = if attested {
            broker
                .invoke(
                    &gateway(),
                    Some(&grant),
                    Some(
                        &Attestation::for_subject(claim.subject.clone(), claim.agent.clone())
                            .bound_to(id),
                    ),
                    request,
                    Default::default(),
                )
                .await
        } else {
            broker
                .invoke(&gateway(), None, None, request, Default::default())
                .await
        }
        .expect("reserved route denial is audited");
        assert_eq!(
            result.result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
    }

    let mut swaps = Vec::new();
    let mut swapped = claim.clone();
    swapped.scope.as_mut().expect("chat scope").conversation.id = "c999999".to_owned();
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped
        .scope
        .as_mut()
        .expect("chat scope")
        .conversation
        .kind = ConversationKind::DirectMessage;
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped.scope.as_mut().expect("chat scope").transport =
        "other-slack".parse().expect("transport");
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped.scope.as_mut().expect("chat scope").kind = ChatTransportKind::Discord;
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped.agent = "other-agent".parse().expect("agent");
    swaps.push(swapped);
    for swapped in swaps {
        assert!(
            broker
                .capability_surface(&gateway(), Some(&grant), Some(&swapped))
                .is_none(),
            "every independently swapped selector field denies"
        );
    }
    // A grant on a channel implicitly authorizes every thread under it, but the storage namespace
    // keys on the whole conversation, so different threads under the same grant never share memory.
    let mut sibling_thread = claim.clone();
    sibling_thread
        .scope
        .as_mut()
        .expect("chat scope")
        .conversation
        .thread = Some("1712345678.999999".to_owned());
    assert!(
        broker
            .capability_surface(&gateway(), Some(&grant), Some(&sibling_thread))
            .is_some(),
        "a grant names a parent conversation, so its threads are covered by it"
    );
    assert_eq!(
        fs::read_dir(root.join("namespaces"))
            .expect("namespace root")
            .count(),
        0,
        "scope denials happen before namespace creation"
    );

    let generic_id = "generic-record"
        .parse::<InvocationId>()
        .expect("invocation");
    let generic = broker
        .invoke(
            &gateway(),
            Some(&grant),
            Some(&claim.bound_to(generic_id.clone())),
            InvocationRequest {
                id: generic_id,
                capability: "memory.chat.record".parse().expect("capability"),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input: json!({}),
                secret_use: None,
            },
            Default::default(),
        )
        .await
        .expect("generic record denial is accounted");
    assert_eq!(
        generic.result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(
        generic.result.error.as_deref(),
        Some("record-operation-required")
    );

    let record_id = "record-1".parse::<InvocationId>().expect("invocation");
    let record = broker
        .record_delivered_turn(
            &gateway(),
            Some(&grant),
            &claim.bound_to(record_id.clone()),
            DeliveredTurnRequest {
                id: record_id,
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                delivery: DeliveryIdentity::Slack {
                    channel: "c0123abc".to_owned(),
                    timestamp: "1712345678.000100".to_owned(),
                },
                user: "What shipped?".to_owned(),
                assistant: "Durable memory shipped.".to_owned(),
            },
        )
        .await
        .expect("record accounted");
    assert_eq!(
        record.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{record:?}"
    );
    let commitments = record
        .evidence
        .iter()
        .map(|evidence| evidence.digest.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(commitments.len(), record.evidence.len());
    assert!(
        commitments
            .iter()
            .all(|digest| digest.starts_with("sha256:"))
    );
    drop(broker);

    let audit_after_restart = Arc::new(InMemoryAuditLog::new(32).expect("audit"));
    let broker = build_broker(&root, audit_after_restart).await;
    let recent_id = "recent-1".parse::<InvocationId>().expect("invocation");
    let recent = broker
        .invoke(
            &gateway(),
            Some(&grant),
            Some(&claim.bound_to(recent_id.clone())),
            InvocationRequest {
                id: recent_id,
                capability: "memory.chat.recent".parse().expect("capability"),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input: json!({"last": 1}),
                secret_use: None,
            },
            Default::default(),
        )
        .await
        .expect("recent accounted");
    assert_eq!(
        recent.result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(
        recent
            .result
            .output
            .as_ref()
            .and_then(|value| value["turns"][0]["user"].as_str()),
        Some("What shipped?")
    );

    let physical_base = fs::read_dir(root.join("namespaces"))
        .expect("namespace paths")
        .next()
        .expect("one namespace")
        .expect("namespace entry")
        .file_name()
        .into_string()
        .expect("opaque UTF-8 token");
    let records = audit.records();
    assert_eq!(records.len(), 5);
    let encoded = serde_json::to_value(&records).expect("audit serializes");
    let attested = |event: &serde_json::Value| {
        assert_eq!(event["principal"], "maintainer", "{event}");
        assert_eq!(
            event["actor"],
            json!({"type": "agent", "agent": "reviewer"}),
            "{event}"
        );
        assert_eq!(event["via"], "gateway", "{event}");
        assert_eq!(event["attested_subject"], "slack.t0123abc.u9xyz", "{event}");
        assert_eq!(event["provider"], "memory-chat", "{event}");
        assert!(
            event["policy_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == "memory")),
            "{event}"
        );
    };
    for (index, record) in records.into_iter().enumerate() {
        let event = &encoded[index];
        for field in ["principal", "actor", "authorized_by", "policy_digest"] {
            assert!(
                event.get(field).is_some_and(|value| !value.is_null()),
                "a storage-backed record withheld {field}: {event}"
            );
        }
        assert_eq!(event["authorized_by"], "broker", "{event}");
        assert_eq!(event["policy_revision"], "memory-policy", "{event}");
        match record {
            AuditEvent::Decision {
                invocation,
                storage_scope_commitment,
                ..
            } => {
                if invocation.as_str() == "record-1" {
                    attested(event);
                    let scope = storage_scope_commitment.expect("scope commitment");
                    assert_ne!(scope.as_str().trim_start_matches("sha256:"), physical_base);
                }
            }
            AuditEvent::Execution {
                credential,
                storage_scope_commitment,
                storage,
                ..
            } => {
                attested(event);
                assert!(credential.is_none());
                assert!(storage_scope_commitment.is_some() && storage.is_some());
            }
        }
    }

    let tree = format!("{:?}", walk(&root));
    for sentinel in [
        "maintainer",
        "reviewer",
        "scientist-slack",
        "c0123abc",
        "What shipped",
        "memory-chat",
    ] {
        assert!(!tree.contains(sentinel), "storage path leaked {sentinel}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_wasm_b1_original_loads_are_independent_of_write_growth() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let mut config = memory_config();
    config.continuity_policy = ContinuityPolicy::Stable;
    config.max_lookback_turns = 1;
    config.max_recent_turns = 1;
    config.max_search_results = 1;
    config.max_turn_bytes = 1_000;
    config.max_dedup_records = 2_000;
    config.max_dedup_bytes = 262_144;
    config.compaction_target_bytes = 200_000;
    config.compaction_threshold_bytes = 262_144;
    let limits = StorageLimits {
        max_read_bytes_per_invocation: 524_288,
        ..StorageLimits::default()
    };
    let conversation = "c0123abc:1712345678.000430";
    let turns = seed_turn_file(262_000, 1_000);
    let mut dedup = Vec::new();
    for index in 0..1_206 {
        // Field order matches the checksum-pinned provider's canonical Dedup struct.
        let line = format!(
            "{{\"format\":\"dekopon.chat-memory.dedup\",\"version\":1,\"id\":\"sha256:{index:064x}\",\"commitment\":\"sha256:{}\"}}\n",
            "0".repeat(64)
        );
        assert_eq!(line.len(), 217);
        dedup.extend_from_slice(line.as_bytes());
    }
    assert_eq!(dedup.len(), 261_702);
    let user = "B1 user";
    let assistant = "x".repeat((1_000 - canonical_turn_line_bytes(user, "")) as usize);
    assert_eq!(canonical_turn_line_bytes(user, &assistant), 1_000);
    assert_eq!(turns.len() + dedup.len(), 523_702);
    assert!(turns.len() + dedup.len() + 1_000 > 524_288);
    let storage = StorageHost::open(&root, StorageLimits::default()).expect("seed host");
    let grant = storage
        .grant(StorageGrantRequest::new(
            "b1-seed".parse().expect("invocation"),
            MEMORY_RECORD.parse().expect("capability"),
            "memory-chat".parse().expect("provider"),
            StorageInterface::Jsonl,
            StorageAccess::ReadWrite,
            StorageNamespace::Chat,
            "reviewer".parse().expect("agent"),
            "slack.t0123abc.u9xyz".parse().expect("subject"),
            "slack",
            "scientist-slack",
            "c0123abc",
            conversation,
            ContinuityPolicy::Stable,
            b"b1-seed-authority".to_vec(),
        ))
        .expect("seed grant");
    let mut handle = storage.begin(grant).expect("seed handle");
    handle
        .jsonl_replace("turns.jsonl", 0, &turns)
        .expect("seed turns");
    handle
        .jsonl_replace("dedup.jsonl", 0, &dedup)
        .expect("seed dedup");
    handle.commit().expect("seed finish");
    drop(storage);
    let broker = build_broker_with(
        &root,
        Arc::new(InMemoryAuditLog::new(32).expect("audit")),
        config.clone(),
        limits,
        BrokerHostLimits::default(),
        false,
    )
    .await;
    let attestor = attestor_grant();
    let session = claim_for(conversation);
    let result = record_turn_in(
        &broker,
        &session,
        &attestor,
        "b1-record",
        "1712345678.000530",
        user,
        &assistant,
    )
    .await;
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "B1: separately write-charged growth must not consume original-load budget: {result:?}"
    );
    let data = walk(&root)
        .into_iter()
        .filter(|path| path.parent().is_some_and(|parent| parent.ends_with("data")))
        .map(|path| fs::read(path).expect("private data"))
        .collect::<Vec<_>>();
    let actual_dedup = data
        .iter()
        .find(|bytes| bytes.starts_with(&dedup))
        .expect("dedup preserved");
    assert_eq!(actual_dedup.len(), 261_702 + 217);
    assert_eq!(
        actual_dedup.iter().filter(|byte| **byte == b'\n').count(),
        1_207
    );
    let compacted = data
        .iter()
        .find(|bytes| {
            bytes
                .windows(b"dekopon.chat-memory.turn".len())
                .any(|window| window == b"dekopon.chat-memory.turn")
        })
        .expect("compacted turns");
    assert!(compacted.len() <= config.compaction_target_bytes as usize);
    assert!(compacted.len() < turns.len());
    let recent = query_memory_in(
        &broker,
        &session,
        &attestor,
        "b1-recent",
        MEMORY_RECENT,
        json!({"last": 1}),
    )
    .await;
    assert_eq!(recent["turns"][0]["user"], user);
    assert_eq!(recent["turns"][0]["assistant"], assistant);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_memory_namespace_is_reset_by_the_invocation_that_finds_it() {
    let capture = CaptureLayer::workspace();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .expect("no other test in this binary installs a global subscriber");
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let broker = build_broker(&root, Arc::new(InMemoryAuditLog::new(32).expect("audit"))).await;
    assert_eq!(
        record_turn(
            &broker,
            "reset-record",
            "1712345678.000130",
            "remember this",
            "noted"
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let generations = generation_count(&root);

    let base = fs::read_dir(root.join("namespaces"))
        .expect("namespace root")
        .next()
        .expect("one base")
        .expect("base")
        .path();
    let pointer = base.join("current");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&pointer).expect("pointer")).expect("pointer document");
    document["authority"] = serde_json::Value::from("not-a-token");
    fs::write(&pointer, serde_json::to_vec(&document).expect("encode")).expect("corrupt pointer");

    let id = "reset-found".parse::<InvocationId>().expect("invocation");
    let refused = broker
        .invoke(
            &gateway(),
            Some(&attestor_grant()),
            Some(&claim().bound_to(id.clone())),
            InvocationRequest {
                id,
                capability: MEMORY_RECENT.parse().expect("capability"),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input: json!({"last": 1}),
                secret_use: None,
            },
            Default::default(),
        )
        .await
        .expect_err("the invocation that finds the corruption fails");
    assert_eq!(refused.storage_failure_code(), Some("storage-corrupt"));
    assert!(refused.storage_namespace_reset(), "{refused}");

    let parents = capture
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::Event { fields, parent, .. } if fields.contains("storage_namespace_reset") => {
                Some(parent)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(parents, [Some("broker.execute".to_owned())]);
    assert!(
        capture.spans().iter().any(
            |(name, fields)| *name == "broker.execute" && fields.contains("storage.reset=true")
        ),
        "{}",
        capture.spans_text()
    );
    let token = base
        .file_name()
        .expect("base token")
        .to_string_lossy()
        .into_owned();
    let named = format!("storage.namespace=\"{token}\"");
    assert!(
        capture
            .spans()
            .iter()
            .any(|(name, fields)| *name == "broker.execute" && fields.contains(&named)),
        "{}",
        capture.spans_text()
    );
    let spans = capture.spans();
    let recorded = |span: &str, fragment: &str| {
        spans
            .iter()
            .any(|(name, fields)| *name == span && fields.contains(fragment))
    };
    assert!(
        spans
            .iter()
            .any(|(name, fields)| *name == "broker.authorize"
                && fields.contains(" invocation=reset-found")
                && fields.contains(&format!(" capability={MEMORY_RECENT}"))),
        "{}",
        capture.spans_text()
    );
    for fragment in [
        " subject=slack.t0123abc.u9xyz",
        " via=gateway",
        r#" input={"operation":"recent","last":1,"#,
    ] {
        assert!(
            recorded("broker.authorize", fragment),
            "{fragment}: {}",
            capture.spans_text()
        );
    }
    for fragment in [
        " capability=memory.chat.record",
        r#" input={"operation":"record","#,
        " storage=true",
    ] {
        assert!(
            recorded("provider.invoke", fragment),
            "{fragment}: {}",
            capture.spans_text()
        );
    }

    assert_recent_empty(&broker, "reset-retry").await;
    drop(broker);
    assert_eq!(generation_count(&root), generations + 1);
}

async fn record_turn(
    broker: &Broker<InMemoryAuditLog>,
    invocation: &str,
    timestamp: &str,
    user: &str,
    assistant: &str,
) -> dekopon_capability::InvocationResult {
    record_turn_in(
        broker,
        &claim(),
        &attestor_grant(),
        invocation,
        timestamp,
        user,
        assistant,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the fixture exposes every dedup and namespace input independently"
)]
async fn record_turn_in(
    broker: &Broker<InMemoryAuditLog>,
    claim: &Attestation,
    grant: &AttestorGrant,
    invocation: &str,
    timestamp: &str,
    user: &str,
    assistant: &str,
) -> dekopon_capability::InvocationResult {
    let id = invocation.parse::<InvocationId>().expect("invocation");
    broker
        .record_delivered_turn(
            &gateway(),
            Some(grant),
            &claim.bound_to(id.clone()),
            DeliveredTurnRequest {
                id,
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                delivery: DeliveryIdentity::Slack {
                    channel: "c0123abc".to_owned(),
                    timestamp: timestamp.to_owned(),
                },
                user: user.to_owned(),
                assistant: assistant.to_owned(),
            },
        )
        .await
        .expect("record accounted")
}

async fn query_memory(
    broker: &Broker<InMemoryAuditLog>,
    invocation: &str,
    capability: &str,
    input: serde_json::Value,
) -> serde_json::Value {
    let result = query_memory_result(broker, invocation, capability, input).await;
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{result:?}"
    );
    result.output.expect("query output")
}

async fn query_memory_result(
    broker: &Broker<InMemoryAuditLog>,
    invocation: &str,
    capability: &str,
    input: serde_json::Value,
) -> dekopon_capability::InvocationResult {
    query_memory_result_in(
        broker,
        &claim(),
        &attestor_grant(),
        invocation,
        capability,
        input,
    )
    .await
}

async fn query_memory_result_in(
    broker: &Broker<InMemoryAuditLog>,
    claim: &Attestation,
    grant: &AttestorGrant,
    invocation: &str,
    capability: &str,
    input: serde_json::Value,
) -> dekopon_capability::InvocationResult {
    let id = invocation.parse::<InvocationId>().expect("invocation");
    broker
        .invoke(
            &gateway(),
            Some(grant),
            Some(&claim.bound_to(id.clone())),
            InvocationRequest {
                id,
                capability: capability.parse().expect("capability"),
                trace_parent: TRACE_PARENT.parse().expect("valid traceparent fixture"),
                input,
                secret_use: None,
            },
            Default::default(),
        )
        .await
        .expect("query accounted")
        .result
}

async fn query_memory_in(
    broker: &Broker<InMemoryAuditLog>,
    claim: &Attestation,
    grant: &AttestorGrant,
    invocation: &str,
    capability: &str,
    input: serde_json::Value,
) -> serde_json::Value {
    let result = query_memory_result_in(broker, claim, grant, invocation, capability, input).await;
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    result.output.expect("query output")
}

#[tokio::test(flavor = "multi_thread")]
async fn two_authorized_conversations_remain_physically_and_logically_isolated() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let broker = build_broker(&root, Arc::new(InMemoryAuditLog::new(32).expect("audit"))).await;
    let first = claim_for("c0123abc:1712345678.000100");
    let second = claim_for("c0123abc:1712345678.000200");
    let grant = attestor_grant();

    assert_eq!(
        record_turn_in(
            &broker,
            &first,
            &grant,
            "scope-first-record",
            "1712345678.000101",
            "first scope sentinel",
            "first answer",
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let empty_second = query_memory_in(
        &broker,
        &second,
        &grant,
        "scope-second-empty",
        "memory.chat.recent",
        json!({"last": 2}),
    )
    .await;
    assert!(empty_second["turns"].as_array().expect("turns").is_empty());

    assert_eq!(
        record_turn_in(
            &broker,
            &second,
            &grant,
            "scope-second-record",
            "1712345678.000201",
            "second scope sentinel",
            "second answer",
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let first_result = query_memory_in(
        &broker,
        &first,
        &grant,
        "scope-first-query",
        "memory.chat.recent",
        json!({"last": 2}),
    )
    .await;
    let second_result = query_memory_in(
        &broker,
        &second,
        &grant,
        "scope-second-query",
        "memory.chat.recent",
        json!({"last": 2}),
    )
    .await;
    assert_eq!(first_result["turns"][0]["user"], "first scope sentinel");
    assert_eq!(second_result["turns"][0]["user"], "second scope sentinel");
    assert_eq!(
        fs::read_dir(root.join("namespaces"))
            .expect("namespace root")
            .count(),
        2,
        "each canonical conversation receives a distinct opaque base namespace"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn selected_symbolic_credential_rotates_authority_without_hashing_its_value() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let config = memory_config();
    let broker = build_broker_with_principal(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        config.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        Some(("surface-token-a", "secret-value-one")),
        false,
    )
    .await;
    assert_eq!(
        record_turn(
            &broker,
            "credential-surface-record",
            "1712345678.000350",
            "credential A sentinel",
            "credential A answer",
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    drop(broker);

    let broker = build_broker_with_principal(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        config.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        Some(("surface-token-a", "secret-value-two")),
        false,
    )
    .await;
    let same_name = query_memory(
        &broker,
        "credential-secret-value-change",
        "memory.chat.recent",
        json!({"last": 1}),
    )
    .await;
    assert_eq!(same_name["turns"][0]["user"], "credential A sentinel");
    drop(broker);
    assert_eq!(generation_count(&root), 1);

    let broker = build_broker_with_principal(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        config.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        Some(("surface-token-b", "secret-value-two")),
        false,
    )
    .await;
    assert_recent_empty(&broker, "credential-surface-b").await;
    drop(broker);
    assert_eq!(generation_count(&root), 2);

    let broker = build_broker_with_principal(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        config,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        Some(("surface-token-a", "secret-value-three")),
        false,
    )
    .await;
    assert_recent_empty(&broker, "credential-surface-a-again").await;
    drop(broker);
    assert_eq!(generation_count(&root), 3);

    let tree = walk(&root)
        .into_iter()
        .filter_map(|path| fs::read(path).ok())
        .flatten()
        .collect::<Vec<_>>();
    for secret in ["secret-value-one", "secret-value-two", "secret-value-three"] {
        assert!(
            !tree
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "credential values must never enter storage authority material"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_stable_memory_survives_semantic_authority_changes() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let mut stable = memory_config();
    stable.continuity_policy = ContinuityPolicy::Stable;
    let broker = build_broker_with(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        stable.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;
    assert_eq!(
        record_turn(
            &broker,
            "stable-record",
            "1712345678.000400",
            "stable continuity sentinel",
            "stable answer",
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    drop(broker);

    let mut changed_host = BrokerHostLimits::default();
    changed_host.max_tables += 1;
    let broker = build_broker_with(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        stable,
        StorageLimits::default(),
        changed_host,
        false,
    )
    .await;
    let recent = query_memory(
        &broker,
        "stable-after-authority-change",
        "memory.chat.recent",
        json!({"last": 1}),
    )
    .await;
    assert_eq!(recent["turns"][0]["user"], "stable continuity sentinel");
    assert_eq!(generation_count(&root), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_memory_authority_never_rotates_generic_durable_storage() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let mut memory = memory_config();
    memory.enabled_agents = vec!["other-agent".parse().expect("agent")];
    let broker = build_broker_with_principal(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        memory.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        None,
        true,
    )
    .await;
    invoke_generic_storage_denial(&broker, "generic-authority-a").await;
    drop(broker);
    assert_eq!(generation_count(&root), 1);

    memory.max_recent_turns -= 1;
    let broker = build_broker_with_principal(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        memory,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        None,
        true,
    )
    .await;
    invoke_generic_storage_denial(&broker, "generic-authority-b").await;
    drop(broker);
    assert_eq!(
        generation_count(&root),
        1,
        "unreachable exact memory capabilities rotated an unrelated durable-files namespace"
    );
}

async fn invoke_generic_storage_denial(broker: &Broker<InMemoryAuditLog>, invocation: &str) {
    let session = claim();
    let id = invocation.parse::<InvocationId>().expect("invocation");
    let result = broker
        .invoke(
            &gateway(),
            Some(&attestor_grant()),
            Some(&session.bound_to(id.clone())),
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
        .expect("storage denial is accounted");
    assert_eq!(
        result.result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(result.result.error.as_deref(), Some("storage-quota"));
}

#[tokio::test(flavor = "multi_thread")]
async fn authority_surface_ignores_order_and_denied_provider_but_rotates_every_semantic_ceiling() {
    let compiled = tempfile::tempdir().expect("compiled providers");
    let options = BrokerHostOptions {
        cwasm_dir: Some(compiled.path().to_owned()),
        ..BrokerHostOptions::default()
    };
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");

    let mut baseline_memory = memory_config();
    baseline_memory
        .enabled_agents
        .push("other-agent".parse().expect("agent"));
    let broker = build_broker_with_options(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        None,
        false,
        &options,
    )
    .await;
    assert_eq!(
        record_turn(
            &broker,
            "surface-a-record",
            "1712345678.000500",
            "authority A sentinel",
            "authority A answer",
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    drop(broker);
    assert_eq!(generation_count(&root), 1);

    let cold_artifacts = snapshot_tree_bytes(compiled.path());
    assert_eq!(
        cold_artifacts
            .iter()
            .filter(|(path, _, _)| path.extension().is_some_and(|ext| ext == "cwasm"))
            .count(),
        3,
        "the first registry compiles the three distinct provider fixtures"
    );

    let broker = build_broker_with_options(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer-v2",
        None,
        false,
        &options,
    )
    .await;
    let remapped = query_memory(
        &broker,
        "surface-principal-remap",
        "memory.chat.recent",
        json!({"last": 1}),
    )
    .await;
    assert_eq!(remapped["turns"][0]["user"], "authority A sentinel");
    drop(broker);
    assert_eq!(generation_count(&root), 1);

    let mut reordered = baseline_memory.clone();
    reordered.enabled_agents.reverse();
    let broker = build_broker_with_options(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        reordered,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        true,
        "maintainer",
        None,
        false,
        &options,
    )
    .await;
    let order_only = query_memory(
        &broker,
        "surface-order-only",
        "memory.chat.recent",
        json!({"last": 1}),
    )
    .await;
    assert_eq!(order_only["turns"][0]["user"], "authority A sentinel");
    drop(broker);
    assert_eq!(
        generation_count(&root),
        1,
        "provider/enabled-agent ordering and an unrelated denied provider do not rotate"
    );

    let mut host_limits = BrokerHostLimits::default();
    host_limits.max_tables += 1;
    let broker = build_broker_with_options(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        host_limits,
        false,
        "maintainer",
        None,
        false,
        &options,
    )
    .await;
    assert_recent_empty(&broker, "surface-host").await;
    drop(broker);
    assert_eq!(generation_count(&root), 2);

    let broker = build_broker_with_options(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        None,
        false,
        &options,
    )
    .await;
    assert_recent_empty(&broker, "surface-a-again").await;
    drop(broker);
    assert_eq!(generation_count(&root), 3);

    let mut memory_limit = baseline_memory.clone();
    memory_limit.max_recent_turns -= 1;
    let broker = build_broker_with_options(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        memory_limit,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer",
        None,
        false,
        &options,
    )
    .await;
    assert_recent_empty(&broker, "surface-memory-limit").await;
    drop(broker);
    assert_eq!(generation_count(&root), 4);

    let storage_limit = StorageLimits {
        max_open_handles: StorageLimits::default().max_open_handles - 1,
        ..StorageLimits::default()
    };
    let broker = build_broker_with_options(
        &root,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory,
        storage_limit,
        BrokerHostLimits::default(),
        false,
        "maintainer",
        None,
        false,
        &options,
    )
    .await;
    assert_recent_empty(&broker, "surface-storage-limit").await;
    drop(broker);
    assert_eq!(generation_count(&root), 5);
    assert_eq!(
        snapshot_tree_bytes(compiled.path()),
        cold_artifacts,
        "authority changes and provider ordering must reuse immutable compiled artifacts"
    );
    compiled.close().expect("compiled fixtures can be removed");
}

async fn assert_recent_empty(broker: &Broker<InMemoryAuditLog>, invocation: &str) {
    let result = query_memory(broker, invocation, "memory.chat.recent", json!({"last": 1})).await;
    assert!(
        result["turns"].as_array().expect("turns").is_empty(),
        "retired authority generation became visible to {invocation}"
    );
}

fn generation_count(root: &Path) -> usize {
    let base = fs::read_dir(root.join("namespaces"))
        .expect("namespace root")
        .next()
        .expect("one base")
        .expect("base")
        .path();
    fs::read_dir(base)
        .expect("base entries")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count()
}

#[derive(serde::Serialize)]
struct SeedTurn<'a> {
    format: &'static str,
    version: u8,
    id: String,
    commitment: String,
    user: &'a str,
    assistant: String,
}

fn canonical_turn_line_bytes(user: &str, assistant: &str) -> u64 {
    let commitment = format!("sha256:{}", "0".repeat(64));
    serde_json::to_vec(&SeedTurn {
        format: "dekopon.chat-memory.turn",
        version: 1,
        id: commitment.clone(),
        commitment,
        user,
        assistant: assistant.to_owned(),
    })
    .expect("canonical turn")
    .len() as u64
        + 1
}

fn seed_turn_file(target: u64, maximum_line: u64) -> Vec<u8> {
    const RECORDS: usize = 400;
    let commitment = format!("sha256:{}", "0".repeat(64));
    let minimum_lines = (0..RECORDS)
        .map(|index| {
            serde_json::to_vec(&SeedTurn {
                format: "dekopon.chat-memory.turn",
                version: 1,
                id: format!("sha256:{index:064x}"),
                commitment: commitment.clone(),
                user: "seed",
                assistant: String::new(),
            })
            .expect("minimum seed turn")
            .len() as u64
                + 1
        })
        .collect::<Vec<_>>();
    assert!(minimum_lines.iter().all(|line| *line < maximum_line));
    let mut remaining = target;
    let mut output = Vec::with_capacity(target as usize);
    for index in 0..RECORDS {
        let minimum_after = minimum_lines[index + 1..].iter().sum::<u64>();
        let line_target = maximum_line.min(
            remaining
                .checked_sub(minimum_after)
                .expect("target fits remaining minimum lines"),
        );
        let filler = line_target
            .checked_sub(minimum_lines[index])
            .expect("line has filler headroom");
        let line = serde_json::to_vec(&SeedTurn {
            format: "dekopon.chat-memory.turn",
            version: 1,
            id: format!("sha256:{index:064x}"),
            commitment: commitment.clone(),
            user: "seed",
            assistant: "x".repeat(filler as usize),
        })
        .expect("seed turn");
        assert_eq!(line.len() as u64 + 1, line_target);
        output.extend_from_slice(&line);
        output.push(b'\n');
        remaining -= line_target;
    }
    assert_eq!(remaining, 0);
    output
}

fn snapshot_tree_bytes(path: &Path) -> Vec<(PathBuf, u32, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, output: &mut Vec<(PathBuf, u32, Vec<u8>)>) {
        let mut entries = fs::read_dir(path)
            .expect("snapshot directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("snapshot entries");
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("snapshot metadata");
            let contents = if metadata.is_file() {
                fs::read(&path).expect("snapshot file")
            } else {
                Vec::new()
            };
            output.push((
                path.strip_prefix(root)
                    .expect("relative path")
                    .to_path_buf(),
                metadata.permissions().mode(),
                contents,
            ));
            if metadata.is_dir() {
                visit(root, &path, output);
            }
        }
    }
    let mut output = Vec::new();
    visit(path, path, &mut output);
    output
}

fn walk(path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![path.to_path_buf()];
    if path.is_dir() {
        for entry in fs::read_dir(path).expect("read tree") {
            paths.extend(walk(&entry.expect("entry").path()));
        }
    }
    paths
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_memory_without_routes_names_every_missing_role() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let storage = StorageHost::open(&root, StorageLimits::default()).expect("storage host");
    let registry = BrokerProviderRegistry::load_with_storage(
        [provider_fixture("memory-chat-provider.wasm")],
        BrokerHostLimits::default(),
        Some(storage),
    )
    .await
    .expect("memory fixture loads with storage");
    let world = PolicyWorld::new(
        ["caller".parse::<PrincipalId>().expect("caller")],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("world");
    let unrouted = |effect, risk, access| {
        let mut set = memory_constraint(CapabilityRoute::ChatMemoryRecord, effect, risk, access);
        set.route = CapabilityRoute::Generic;
        set
    };
    let constraints = ConstraintCatalog::new([
        (
            MEMORY_RECORD.parse().expect("capability"),
            unrouted(
                EffectKind::LocalWrite,
                RiskLevel::Medium,
                StorageAccess::ReadWrite,
            ),
        ),
        (
            MEMORY_RECENT.parse().expect("capability"),
            unrouted(
                EffectKind::ReadOnly,
                RiskLevel::High,
                StorageAccess::ReadOnly,
            ),
        ),
        (
            "memory.chat.search".parse().expect("capability"),
            unrouted(
                EffectKind::ReadOnly,
                RiskLevel::High,
                StorageAccess::ReadOnly,
            ),
        ),
    ])
    .expect("an unrouted catalog is a valid catalog");
    let broker = Broker::new(
        registry,
        "broker".parse().expect("broker"),
        "unrouted-memory-policy".to_owned(),
        PolicyEngine::new("", &world).expect("empty policy"),
        constraints,
        CredentialStore::empty(),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(8).expect("audit")),
        BrokerLimits::default(),
    )
    .expect("a deployment with no declared routes still starts");
    let Err(error) = broker.with_chat_memory(memory_config()) else {
        panic!("chatMemory must not compose with an unrouted catalog");
    };
    let rendered = error.to_string();
    let BrokerBuildError::UnroutedChatMemory { roles } = error else {
        panic!("an unrouted catalog must be its own build error: {rendered}");
    };
    assert_eq!(
        roles,
        vec![
            CapabilityRoute::ChatMemoryRecord,
            CapabilityRoute::ChatMemoryRecent,
            CapabilityRoute::ChatMemorySearch,
        ],
        "every missing role is reported at once, not the first: {rendered}"
    );
    for fragment in [
        "route:",
        "chatMemoryRecord",
        "chatMemoryRecent",
        "chatMemorySearch",
        "exactly one constraint set",
        "docs/upgrading.md",
    ] {
        assert!(
            rendered.contains(fragment),
            "the refusal must name {fragment}: {rendered}"
        );
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
            max_output_bytes: 131_072,
            http: None,
            storage,
            secret_use: None,
        },
    };
    let read_only = Some(StorageConstraints {
        interface: StorageInterface::Jsonl,
        access: StorageAccess::ReadOnly,
        namespace: StorageNamespace::Chat,
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
