#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use dekopon_broker::{
    AttestorGrant, AuditEvent, AuthenticatedContext, Broker, BrokerBuildError, BrokerError,
    BrokerLimits, CapabilityRoute, ChatAttestation, ChatMemoryConfig, ChatScopeGrant,
    ChatSessionClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet, CredentialStore,
    DeliveredTurnRequest, DeliveryIdentity, IdentityDirectory, InMemoryAuditLog, PolicyEngine,
    PolicyWorld, RouteConflict, SubjectAttestation,
};

/// Capability identifiers the shipped `memory-chat` provider declares.
///
/// Fixtures only. The broker reserves the surface by the declared `route`, so these are the
/// component's names rather than anything the broker compares against.
const MEMORY_RECORD: &str = "memory.chat.record";
const MEMORY_RECENT: &str = "memory.chat.recent";
use dekopon_broker_host::{BoundCredential, BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{ChatScopeClaim, InvocationRequest};
use dekopon_capability::{
    EffectKind, HttpConstraints, Idempotency, StorageAccess, StorageConstraints, StorageInterface,
    StorageNamespace,
};
use dekopon_core::{
    Actor, AgentId, ExternalSubject, InvocationId, PrincipalId, Redacted, RiskLevel, TransportId,
};
use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits};
use dekopon_test_support::provider_fixture;
use serde_json::json;

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
            Idempotency::Conditional,
            StorageAccess::ReadWrite,
        ),
        (
            "memory.chat.recent",
            CapabilityRoute::ChatMemoryRecent,
            EffectKind::ReadOnly,
            RiskLevel::High,
            Idempotency::Idempotent,
            StorageAccess::ReadOnly,
        ),
        (
            "memory.chat.search",
            CapabilityRoute::ChatMemorySearch,
            EffectKind::ReadOnly,
            RiskLevel::High,
            Idempotency::Idempotent,
            StorageAccess::ReadOnly,
        ),
    ]
    .into_iter()
    .map(|(id, route, effect, risk, idempotency, access)| {
        let capability = id.parse().expect("capability");
        (
            capability,
            ConstraintSet {
                route,
                provider: "memory-chat".parse().expect("provider"),
                effect,
                risk,
                idempotency,
                credential: None,
                credential_by_agent: Default::default(),
                constraints: dekopon_capability::ExecutionConstraints {
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
            idempotency: Idempotency::Conditional,
            credential: None,
            credential_by_agent: Default::default(),
            constraints: dekopon_capability::ExecutionConstraints {
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
                idempotency: Idempotency::Idempotent,
                credential: Some(credential.to_owned()),
                credential_by_agent: Default::default(),
                constraints: dekopon_capability::ExecutionConstraints {
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

async fn build_broker(
    root: &Path,
    key: &Path,
    audit: Arc<InMemoryAuditLog>,
) -> Broker<InMemoryAuditLog> {
    build_broker_with(
        root,
        key,
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
    key: &Path,
    audit: Arc<InMemoryAuditLog>,
    memory: ChatMemoryConfig,
    storage_limits: StorageLimits,
    host_limits: BrokerHostLimits,
    reverse_provider_order: bool,
) -> Broker<InMemoryAuditLog> {
    build_broker_with_principal(
        root,
        key,
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
    key: &Path,
    audit: Arc<InMemoryAuditLog>,
    memory: ChatMemoryConfig,
    storage_limits: StorageLimits,
    host_limits: BrokerHostLimits,
    reverse_provider_order: bool,
    mapped_principal: &str,
    authority_credential: Option<(&str, &str)>,
    permit_generic_storage: bool,
) -> Broker<InMemoryAuditLog> {
    let storage = StorageHost::open(root, key, storage_limits).expect("storage host");
    let mut providers = vec![
        provider_fixture("memory-chat-provider.wasm"),
        provider_fixture("echo-provider.wasm"),
        provider_fixture("storage-probe-provider.wasm"),
    ];
    if authority_credential.is_some() {
        providers.push(provider_fixture("http-probe-provider.wasm"));
    }
    if reverse_provider_order {
        providers.reverse();
    }
    let registry = BrokerProviderRegistry::load_with_storage(providers, host_limits, Some(storage))
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
        when { context has via && context.via == "gateway"
            && context has transportKind && context.transportKind == "slack"
            && context has transport && context.transport == "scientist-slack"
            && context has channel && context.channel == "c0123abc"
            && context has conversation };

        @id("memory")
        permit(principal == Dekopon::Principal::"$PRINCIPAL",
               action in [Dekopon::Action::"memory.chat.record",
                          Dekopon::Action::"memory.chat.recent",
                          Dekopon::Action::"memory.chat.search"],
               resource == Dekopon::Provider::"memory-chat")
        when { context has via && context.via == "gateway"
            && context has agent && context.agent == "reviewer"
            && context has transportKind && context.transportKind == "slack"
            && context has transport && context.transport == "scientist-slack"
            && context has channel && context.channel == "c0123abc"
            && context has conversation };
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
        when { context has via && context.via == "gateway"
            && context has agent && context.agent == "reviewer" };
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

fn claim() -> ChatSessionClaim {
    claim_for("c0123abc:1712345678.000100")
}

fn claim_for(conversation: &str) -> ChatSessionClaim {
    ChatSessionClaim {
        subject: "slack.t0123abc.u9xyz"
            .parse::<ExternalSubject>()
            .expect("subject"),
        agent: "reviewer".parse::<AgentId>().expect("agent"),
        scope: ChatScopeClaim {
            transport: "scientist-slack".parse::<TransportId>().expect("transport"),
            kind: ChatTransportKind::Slack,
            channel: "c0123abc".to_owned(),
            conversation: conversation.to_owned(),
        },
    }
}

fn grant() -> AttestorGrant {
    grant_for(&["c0123abc:1712345678.000100"])
}

fn grant_for(conversations: &[&str]) -> AttestorGrant {
    AttestorGrant {
        namespaces: vec!["slack.t0123abc".to_owned()],
        chat_scopes: conversations
            .iter()
            .map(|conversation| ChatScopeGrant::ExactConversation {
                kind: ChatTransportKind::Slack,
                transport: "scientist-slack".parse().expect("transport"),
                channel: "c0123abc".to_owned(),
                conversation: (*conversation).to_owned(),
                local_subject_service: None,
            })
            .collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn authorization_audit_failure_precedes_every_storage_tree_mutation() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let audit = Arc::new(InMemoryAuditLog::new(1).expect("one-record audit"));
    let broker = build_broker_with(
        &root,
        &key,
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
    assert_eq!(audit.records().await.len(), 1);
    let before = snapshot_tree_bytes(&root);

    let id = "audit-full-record"
        .parse::<InvocationId>()
        .expect("invocation");
    let attestor = grant();
    let session = claim();
    let error = broker
        .record_delivered_turn_for_chat(
            &gateway(),
            Some(&attestor),
            &ChatAttestation {
                subject: session.subject,
                agent: session.agent,
                scope: session.scope,
                invocation: id.clone(),
            },
            DeliveredTurnRequest {
                id,
                trace: "trace-audit-full-record".parse().expect("trace"),
                trace_parent: None,
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
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let broker = build_broker_with_principal(
        &root,
        &key,
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
    // One authorization pass, two derived listings: a combined view that disagreed with either
    // listing would break the guarantee that what a session is shown is what it may invoke.
    assert_eq!(
        broker.capability_view(&direct),
        (broker.capabilities(&direct), broker.command_words(&direct))
    );

    let session = claim();
    let legacy_grant = AttestorGrant {
        namespaces: vec!["slack.t0123abc".to_owned()],
        chat_scopes: Vec::new(),
    };
    let (legacy_capabilities, legacy_words) = broker
        .capabilities_for(
            &gateway(),
            Some(&legacy_grant),
            &session.subject,
            &session.agent,
        )
        .expect("legacy subject-only chat remains authorized");
    assert!(
        legacy_capabilities
            .iter()
            .all(|entry| entry.capability.id.as_str() != storage_id)
    );
    assert!(!legacy_words.iter().any(|word| word == storage_word));

    let (scoped_capabilities, scoped_words, _) = broker
        .capabilities_for_chat(&gateway(), Some(&grant()), &session)
        .expect("scoped chat is authorized");
    assert!(
        scoped_capabilities
            .iter()
            .any(|entry| entry.capability.id.as_str() == storage_id)
    );
    assert!(scoped_words.iter().any(|word| word == storage_word));
}
/// The reserved chat-memory surface is the operator's declaration, not a spelling.
///
/// The fixture is deliberately hostile in every way a name can be: its provider identity is
/// `memory-chat` and one of its capabilities is `memory.chat.export`. With no `route` declared,
/// both are ordinary capabilities on every path — which is the whole point of typing the route,
/// because the reservation now follows what an operator wrote rather than what they happened to
/// call something.
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
            "caller".parse::<PrincipalId>().expect("caller"),
            "gateway".parse().expect("gateway"),
            "maintainer".parse().expect("maintainer"),
        ],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("policy world");
    let policy = PolicyEngine::new(
        r#"
        permit(principal == Dekopon::Principal::"caller",
               action in [Dekopon::Action::"ordinary.escape",
                          Dekopon::Action::"memory.chat.export"],
               resource == Dekopon::Provider::"memory-chat")
        unless { context has via };
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context has via && context.via == "gateway" };
        permit(principal == Dekopon::Principal::"maintainer",
               action in [Dekopon::Action::"ordinary.escape",
                          Dekopon::Action::"memory.chat.export"],
               resource == Dekopon::Provider::"memory-chat")
        when { context has via && context.via == "gateway"
            && context has agent && context.agent == "reviewer" };
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
    let caller = AuthenticatedContext::new(
        "caller".parse().expect("caller"),
        Actor::Service {
            principal: "caller".parse().expect("caller"),
        },
    )
    .expect("caller context");
    assert_eq!(
        broker
            .capabilities(&caller)
            .iter()
            .map(|entry| entry.capability.id.as_str().to_owned())
            .collect::<Vec<_>>(),
        ["memory.chat.export", "ordinary.escape"],
        "an undeclared route hides nothing, however the capability is spelled"
    );
    assert_eq!(broker.command_words(&caller), ["recall"]);
    broker
        .resolve_command("recall", &[])
        .await
        .expect("the word of a provider that owns no route resolves normally");

    for (index, capability) in ["ordinary.escape", "memory.chat.export"]
        .into_iter()
        .enumerate()
    {
        let result = broker
            .invoke(
                &caller,
                InvocationRequest {
                    id: format!("unrouted-direct-{index}")
                        .parse()
                        .expect("invocation"),
                    capability: capability.parse().expect("capability"),
                    trace: format!("trace-unrouted-direct-{index}")
                        .parse()
                        .expect("trace"),
                    trace_parent: None,
                    input: json!({}),
                    secret_use: None,
                },
            )
            .await
            .expect("ordinary invocation is audited");
        assert_eq!(
            result.outcome,
            dekopon_capability::InvocationOutcome::Succeeded,
            "{result:?}"
        );
    }

    let gateway = gateway();
    let grant = AttestorGrant {
        namespaces: vec!["slack.t0123abc".to_owned()],
        chat_scopes: Vec::new(),
    };
    let claim = claim();
    let (listed, _) = broker
        .capabilities_for(&gateway, Some(&grant), &claim.subject, &claim.agent)
        .expect("legacy attestation is honored");
    assert_eq!(listed.len(), 2, "the attested listing reserves nothing");
    let (listed, words, memory) = broker
        .capabilities_for_chat(&gateway, Some(&grant), &claim)
        .expect("ordinary chat remains available");
    assert_eq!(listed.len(), 2);
    assert_eq!(words, ["recall"]);
    assert!(memory.is_none(), "no route means no memory surface");
    broker
        .resolve_command_for_chat(&gateway, Some(&grant), &claim, "recall", &[])
        .await
        .expect("chat resolution reserves nothing either");
    let chat_id = "unrouted-chat".parse::<InvocationId>().expect("invocation");
    let chat_result = broker
        .invoke_for_chat(
            &gateway,
            Some(&grant),
            &ChatAttestation {
                subject: claim.subject.clone(),
                agent: claim.agent.clone(),
                scope: claim.scope.clone(),
                invocation: chat_id.clone(),
            },
            InvocationRequest {
                id: chat_id,
                capability: "ordinary.escape".parse().expect("capability"),
                trace: "trace-unrouted-chat".parse().expect("trace"),
                trace_parent: None,
                input: json!({}),
                secret_use: None,
            },
        )
        .await
        .expect("chat invocation is audited");
    assert_eq!(
        chat_result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{chat_result:?}"
    );

    let id = "unrouted-attested"
        .parse::<InvocationId>()
        .expect("invocation");
    let result = broker
        .invoke_for(
            &gateway,
            Some(&grant),
            &SubjectAttestation {
                subject: claim.subject,
                agent: claim.agent,
                invocation: id.clone(),
            },
            InvocationRequest {
                id,
                capability: "ordinary.escape".parse().expect("capability"),
                trace: "trace-unrouted-attested".parse().expect("trace"),
                trace_parent: None,
                input: json!({}),
                secret_use: None,
            },
        )
        .await
        .expect("attested invocation is audited");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded,
        "{result:?}"
    );
    drop(broker);

    // Declaring the three routes correctly is still not enough: the routed provider must own
    // exactly those three capabilities, so a component with a fourth route can never be the one
    // holding a conversation's storage authority.
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let storage = StorageHost::open(&root, &key, StorageLimits::default()).expect("storage host");
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
                Idempotency::Conditional,
                StorageAccess::ReadWrite,
            ),
        ),
        (
            "memory.chat.recent".parse().expect("capability"),
            memory_constraint(
                CapabilityRoute::ChatMemoryRecent,
                EffectKind::ReadOnly,
                RiskLevel::High,
                Idempotency::Idempotent,
                StorageAccess::ReadOnly,
            ),
        ),
        (
            "memory.chat.search".parse().expect("capability"),
            memory_constraint(
                CapabilityRoute::ChatMemorySearch,
                EffectKind::ReadOnly,
                RiskLevel::High,
                Idempotency::Idempotent,
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

/// Renaming the shipped provider changes nothing the broker hides or denies.
///
/// `storage-probe` is named nothing like chat memory and declares `storage-probe.run`, but the
/// deployment routes it as the record half of the surface — and that alone takes it off the
/// generic listing, out of the vocabulary, and off every non-record invoke path, exactly as the
/// shipped `memory-chat` provider is.
#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_provider_carrying_a_declared_route_is_still_hidden_and_denied() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let storage = StorageHost::open(&root, &key, StorageLimits::default()).expect("storage host");
    let registry = BrokerProviderRegistry::load_with_storage(
        [provider_fixture("storage-probe-provider.wasm")],
        BrokerHostLimits::default(),
        Some(storage),
    )
    .await
    .expect("storage probe fixture loads");
    let world = PolicyWorld::new(
        [
            "caller".parse::<PrincipalId>().expect("caller"),
            "gateway".parse().expect("gateway"),
            "maintainer".parse().expect("maintainer"),
        ],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("policy world");
    // Policy permits it on every path, so what hides it can only be the route.
    let policy = PolicyEngine::new(
        r#"
        permit(principal == Dekopon::Principal::"caller",
               action == Dekopon::Action::"storage-probe.run",
               resource == Dekopon::Provider::"storage-probe")
        unless { context has via };
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context has via && context.via == "gateway" };
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"storage-probe.run",
               resource == Dekopon::Provider::"storage-probe")
        when { context has via && context.via == "gateway"
            && context has agent && context.agent == "reviewer" };
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
            idempotency: Idempotency::Conditional,
            credential: None,
            credential_by_agent: Default::default(),
            constraints: dekopon_capability::ExecutionConstraints {
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
    let caller = AuthenticatedContext::new(
        "caller".parse().expect("caller"),
        Actor::Service {
            principal: "caller".parse().expect("caller"),
        },
    )
    .expect("caller context");
    assert!(broker.capabilities(&caller).is_empty());
    assert!(broker.command_words(&caller).is_empty());
    assert!(
        broker.resolve_command("storageprobe", &[]).await.is_err(),
        "the routed provider's word is reserved even though nothing about it says memory"
    );

    let direct = "renamed-direct"
        .parse::<InvocationId>()
        .expect("invocation");
    let result = broker
        .invoke(
            &caller,
            InvocationRequest {
                id: direct,
                capability: "storage-probe.run".parse().expect("capability"),
                trace: "trace-renamed-direct".parse().expect("trace"),
                trace_parent: None,
                input: json!({}),
                secret_use: None,
            },
        )
        .await
        .expect("reserved denial is audited");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.error.as_deref(), Some("chat-scope-required"));

    let gateway = gateway();
    let grant = grant();
    let claim = claim();
    let (listed, words) = broker
        .capabilities_for(&gateway, Some(&grant), &claim.subject, &claim.agent)
        .expect("legacy attestation is honored");
    assert!(listed.is_empty() && words.is_empty());
    let (listed, words, memory) = broker
        .capabilities_for_chat(&gateway, Some(&grant), &claim)
        .expect("ordinary chat remains available");
    assert!(listed.is_empty() && words.is_empty() && memory.is_none());
    assert!(
        broker
            .resolve_command_for_chat(&gateway, Some(&grant), &claim, "storageprobe", &[])
            .await
            .is_err()
    );

    let chat_id = "renamed-chat".parse::<InvocationId>().expect("invocation");
    let chat_result = broker
        .invoke_for_chat(
            &gateway,
            Some(&grant),
            &ChatAttestation {
                subject: claim.subject.clone(),
                agent: claim.agent.clone(),
                scope: claim.scope.clone(),
                invocation: chat_id.clone(),
            },
            InvocationRequest {
                id: chat_id,
                capability: "storage-probe.run".parse().expect("capability"),
                trace: "trace-renamed-chat".parse().expect("trace"),
                trace_parent: None,
                input: json!({}),
                secret_use: None,
            },
        )
        .await
        .expect("chat reserved denial is audited");
    assert_eq!(
        chat_result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(
        chat_result.error.as_deref(),
        Some("record-operation-required"),
        "the record route is unreachable from the generic chat invoke path"
    );

    let id = "renamed-attested"
        .parse::<InvocationId>()
        .expect("invocation");
    let result = broker
        .invoke_for(
            &gateway,
            Some(&grant),
            &SubjectAttestation {
                subject: claim.subject,
                agent: claim.agent,
                invocation: id.clone(),
            },
            InvocationRequest {
                id,
                capability: "storage-probe.run".parse().expect("capability"),
                trace: "trace-renamed-attested".parse().expect("trace"),
                trace_parent: None,
                input: json!({}),
                secret_use: None,
            },
        )
        .await
        .expect("attested reserved denial is audited");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(result.error.as_deref(), Some("chat-scope-required"));
}

fn memory_constraint(
    route: CapabilityRoute,
    effect: EffectKind,
    risk: RiskLevel,
    idempotency: Idempotency,
    access: StorageAccess,
) -> ConstraintSet {
    ConstraintSet {
        route,
        provider: "memory-chat".parse().expect("provider"),
        effect,
        risk,
        idempotency,
        credential: None,
        credential_by_agent: Default::default(),
        constraints: dekopon_capability::ExecutionConstraints {
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
        idempotency: Idempotency::Idempotent,
        credential: None,
        credential_by_agent: Default::default(),
        constraints: dekopon_capability::ExecutionConstraints::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn records_after_typed_acceptance_and_retrieves_after_restart() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let audit = Arc::new(InMemoryAuditLog::new(32).expect("audit"));
    let broker = build_broker(&root, &key, Arc::clone(&audit)).await;
    let claim = claim();
    let grant = grant();
    let (capabilities, words, memory) = broker
        .capabilities_for_chat(&gateway(), Some(&grant), &claim)
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
        broker.resolve_command("memory", &[]).await.is_err(),
        "legacy command resolution never enters the memory provider"
    );
    // A routed capability on the two legacy paths: reserved, denied, and audited without the
    // identity every non-storage record carries.
    for (index, attested) in [false, true].into_iter().enumerate() {
        let id = format!("reserved-route-{index}")
            .parse::<InvocationId>()
            .expect("invocation");
        let request = InvocationRequest {
            id: id.clone(),
            capability: MEMORY_RECENT.parse().expect("capability"),
            trace: format!("trace-reserved-route-{index}")
                .parse()
                .expect("trace"),
            trace_parent: None,
            input: json!({}),
            secret_use: None,
        };
        let result = if attested {
            broker
                .invoke_for(
                    &gateway(),
                    Some(&grant),
                    &SubjectAttestation {
                        subject: claim.subject.clone(),
                        agent: claim.agent.clone(),
                        invocation: id,
                    },
                    request,
                )
                .await
        } else {
            broker.invoke(&gateway(), request).await
        }
        .expect("reserved route denial is audited");
        assert_eq!(
            result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
    }

    let mut swaps = Vec::new();
    let mut swapped = claim.clone();
    swapped.scope.channel = "c999999".to_owned();
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped.scope.conversation = "c0123abc:1712345678.999999".to_owned();
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped.scope.transport = "other-slack".parse().expect("transport");
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped.scope.kind = ChatTransportKind::Discord;
    swaps.push(swapped);
    let mut swapped = claim.clone();
    swapped.agent = "other-agent".parse().expect("agent");
    swaps.push(swapped);
    for swapped in swaps {
        assert!(
            broker
                .capabilities_for_chat(&gateway(), Some(&grant), &swapped)
                .is_none(),
            "every independently swapped scope field denies"
        );
    }
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
        .invoke_for_chat(
            &gateway(),
            Some(&grant),
            &ChatAttestation {
                subject: claim.subject.clone(),
                agent: claim.agent.clone(),
                scope: claim.scope.clone(),
                invocation: generic_id.clone(),
            },
            InvocationRequest {
                id: generic_id,
                capability: "memory.chat.record".parse().expect("capability"),
                trace: "trace-generic-record".parse().expect("trace"),
                trace_parent: None,
                input: json!({}),
                secret_use: None,
            },
        )
        .await
        .expect("generic record denial is accounted");
    assert_eq!(
        generic.outcome,
        dekopon_capability::InvocationOutcome::Denied
    );
    assert_eq!(generic.error.as_deref(), Some("record-operation-required"));

    let record_id = "record-1".parse::<InvocationId>().expect("invocation");
    let record = broker
        .record_delivered_turn_for_chat(
            &gateway(),
            Some(&grant),
            &ChatAttestation {
                subject: claim.subject.clone(),
                agent: claim.agent.clone(),
                scope: claim.scope.clone(),
                invocation: record_id.clone(),
            },
            DeliveredTurnRequest {
                id: record_id,
                trace: "trace-memory".parse().expect("trace"),
                trace_parent: None,
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
            .all(|digest| digest.starts_with("hmac-sha256:"))
    );
    drop(broker);

    let audit_after_restart = Arc::new(InMemoryAuditLog::new(32).expect("audit"));
    let broker = build_broker(&root, &key, audit_after_restart).await;
    let recent_id = "recent-1".parse::<InvocationId>().expect("invocation");
    let recent = broker
        .invoke_for_chat(
            &gateway(),
            Some(&grant),
            &ChatAttestation {
                subject: claim.subject.clone(),
                agent: claim.agent.clone(),
                scope: claim.scope.clone(),
                invocation: recent_id.clone(),
            },
            InvocationRequest {
                id: recent_id,
                capability: "memory.chat.recent".parse().expect("capability"),
                trace: "trace-recent".parse().expect("trace"),
                trace_parent: None,
                input: json!({"last": 1}),
                secret_use: None,
            },
        )
        .await
        .expect("recent accounted");
    assert_eq!(
        recent.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(
        recent
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
    let records = audit.records().await;
    assert_eq!(records.len(), 5);
    for record in records {
        match record.event {
            AuditEvent::Decision {
                invocation,
                principal,
                actor,
                via,
                attested_subject,
                provider,
                authorized_by,
                policy_revision,
                policy_ids,
                policy_digest,
                storage_scope_commitment,
                ..
            } => {
                assert!(
                    principal.is_none()
                        && actor.is_none()
                        && via.is_none()
                        && attested_subject.is_none()
                );
                assert!(provider.is_none() && authorized_by.is_none() && policy_revision.is_none());
                assert!(policy_ids.is_empty() && policy_digest.is_none());
                if invocation.as_str() == "record-1" {
                    let scope = storage_scope_commitment.expect("scope commitment");
                    assert_ne!(
                        scope.as_str().trim_start_matches("hmac-sha256:"),
                        physical_base
                    );
                }
            }
            AuditEvent::Execution {
                principal,
                actor,
                via,
                attested_subject,
                provider,
                authorized_by,
                policy_revision,
                policy_ids,
                policy_digest,
                credential,
                storage_scope_commitment,
                storage,
                ..
            } => {
                assert!(
                    principal.is_none()
                        && actor.is_none()
                        && via.is_none()
                        && attested_subject.is_none()
                );
                assert!(provider.is_none() && authorized_by.is_none() && policy_revision.is_none());
                assert!(policy_ids.is_empty() && policy_digest.is_none() && credential.is_none());
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
async fn generated_wasm_compacts_at_default_threshold_exactly_and_plus_one() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let mut config = memory_config();
    config.continuity_policy = ContinuityPolicy::Stable;
    let cases = [
        (
            "c0123abc:1712345678.000410",
            "default exact user",
            "default exact assistant",
            0_u64,
        ),
        (
            "c0123abc:1712345678.000420",
            "default plus one user",
            "default plus one assistant",
            1_u64,
        ),
    ];

    // Seeding two 12 MiB pre-compaction fixtures is test setup, not the generated-provider path
    // under test. Give those direct native commits enough time to survive parallel test I/O; the
    // broker below is deliberately rebuilt with the unchanged default 5-second finalization
    // budget, default Wasm memory, and default fuel for both real compactions.
    let seed_limits = StorageLimits {
        finalization_budget_ms: 60_000,
        ..StorageLimits::default()
    };
    let storage = StorageHost::open(&root, &key, seed_limits).expect("storage host");
    for (index, (conversation, user, assistant, delta)) in cases.iter().enumerate() {
        let appended = canonical_turn_line_bytes(user, assistant);
        let seed_size = config
            .compaction_threshold_bytes
            .checked_sub(appended)
            .and_then(|size| size.checked_add(*delta))
            .expect("default threshold has line headroom");
        let seed = seed_turn_file(seed_size, config.max_turn_bytes);
        assert_eq!(seed.len() as u64, seed_size);
        let grant = storage
            .grant(StorageGrantRequest::new(
                format!("default-scale-seed-{index}")
                    .parse()
                    .expect("invocation"),
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
                *conversation,
                ContinuityPolicy::Stable,
                b"default-scale-seed-authority".to_vec(),
            ))
            .expect("seed grant");
        let mut transaction = storage.begin(grant).expect("seed transaction");
        transaction
            .jsonl_replace("turns.jsonl", 0, &seed)
            .expect("default-scale seed fits one coherent replacement call");
        transaction.commit().expect("seed commit");
    }
    drop(storage);

    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(32).expect("audit")),
        config.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;
    let attestor = grant_for(&[cases[0].0, cases[1].0]);
    for (index, (conversation, user, assistant, delta)) in cases.iter().enumerate() {
        let session = claim_for(conversation);
        let result = record_turn_in(
            &broker,
            &session,
            &attestor,
            &format!("default-scale-record-{index}"),
            &format!("1712345678.{:06}", 500 + index),
            user,
            assistant,
        )
        .await;
        assert_eq!(
            result.outcome,
            dekopon_capability::InvocationOutcome::Succeeded,
            "default threshold +{delta} failed under default 64 MiB memory and fuel: {result:?}"
        );
        let recent = query_memory_in(
            &broker,
            &session,
            &attestor,
            &format!("default-scale-recent-{index}"),
            MEMORY_RECENT,
            json!({"last": 1}),
        )
        .await;
        assert_eq!(recent["turns"][0]["user"], *user);
    }

    let replacements = walk(&root)
        .into_iter()
        .filter(|path| path.parent().is_some_and(|parent| parent.ends_with("data")))
        .filter_map(|path| fs::metadata(path).ok().map(|metadata| metadata.len()))
        .filter(|length| *length > 1024 * 1024)
        .collect::<Vec<_>>();
    assert_eq!(
        replacements.len(),
        2,
        "both default-scale turn files compacted"
    );
    assert!(
        replacements
            .iter()
            .all(|length| *length <= config.compaction_target_bytes),
        "default 8 MiB replacement target was exceeded: {replacements:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dedup_conflict_capacity_search_and_compaction_preserve_bounded_reads() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let mut config = memory_config();
    config.max_lookback_turns = 2;
    config.max_recent_turns = 2;
    config.max_search_results = 2;
    config.max_turn_bytes = 2_048;
    config.max_dedup_records = 4;
    config.compaction_target_bytes = 4_096;
    config.compaction_threshold_bytes = 5_000;
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(64).expect("audit")),
        config,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;

    let first_user = format!("Alpha {}", "a".repeat(600));
    let first_assistant = format!("First {}", "b".repeat(600));
    assert_eq!(
        record_turn(
            &broker,
            "record-a",
            "1712345678.000101",
            &first_user,
            &first_assistant,
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let duplicate = record_turn(
        &broker,
        "record-a-duplicate",
        "1712345678.000101",
        &first_user,
        &first_assistant,
    )
    .await;
    assert_eq!(
        duplicate.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    assert_eq!(
        duplicate
            .output
            .as_ref()
            .and_then(|value| value["duplicate"].as_bool()),
        Some(true)
    );
    let conflict = record_turn(
        &broker,
        "record-a-conflict",
        "1712345678.000101",
        &first_user,
        "changed answer",
    )
    .await;
    assert_eq!(
        conflict.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(conflict.error.as_deref(), Some("dedup-conflict"));

    for (index, (label, timestamp)) in [
        ("Beta", "1712345678.000102"),
        ("Gamma", "1712345678.000103"),
        ("Delta", "1712345678.000104"),
    ]
    .into_iter()
    .enumerate()
    {
        let user = format!("{label} {}", "u".repeat(600));
        let assistant = format!("answer {label} {}", "v".repeat(600));
        assert_eq!(
            record_turn(
                &broker,
                &format!("record-{}", index + 2),
                timestamp,
                &user,
                &assistant,
            )
            .await
            .outcome,
            dekopon_capability::InvocationOutcome::Succeeded
        );
    }
    let capacity = record_turn(
        &broker,
        "record-capacity",
        "1712345678.000105",
        "Epsilon",
        "capacity",
    )
    .await;
    assert_eq!(
        capacity.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(capacity.error.as_deref(), Some("dedup-capacity"));

    let recent = query_memory(
        &broker,
        "recent-bounded",
        "memory.chat.recent",
        json!({"last": 2}),
    )
    .await;
    let users = recent["turns"]
        .as_array()
        .expect("turns")
        .iter()
        .filter_map(|turn| turn["user"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 2);
    assert!(users[0].starts_with("Gamma") && users[1].starts_with("Delta"));

    let search = query_memory(
        &broker,
        "search-casefold",
        "memory.chat.search",
        json!({"query": "DELTA"}),
    )
    .await;
    assert_eq!(search["turns"].as_array().expect("turns").len(), 1);
    let old = query_memory(
        &broker,
        "search-compacted",
        "memory.chat.search",
        json!({"query": "Alpha"}),
    )
    .await;
    assert!(old["turns"].as_array().expect("turns").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn newest_result_bounds_and_complete_record_corruption_are_publicly_classified() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let mut config = memory_config();
    config.max_turn_bytes = 2_048;
    config.max_result_bytes = 512;
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        config,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;

    assert_eq!(
        record_turn(
            &broker,
            "bounded-record",
            "1712345678.000120",
            &"x".repeat(700),
            "answer",
        )
        .await
        .outcome,
        dekopon_capability::InvocationOutcome::Succeeded
    );
    let oversized = query_memory_result(
        &broker,
        "bounded-result",
        "memory.chat.recent",
        json!({"last": 1}),
    )
    .await;
    assert_eq!(
        oversized.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(oversized.error.as_deref(), Some("result-too-large"));

    let turns = walk(&root)
        .into_iter()
        .find(|path| {
            path.is_file()
                && fs::read(path).is_ok_and(|bytes| {
                    bytes
                        .windows(b"dekopon.chat-memory.turn".len())
                        .any(|window| window == b"dekopon.chat-memory.turn")
                })
        })
        .expect("turns file");
    fs::write(turns, b"{\"malformed\":true}\n").expect("corrupt complete record");
    let corrupt = query_memory_result(
        &broker,
        "corrupt-result",
        "memory.chat.recent",
        json!({"last": 1}),
    )
    .await;
    assert_eq!(
        corrupt.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(corrupt.error.as_deref(), Some("memory-corrupt"));
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
        &grant(),
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
    claim: &ChatSessionClaim,
    grant: &AttestorGrant,
    invocation: &str,
    timestamp: &str,
    user: &str,
    assistant: &str,
) -> dekopon_capability::InvocationResult {
    let id = invocation.parse::<InvocationId>().expect("invocation");
    broker
        .record_delivered_turn_for_chat(
            &gateway(),
            Some(grant),
            &ChatAttestation {
                subject: claim.subject.clone(),
                agent: claim.agent.clone(),
                scope: claim.scope.clone(),
                invocation: id.clone(),
            },
            DeliveredTurnRequest {
                id,
                trace: format!("trace-{invocation}").parse().expect("trace"),
                trace_parent: None,
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
    query_memory_result_in(broker, &claim(), &grant(), invocation, capability, input).await
}

async fn query_memory_result_in(
    broker: &Broker<InMemoryAuditLog>,
    claim: &ChatSessionClaim,
    grant: &AttestorGrant,
    invocation: &str,
    capability: &str,
    input: serde_json::Value,
) -> dekopon_capability::InvocationResult {
    let id = invocation.parse::<InvocationId>().expect("invocation");
    broker
        .invoke_for_chat(
            &gateway(),
            Some(grant),
            &ChatAttestation {
                subject: claim.subject.clone(),
                agent: claim.agent.clone(),
                scope: claim.scope.clone(),
                invocation: id.clone(),
            },
            InvocationRequest {
                id,
                capability: capability.parse().expect("capability"),
                trace: format!("trace-{invocation}").parse().expect("trace"),
                trace_parent: None,
                input,
                secret_use: None,
            },
        )
        .await
        .expect("query accounted")
}

async fn query_memory_in(
    broker: &Broker<InMemoryAuditLog>,
    claim: &ChatSessionClaim,
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
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let broker = build_broker(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(32).expect("audit")),
    )
    .await;
    let first = claim_for("c0123abc:1712345678.000100");
    let second = claim_for("c0123abc:1712345678.000200");
    let grant = grant_for(&["c0123abc:1712345678.000100", "c0123abc:1712345678.000200"]);

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
async fn search_reads_complete_records_across_the_fixed_chunk_boundary() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let mut config = memory_config();
    config.max_lookback_turns = 7;
    config.max_recent_turns = 7;
    config.max_search_results = 7;
    config.max_turn_bytes = 60 * 1024;
    config.compaction_target_bytes = 500_000;
    config.compaction_threshold_bytes = 600_000;
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(32).expect("audit")),
        config,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;

    for index in 0..7 {
        let marker = if index == 0 {
            "chunk-boundary-first"
        } else {
            "chunk-boundary-filler"
        };
        let user = format!("{marker}-{index}-{}", "x".repeat(38 * 1024));
        assert_eq!(
            record_turn(
                &broker,
                &format!("chunk-record-{index}"),
                &format!("1712345678.{:06}", 300 + index),
                &user,
                "accepted",
            )
            .await
            .outcome,
            dekopon_capability::InvocationOutcome::Succeeded
        );
    }
    let result = query_memory(
        &broker,
        "chunk-search",
        "memory.chat.search",
        json!({"query": "CHUNK-BOUNDARY-FIRST"}),
    )
    .await;
    let turns = result["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 1);
    assert!(
        turns[0]["user"]
            .as_str()
            .expect("user")
            .starts_with("chunk-boundary-first-0-")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn selected_symbolic_credential_rotates_authority_without_hashing_its_value() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let config = memory_config();
    let broker = build_broker_with_principal(
        &root,
        &key,
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

    // Secret values are deliberately not authority metadata. Replacing one behind the same
    // selected symbolic reference keeps the current generation and its existing text.
    let broker = build_broker_with_principal(
        &root,
        &key,
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
        &key,
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
        &key,
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
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let mut stable = memory_config();
    stable.continuity_policy = ContinuityPolicy::Stable;
    let broker = build_broker_with(
        &root,
        &key,
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
        &key,
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
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    let mut memory = memory_config();
    memory.enabled_agents = vec!["other-agent".parse().expect("agent")];
    let broker = build_broker_with_principal(
        &root,
        &key,
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

    // Cedar still permits all three exact memory IDs, but chatMemory is not enabled for reviewer.
    // Changing an unreachable memory ceiling must not enter this generic provider's authority.
    memory.max_recent_turns -= 1;
    let broker = build_broker_with_principal(
        &root,
        &key,
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
        .invoke_for_chat(
            &gateway(),
            Some(&grant()),
            &ChatAttestation {
                subject: session.subject,
                agent: session.agent,
                scope: session.scope,
                invocation: id.clone(),
            },
            InvocationRequest {
                id,
                capability: "storage-probe.run".parse().expect("capability"),
                trace: format!("trace-{invocation}").parse().expect("trace"),
                trace_parent: None,
                input: json!({"mode": "quota-denial"}),
                secret_use: None,
            },
        )
        .await
        .expect("storage denial is accounted");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Failed
    );
    assert_eq!(result.error.as_deref(), Some("storage-quota"));
}

#[tokio::test(flavor = "multi_thread")]
async fn authority_surface_ignores_order_and_denied_provider_but_rotates_every_semantic_ceiling() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let root = directory.join("provider-storage");
    let key = directory.join("storage-key.yaml");
    fs::write(&key, "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n").expect("key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");

    let mut baseline_memory = memory_config();
    baseline_memory
        .enabled_agents
        .push("other-agent".parse().expect("agent"));
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;
    assert!(
        broker.capability_uses_storage(&"storage-probe.run".parse().expect("capability")),
        "generic durable-files constraints must select identity-free outer spans"
    );
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

    // Principal is an owner-controlled mapping for the canonical subject in the base namespace,
    // not part of the resulting effective authority surface. Equivalent grants under a remap keep
    // continuity exactly as provider order, policy formatting, and unrelated denied providers do.
    let broker = build_broker_with_principal(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
        "maintainer-v2",
        None,
        false,
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
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        reordered,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        true,
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
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        host_limits,
        false,
    )
    .await;
    assert_recent_empty(&broker, "surface-host").await;
    drop(broker);
    assert_eq!(generation_count(&root), 2);

    // Returning B -> A still mints a third generation; an old authority generation is never
    // reopened merely because its canonical bytes recur.
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory.clone(),
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;
    assert_recent_empty(&broker, "surface-a-again").await;
    drop(broker);
    assert_eq!(generation_count(&root), 3);

    let mut memory_limit = baseline_memory.clone();
    memory_limit.max_recent_turns -= 1;
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        memory_limit,
        StorageLimits::default(),
        BrokerHostLimits::default(),
        false,
    )
    .await;
    assert_recent_empty(&broker, "surface-memory-limit").await;
    drop(broker);
    assert_eq!(generation_count(&root), 4);

    let storage_limit = StorageLimits {
        max_open_handles: StorageLimits::default().max_open_handles - 1,
        ..StorageLimits::default()
    };
    let broker = build_broker_with(
        &root,
        &key,
        Arc::new(InMemoryAuditLog::new(16).expect("audit")),
        baseline_memory,
        storage_limit,
        BrokerHostLimits::default(),
        false,
    )
    .await;
    assert_recent_empty(&broker, "surface-storage-limit").await;
    drop(broker);
    assert_eq!(generation_count(&root), 5);
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
    let commitment = format!("hmac-sha256:{}", "0".repeat(64));
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
    let commitment = format!("hmac-sha256:{}", "0".repeat(64));
    let minimum_lines = (0..RECORDS)
        .map(|index| {
            serde_json::to_vec(&SeedTurn {
                format: "dekopon.chat-memory.turn",
                version: 1,
                id: format!("hmac-sha256:{index:064x}"),
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
            id: format!("hmac-sha256:{index:064x}"),
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

/// Route mistakes are reported together, because a route file is edited as a whole.
#[tokio::test(flavor = "multi_thread")]
async fn every_declared_route_conflict_is_reported_at_startup() {
    let registry =
        BrokerProviderRegistry::load([provider_fixture("echo-provider.wasm")], BrokerHostLimits::default())
            .await
            .expect("echo fixture loads");
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
        idempotency: Idempotency::Idempotent,
        credential: None,
        credential_by_agent: Default::default(),
        constraints: dekopon_capability::ExecutionConstraints {
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
            "echo.echo".parse().expect("capability"),
            routed(CapabilityRoute::ChatMemoryRecent, "echo", read_only.clone()),
        ),
        (
            "echo.second".parse().expect("capability"),
            routed(CapabilityRoute::ChatMemoryRecent, "echo", read_only),
        ),
        (
            "echo.third".parse().expect("capability"),
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
                    "echo.echo".parse().expect("capability"),
                    "echo.second".parse().expect("capability"),
                ],
            },
            RouteConflict::SplitProvider {
                providers: vec![
                    "echo".parse().expect("provider"),
                    "elsewhere".parse().expect("provider"),
                ],
            },
            RouteConflict::MissingChatStorage {
                capability: "echo.third".parse().expect("capability"),
                route: CapabilityRoute::ChatMemorySearch,
                access: StorageAccess::ReadOnly,
            },
        ],
        "one run must report every route mistake: {rendered}"
    );
    for fragment in ["echo.second", "elsewhere", "echo.third"] {
        assert!(
            rendered.contains(fragment),
            "the message names {fragment}: {rendered}"
        );
    }
}
