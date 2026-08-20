#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use dekopon_broker::{
    AttestorGrant, AuditEvent, Broker, BrokerLimits, ChatAttestation, ChatMemoryConfig,
    ChatScopeGrant, ChatSessionClaim, ChatTransportKind, ConstraintCatalog, ConstraintSet,
    CredentialStore, DeliveredTurnRequest, DeliveryIdentity, IdentityDirectory, InMemoryAuditLog,
    PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{ChatScopeClaim, InvocationRequest};
use dekopon_capability::{
    EffectKind, Idempotency, StorageAccess, StorageConstraints, StorageInterface, StorageNamespace,
};
use dekopon_core::{
    Actor, AgentId, ExternalSubject, InvocationId, PrincipalId, RiskLevel, TransportId,
};
use dekopon_storage_host::{ContinuityPolicy, StorageHost, StorageLimits};
use serde_json::json;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/providers")
        .join(name)
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
        max_dedup_records: 16_000,
        max_dedup_bytes: 4_194_304,
        compaction_target_bytes: 8_388_608,
        compaction_threshold_bytes: 12_582_912,
    }
}

fn constraints() -> ConstraintCatalog {
    let entries = [
        (
            "memory.chat.record",
            EffectKind::LocalWrite,
            RiskLevel::Medium,
            Idempotency::Conditional,
            StorageAccess::ReadWrite,
        ),
        (
            "memory.chat.recent",
            EffectKind::ReadOnly,
            RiskLevel::High,
            Idempotency::Idempotent,
            StorageAccess::ReadOnly,
        ),
        (
            "memory.chat.search",
            EffectKind::ReadOnly,
            RiskLevel::High,
            Idempotency::Idempotent,
            StorageAccess::ReadOnly,
        ),
    ]
    .into_iter()
    .map(|(id, effect, risk, idempotency, access)| {
        let capability = id.parse().expect("capability");
        (
            capability,
            ConstraintSet {
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
                },
            },
        )
    });
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
    let storage = StorageHost::open(root, key, storage_limits).expect("storage host");
    let mut providers = vec![
        fixture("memory-chat-provider.wasm"),
        fixture("echo-provider.wasm"),
    ];
    if reverse_provider_order {
        providers.reverse();
    }
    let registry = BrokerProviderRegistry::load_with_storage(providers, host_limits, Some(storage))
        .await
        .expect("memory provider loads");
    let world = PolicyWorld::new(
        [
            "gateway".parse::<PrincipalId>().expect("gateway"),
            "maintainer".parse().expect("maintainer"),
        ],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("world");
    let policy = PolicyEngine::new(
        r#"
        @id("prompt")
        permit(principal == Dekopon::Principal::"maintainer",
               action == Dekopon::Action::"agent.prompt",
               resource == Dekopon::Agent::"reviewer")
        when { context has via && context.via == "gateway"
            && context has transportKind && context.transportKind == "slack"
            && context has transport && context.transport == "scientist-slack"
            && context has channel && context.channel == "c0123abc"
            && context has conversation && context.conversation == "c0123abc:1712345678.000100" };

        @id("memory")
        permit(principal == Dekopon::Principal::"maintainer",
               action in [Dekopon::Action::"memory.chat.record",
                          Dekopon::Action::"memory.chat.recent",
                          Dekopon::Action::"memory.chat.search"],
               resource == Dekopon::Provider::"memory-chat")
        when { context has via && context.via == "gateway"
            && context has agent && context.agent == "reviewer"
            && context has transportKind && context.transportKind == "slack"
            && context has transport && context.transport == "scientist-slack"
            && context has channel && context.channel == "c0123abc"
            && context has conversation && context.conversation == "c0123abc:1712345678.000100" };
        "#,
        &world,
    )
    .expect("policy");
    Broker::new(
        registry,
        "broker".parse().expect("broker"),
        "memory-policy".to_owned(),
        policy,
        constraints(),
        CredentialStore::empty(),
        IdentityDirectory::new([(
            "slack.t0123abc.u9xyz".parse().expect("subject"),
            "maintainer".parse().expect("principal"),
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
    ChatSessionClaim {
        subject: "slack.t0123abc.u9xyz"
            .parse::<ExternalSubject>()
            .expect("subject"),
        agent: "reviewer".parse::<AgentId>().expect("agent"),
        scope: ChatScopeClaim {
            transport: "scientist-slack".parse::<TransportId>().expect("transport"),
            kind: ChatTransportKind::Slack,
            channel: "c0123abc".to_owned(),
            conversation: "c0123abc:1712345678.000100".to_owned(),
        },
    }
}

fn grant() -> AttestorGrant {
    AttestorGrant {
        namespaces: vec!["slack.t0123abc".to_owned()],
        chat_scopes: vec![ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Slack,
            transport: "scientist-slack".parse().expect("transport"),
            channel: "c0123abc".to_owned(),
            conversation: "c0123abc:1712345678.000100".to_owned(),
            local_subject_service: None,
        }],
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
    assert_eq!(records.len(), 3);
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
    let claim = claim();
    let id = invocation.parse::<InvocationId>().expect("invocation");
    broker
        .record_delivered_turn_for_chat(
            &gateway(),
            Some(&grant()),
            &ChatAttestation {
                subject: claim.subject,
                agent: claim.agent,
                scope: claim.scope,
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
        dekopon_capability::InvocationOutcome::Succeeded
    );
    result.output.expect("query output")
}

async fn query_memory_result(
    broker: &Broker<InMemoryAuditLog>,
    invocation: &str,
    capability: &str,
    input: serde_json::Value,
) -> dekopon_capability::InvocationResult {
    let claim = claim();
    let id = invocation.parse::<InvocationId>().expect("invocation");
    broker
        .invoke_for_chat(
            &gateway(),
            Some(&grant()),
            &ChatAttestation {
                subject: claim.subject,
                agent: claim.agent,
                scope: claim.scope,
                invocation: id.clone(),
            },
            InvocationRequest {
                id,
                capability: capability.parse().expect("capability"),
                trace: format!("trace-{invocation}").parse().expect("trace"),
                trace_parent: None,
                input,
            },
        )
        .await
        .expect("query accounted")
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
    touch_recent(&broker, "surface-a").await;
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
    touch_recent(&broker, "surface-order-only").await;
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
    touch_recent(&broker, "surface-host").await;
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
    touch_recent(&broker, "surface-a-again").await;
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
    touch_recent(&broker, "surface-memory-limit").await;
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
    touch_recent(&broker, "surface-storage-limit").await;
    drop(broker);
    assert_eq!(generation_count(&root), 5);
}

async fn touch_recent(broker: &Broker<InMemoryAuditLog>, invocation: &str) {
    let claim = claim();
    let id = invocation.parse::<InvocationId>().expect("invocation");
    let result = broker
        .invoke_for_chat(
            &gateway(),
            Some(&grant()),
            &ChatAttestation {
                subject: claim.subject,
                agent: claim.agent,
                scope: claim.scope,
                invocation: id.clone(),
            },
            InvocationRequest {
                id,
                capability: "memory.chat.recent".parse().expect("capability"),
                trace: format!("trace-{invocation}").parse().expect("trace"),
                trace_parent: None,
                input: json!({"last": 1}),
            },
        )
        .await
        .expect("recent invocation accounted");
    assert_eq!(
        result.outcome,
        dekopon_capability::InvocationOutcome::Succeeded
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

fn walk(path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![path.to_path_buf()];
    if path.is_dir() {
        for entry in fs::read_dir(path).expect("read tree") {
            paths.extend(walk(&entry.expect("entry").path()));
        }
    }
    paths
}
