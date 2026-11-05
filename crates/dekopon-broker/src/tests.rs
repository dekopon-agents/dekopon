use std::{collections::BTreeMap, sync::Arc};

use dekopon_broker_host::BrokerHostError;
use dekopon_broker_host::BrokerHostLimits;
use dekopon_capability::{
    ChatSlot, ExecutionConstraints, HttpConstraints, RequestTemplate, StorageAccess,
    StorageConstraints, StorageInterface, StorageScope,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, MAX_FAILURE_MESSAGE_BYTES, PrincipalId,
    ProviderFailureDetail, ProviderId, SecretDrn, SecretSinkKind, SecretUseProposal, TraceId,
};

use super::{
    AuditConfigurationError, AuditError, AuditEvent, AuditLog, AuthenticatedContext,
    AuthorityEncoder, Broker, BrokerLimits, CapabilityRoute, ChatMemoryConfig, ConstraintCatalog,
    ConstraintSet, ContextError, IdentityDirectory, InMemoryAuditLog, PolicyEngine, PolicyWorld,
    encode_capability_authority, encode_execution_constraints, encode_host_limits,
    encode_memory_config, encode_storage_limits, provider_exit_status, provider_failure_detail,
    public_host_error, render_chat_slots,
};

#[tokio::test]
async fn desired_pin_without_a_loaded_artifact_is_not_an_execution_digest() {
    use dekopon_broker_host::{BrokerProviderRegistry, LockedProviderSource};
    use dekopon_capability::{EffectKind, InvocationOutcome};
    use dekopon_core::RiskLevel;
    use dekopon_test_support::{CaptureLayer, provider_fixture};
    use sha2::{Digest, Sha256};
    use tracing_subscriber::prelude::*;

    let fixture = provider_fixture("cli-probe-provider.wasm");
    let bytes = std::fs::read(&fixture).expect("real component bytes");
    let pin = LockedProviderSource::new(
        fixture,
        bytes.len() as u64,
        Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "cli-probe".parse().expect("provider"),
    )
    .expect("valid desired pin for the real component");
    let registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("a different real component loads");
    let artifact = registry
        .loaded_provider_metadata()
        .find(|loaded| loaded.manifest.id == *pin.provider_id())
        .map(|loaded| loaded.artifact_sha256);
    assert!(
        artifact.is_none(),
        "a desired pin does not load an artifact"
    );

    let captured = CaptureLayer::workspace();
    let _subscriber = tracing_subscriber::registry()
        .with(captured.clone())
        .set_default();
    let broker: PrincipalId = "broker".parse().expect("principal");
    let context = AuthenticatedContext::new(
        broker.clone(),
        Actor::Service {
            principal: broker.clone(),
        },
    )
    .expect("authenticated service");
    let capability = "cli-probe.upper".parse().expect("capability");
    let world = PolicyWorld::new([broker.clone()], []).expect("world");
    let policy = PolicyEngine::new("", &world).expect("policy");
    let set = ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: pin.provider_id().clone(),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        constraints: ExecutionConstraints::default(),
    };
    super::emit_audit_event(&super::execution_event(
        &context,
        &"invoke-unloaded".parse().expect("invocation"),
        "0000000000000000000000000000f1c7".parse().expect("trace"),
        &capability,
        "decision-unloaded",
        &[],
        policy.digest(),
        artifact.as_deref(),
        &broker,
        &set,
        InvocationOutcome::Failed,
        None,
        0,
        None,
        None,
        Vec::new(),
        None,
        None,
    ));
    let records = captured.events_where(
        "dekopon_broker::audit",
        &[
            ("audit.event", "broker.execution"),
            ("invocation.id", "invoke-unloaded"),
        ],
    );
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].field("provider.artifact_sha256"),
        None,
        "a desired pin {} is not serving evidence",
        pin.artifact_sha256()
    );
}

#[tokio::test]
async fn smoke_digest_is_boot_via_subject_and_agent_bound() {
    async fn broker() -> Broker<InMemoryAuditLog> {
        let registry = dekopon_broker_host::BrokerProviderRegistry::load(
            [dekopon_test_support::provider_fixture(
                "cli-probe-provider.wasm",
            )],
            BrokerHostLimits::default(),
        )
        .await
        .expect("fixture");
        let world = PolicyWorld::new(
            ["peer".parse().expect("principal")],
            registry
                .capabilities()
                .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
        )
        .expect("world");
        Broker::new(
            registry,
            "broker".parse().expect("principal"),
            PolicyEngine::new("", &world).expect("policy"),
            ConstraintCatalog::new([]).expect("constraints"),
            IdentityDirectory::empty(),
            Arc::new(InMemoryAuditLog::new(4).expect("audit")),
            BrokerLimits::default(),
        )
        .expect("broker")
    }
    let first = broker().await;
    let second = broker().await;
    let via = "peer".parse().expect("via");
    let subject = "drn:slack:user:t0123abc:u9xyz".parse().expect("subject");
    let agent = "reviewer".parse().expect("agent");
    let digest = first.console_smoke_conversation(&via, &subject, &agent);
    assert!(digest.starts_with("smoke-") && digest.len() == 70);
    assert_eq!(
        digest,
        first.console_smoke_conversation(&via, &subject, &agent)
    );
    assert_ne!(
        digest,
        second.console_smoke_conversation(&via, &subject, &agent)
    );
    assert_ne!(
        digest,
        first.console_smoke_conversation(&"other".parse().expect("via"), &subject, &agent)
    );
    assert_ne!(
        digest,
        first.console_smoke_conversation(
            &via,
            &"drn:slack:user:t0123abc:uother".parse().expect("subject"),
            &agent,
        )
    );
    assert_ne!(
        digest,
        first.console_smoke_conversation(&via, &subject, &"other".parse().expect("agent"))
    );
}

fn decision(invocation: &str, allowed: bool) -> AuditEvent {
    AuditEvent::Decision {
        authority: Box::new(super::DecisionAuthority {
            github_denial: None,
            principal_kind: dekopon_core::PrincipalKind::Service,
            started_by: None,
            chat: None,
            space: None,
        }),
        invocation: invocation
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        trace: "0000000000000000000000000000f1c7"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        principal: Some(
            "caller"
                .parse::<PrincipalId>()
                .expect("valid principal fixture"),
        ),
        actor: Some(Actor::Agent {
            agent: "reviewer".parse::<AgentId>().expect("valid agent fixture"),
        }),
        via: None,
        attested_subject: None,
        capability: "cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        secret: None,
        secret_sink: None,
        provider: None,
        authorized_by: Some(
            "broker"
                .parse::<PrincipalId>()
                .expect("valid principal fixture"),
        ),
        decision_id: format!("decision-{invocation}"),

        policy_ids: Vec::new(),
        policy_error_ids: Vec::new(),
        policy_errors_present: None,
        decision_point: "invocation".to_owned(),
        policy_digest: None,
        allowed,
        reason: (!allowed).then(|| "policy-denied".to_owned()),
        decision_digest: format!("sha256:{}", "a".repeat(64)),
        storage_scope_commitment: None,
        storage: None,
    }
}

#[test]
fn console_scope_refusal_audits_a_token_and_sends_only_the_wire_sentence() {
    let refusal = super::console_real_scope_refusal();
    assert_eq!(refusal.reason, "console-real-scope-denied");
    assert_eq!(refusal.wire, super::CONSOLE_REAL_SCOPE_REFUSAL);
    assert!(refusal.wire.starts_with("dekopon sandbox:"));
}

#[test]
fn the_declared_route_is_what_reserves_a_capability_not_its_spelling() {
    let authored = serde_json::json!({
        "provider": "memory-chat",
        "effect": "read-only",
        "risk": "Low",
        "constraints": {"timeoutMs": 1000},
    });
    let generic: ConstraintSet =
        serde_json::from_value(authored.clone()).expect("route may be omitted");
    assert_eq!(generic.route, CapabilityRoute::Generic);
    assert!(generic.route.is_generic() && !generic.route.is_chat_memory());
    assert!(
        !serde_json::to_value(&generic)
            .expect("serialize")
            .as_object()
            .expect("object")
            .contains_key("route"),
        "the default route stays out of serialized configuration"
    );

    let mut declared = authored;
    declared["route"] = serde_json::json!("chatMemoryRecord");
    let routed: ConstraintSet =
        serde_json::from_value(declared).expect("route parses from its camelCase name");
    assert_eq!(routed.route, CapabilityRoute::ChatMemoryRecord);
    assert!(routed.route.is_chat_memory() && !routed.route.is_chat_memory_retrieval());
    assert_eq!(
        serde_json::to_value(&routed).expect("serialize")["route"],
        serde_json::json!("chatMemoryRecord")
    );

    for route in CapabilityRoute::CHAT_MEMORY {
        assert!(route.is_chat_memory());
        assert_eq!(route.to_string(), route.as_str());
    }
    assert!(CapabilityRoute::ChatMemoryRecent.is_chat_memory_retrieval());
    assert!(CapabilityRoute::ChatMemorySearch.is_chat_memory_retrieval());
    assert_eq!(CapabilityRoute::default(), CapabilityRoute::Generic);
}

#[test]
fn memory_composition_reserves_host_calls_and_pre_compaction_peak() {
    let memory = ChatMemoryConfig {
        continuity_policy: dekopon_storage_host::ContinuityPolicy::Paranoid,
        enabled_agents: vec!["reviewer".parse().expect("agent")],
        max_lookback_turns: 200,
        max_recent_turns: 20,
        max_search_results: 20,
        max_query_bytes: 256,
        max_result_bytes: 65_536,
        max_turn_bytes: 32_768,
        compaction_target_bytes: 8_388_608,
        compaction_threshold_bytes: 12_582_912,
        continuity_migration: None,
    };

    let mut minimal = memory.clone();
    minimal.max_lookback_turns = 1;
    minimal.max_recent_turns = 1;
    minimal.max_search_results = 1;
    minimal.max_turn_bytes = 241;
    minimal.compaction_target_bytes = 241;
    minimal.compaction_threshold_bytes = 242;
    let too_small_call = dekopon_storage_host::StorageLimits {
        max_write_bytes_per_call: 240,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(minimal.validate(&too_small_call).is_err());

    let unaligned_read_budget = dekopon_storage_host::StorageLimits {
        max_read_bytes_per_invocation: 262_143,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(
        minimal.validate(&unaligned_read_budget).is_err(),
        "a partial final read is still charged its full 256 KiB chunk"
    );

    let too_small_namespace = dekopon_storage_host::StorageLimits {
        max_namespace_bytes: 12 * 1024 * 1024,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&too_small_namespace).is_err());
    let formerly_staged_copy_rejection = dekopon_storage_host::StorageLimits {
        max_namespace_bytes: 30 * 1024 * 1024,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&formerly_staged_copy_rejection).is_ok());

    type Limit = fn(&mut dekopon_storage_host::StorageLimits) -> &mut u64;
    let boundaries: &[(&ChatMemoryConfig, Limit, u64)] = &[
        (
            &memory,
            |v| &mut v.max_write_bytes_per_call,
            memory.compaction_target_bytes,
        ),
        (
            &memory,
            |v| &mut v.max_file_bytes,
            memory.compaction_threshold_bytes + memory.max_turn_bytes,
        ),
        (
            &memory,
            |v| &mut v.max_namespace_bytes,
            memory.compaction_threshold_bytes + memory.max_turn_bytes + 32 * 4_096,
        ),
        (&minimal, |v| &mut v.max_host_calls_per_invocation, 4),
        (&minimal, |v| &mut v.max_files_per_namespace, 1),
    ];
    for (config, field, exact) in boundaries {
        let mut limits = dekopon_storage_host::StorageLimits::default();
        *field(&mut limits) = *exact;
        assert!(config.validate(&limits).is_ok());
        *field(&mut limits) = exact - 1;
        assert!(config.validate(&limits).is_err());
    }

    let mut result_too_small_for_empty_history = minimal;
    result_too_small_for_empty_history.max_result_bytes = 29;
    assert!(
        result_too_small_for_empty_history
            .validate(&dekopon_storage_host::StorageLimits::default())
            .is_err()
    );

    assert_eq!(
        memory
            .maximum_provider_input_bytes()
            .expect("default input arithmetic"),
        memory.max_turn_bytes + 4 * 1024
    );
    let mut query_dominates = memory.clone();
    query_dominates.max_query_bytes = 10_000;
    assert_eq!(
        query_dominates
            .maximum_provider_input_bytes()
            .expect("query input arithmetic"),
        10_000 * 6 + 4 * 1024
    );
    assert_eq!(
        memory
            .maximum_provider_working_set_bytes()
            .expect("working-set arithmetic"),
        memory.compaction_threshold_bytes * 2
            + memory.compaction_target_bytes * 2
            + memory.max_turn_bytes
            + memory.max_result_bytes
            + 4 * 1024 * 1024
    );

    let minimum_fuel = memory
        .minimum_provider_fuel()
        .expect("default memory fuel arithmetic");
    assert!(minimum_fuel <= BrokerHostLimits::default().fuel);
    assert_eq!(
        minimum_fuel,
        (memory.compaction_threshold_bytes
            + memory.compaction_target_bytes
            + memory.max_turn_bytes)
            * 256
            + 10_000_000
    );
    memory
        .validate_host_limits(&BrokerHostLimits::default())
        .expect("default host and memory limits compose");
    for host in [
        BrokerHostLimits {
            max_input_bytes: usize::try_from(
                memory.maximum_provider_input_bytes().expect("input") - 1,
            )
            .expect("input fits usize"),
            ..BrokerHostLimits::default()
        },
        BrokerHostLimits {
            max_memory_bytes: usize::try_from(
                memory
                    .maximum_provider_working_set_bytes()
                    .expect("working set")
                    - 1,
            )
            .expect("working set fits usize"),
            ..BrokerHostLimits::default()
        },
        BrokerHostLimits {
            fuel: minimum_fuel - 1,
            ..BrokerHostLimits::default()
        },
    ] {
        assert!(memory.validate_host_limits(&host).is_err());
    }

    query_dominates.max_query_bytes = u64::MAX;
    assert!(query_dominates.maximum_provider_input_bytes().is_err());
}

#[test]
fn every_authority_ceiling_is_canonical_and_semantic() {
    type Mutation<T> = (&'static str, fn(&mut T));

    fn encoded_host(limits: &BrokerHostLimits) -> Vec<u8> {
        let mut encoded = AuthorityEncoder::new();
        encode_host_limits(&mut encoded, limits);
        encoded.finish()
    }
    fn encoded_storage(limits: &dekopon_storage_host::StorageLimits) -> Vec<u8> {
        let mut encoded = AuthorityEncoder::new();
        encode_storage_limits(&mut encoded, limits);
        encoded.finish()
    }
    fn encoded_memory(config: &ChatMemoryConfig) -> Vec<u8> {
        let mut encoded = AuthorityEncoder::new();
        encode_memory_config(&mut encoded, config);
        encoded.finish()
    }
    fn assert_rotations<T: Clone>(
        baseline: &T,
        mutations: &[Mutation<T>],
        encode: impl Fn(&T) -> Vec<u8>,
    ) {
        let baseline_bytes = encode(baseline);
        for (field, mutate) in mutations {
            let mut changed = baseline.clone();
            mutate(&mut changed);
            assert_ne!(baseline_bytes, encode(&changed), "{field} did not rotate");
        }
    }

    let host = BrokerHostLimits::default();
    let host_mutations: &[Mutation<BrokerHostLimits>] = &[
        ("maxMemoryBytes", |v| v.max_memory_bytes += 1),
        ("maxTableElements", |v| v.max_table_elements += 1),
        ("maxInstances", |v| v.max_instances += 1),
        ("maxTables", |v| v.max_tables += 1),
        ("maxMemories", |v| v.max_memories += 1),
        ("maxInputBytes", |v| v.max_input_bytes += 1),
        ("maxOutputBytes", |v| v.max_output_bytes += 1),
        ("maxHttpRequests", |v| v.max_http_requests += 1),
        ("maxHttpRequestBytes", |v| v.max_http_request_bytes += 1),
        ("maxHttpResponseBytes", |v| v.max_http_response_bytes += 1),
        ("maxHttpHeaders", |v| v.max_http_headers += 1),
        ("maxHttpHeaderBytes", |v| v.max_http_header_bytes += 1),
        ("fuel", |v| v.fuel += 1),
        ("maxTimeout", |v| {
            v.max_timeout += std::time::Duration::from_nanos(1);
        }),
    ];
    assert_rotations(&host, host_mutations, encoded_host);

    let storage = dekopon_storage_host::StorageLimits::default();
    let storage_mutations: &[Mutation<dekopon_storage_host::StorageLimits>] = &[
        ("maxRootBytes", |v| v.max_root_bytes += 1),
        ("maxNamespaces", |v| v.max_namespaces += 1),
        ("maxNamespaceBytes", |v| v.max_namespace_bytes += 1),
        ("maxFilesPerNamespace", |v| v.max_files_per_namespace += 1),
        ("maxFileBytes", |v| v.max_file_bytes += 1),
        ("maxOpenHandles", |v| v.max_open_handles += 1),
        ("maxHandlesPerInvocation", |v| {
            v.max_handles_per_invocation += 1
        }),
        ("maxHostCallsPerInvocation", |v| {
            v.max_host_calls_per_invocation += 1
        }),
        ("maxReadBytesPerCall", |v| v.max_read_bytes_per_call += 1),
        ("maxReadBytesPerInvocation", |v| {
            v.max_read_bytes_per_invocation += 1
        }),
        ("maxWriteBytesPerCall", |v| v.max_write_bytes_per_call += 1),
        ("maxWriteBytesPerInvocation", |v| {
            v.max_write_bytes_per_invocation += 1
        }),
        ("maxEntropyBytesPerCall", |v| {
            v.max_entropy_bytes_per_call += 1
        }),
        ("maxEntropyBytesPerInvocation", |v| {
            v.max_entropy_bytes_per_invocation += 1
        }),
        ("lockTimeoutMs", |v| v.lock_timeout_ms += 1),
        ("finalizationBudgetMs", |v| v.finalization_budget_ms += 1),
        ("maxActiveInvocations", |v| v.max_active_invocations += 1),
        ("startupMaxEntries", |v| v.startup_max_entries += 1),
    ];
    assert_rotations(&storage, storage_mutations, encoded_storage);

    let memory = ChatMemoryConfig {
        continuity_policy: dekopon_storage_host::ContinuityPolicy::Paranoid,
        enabled_agents: vec![
            "reviewer".parse().expect("agent"),
            "other".parse().expect("agent"),
        ],
        max_lookback_turns: 200,
        max_recent_turns: 20,
        max_search_results: 20,
        max_query_bytes: 256,
        max_result_bytes: 65_536,
        max_turn_bytes: 32_768,
        compaction_target_bytes: 8_388_608,
        compaction_threshold_bytes: 12_582_912,
        continuity_migration: None,
    };
    let memory_mutations: &[Mutation<ChatMemoryConfig>] = &[
        ("continuityPolicy", |v| {
            v.continuity_policy = dekopon_storage_host::ContinuityPolicy::Relaxed;
        }),
        ("maxLookbackTurns", |v| v.max_lookback_turns += 1),
        ("maxRecentTurns", |v| v.max_recent_turns += 1),
        ("maxSearchResults", |v| v.max_search_results += 1),
        ("maxQueryBytes", |v| v.max_query_bytes += 1),
        ("maxResultBytes", |v| v.max_result_bytes += 1),
        ("maxTurnBytes", |v| v.max_turn_bytes += 1),
        ("compactionTargetBytes", |v| v.compaction_target_bytes += 1),
        ("compactionThresholdBytes", |v| {
            v.compaction_threshold_bytes += 1
        }),
    ];
    assert_rotations(&memory, memory_mutations, encoded_memory);

    let mut reordered_agents = memory.clone();
    reordered_agents.enabled_agents.reverse();
    assert_eq!(encoded_memory(&memory), encoded_memory(&reordered_agents));
    let mut unrelated_agent = memory.clone();
    unrelated_agent
        .enabled_agents
        .push("unrelated".parse().expect("agent"));
    assert_eq!(encoded_memory(&memory), encoded_memory(&unrelated_agent));
}

#[test]
fn capability_authority_commits_exactly_these_fields() {
    fn labels(bytes: &[u8]) -> Vec<String> {
        let mut labels = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let (length, tail) = rest.split_at(8);
            let length =
                u64::from_be_bytes(length.try_into().expect("eight length bytes")) as usize;
            let (label, tail) = tail.split_at(length);
            labels.push(String::from_utf8(label.to_vec()).expect("labels are UTF-8"));
            let (length, tail) = tail.split_at(8);
            let length =
                u64::from_be_bytes(length.try_into().expect("eight length bytes")) as usize;
            rest = &tail[length..];
        }
        labels
    }

    let set = ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: "cli-probe".parse().expect("valid provider fixture"),
        effect: dekopon_capability::EffectKind::ReadOnly,
        risk: dekopon_core::RiskLevel::Low,
        constraints: ExecutionConstraints::default(),
    };
    let mut encoded = AuthorityEncoder::new();
    encode_capability_authority(
        &mut encoded,
        &"cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("valid fixture"),
        &set,
        Some(&SecretUseProposal::HttpBearer {
            secret: "drn:secret:probe/token".parse().expect("secret DRN"),
        }),
        "sha256:artifact",
    );

    assert_eq!(
        labels(&encoded.finish()),
        vec![
            "capability",
            "provider",
            "effect",
            "risk",
            "defaultSecretDrn",
            "defaultSecretSink",
            "defaultSecretBasicUsername.present",
            "execution.timeoutMs",
            "execution.http.present",
            "execution.storage.present",
            "execution.asset.present",
            "providerArtifactSha256",
        ],
        "the storage authority surface gained or lost a field"
    );

    let mut shared = set.clone();
    shared.constraints.storage = Some(StorageConstraints {
        interface: StorageInterface::DurableFiles,
        access: StorageAccess::ReadOnly,
        scope: StorageScope::Agent,
        retention: Default::default(),
        namespace: Some("turso".parse().expect("namespace")),
    });
    let mut encoded = AuthorityEncoder::new();
    encode_capability_authority(
        &mut encoded,
        &"cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("valid fixture"),
        &shared,
        None,
        "sha256:artifact",
    );
    assert_eq!(
        labels(&encoded.finish()),
        vec![
            "capability",
            "provider",
            "effect",
            "risk",
            "execution.timeoutMs",
            "execution.http.present",
            "execution.storage.present",
            "execution.storage.interface",
            "execution.storage.access",
            "execution.storage.namespace",
            "execution.storage.sharedNamespace",
            "execution.asset.present",
            "providerArtifactSha256",
        ],
        "the storage authority surface gained or lost a field"
    );
}

#[test]
fn a_flag_false_http_constraint_set_encodes_as_before_the_field_existed() {
    let constraints = ExecutionConstraints {
        timeout_ms: 30_000,
        http: Some(HttpConstraints {
            allowed_hosts: vec!["a.example:443".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 2,
            max_request_bytes: 3,
            max_response_bytes: 4,
            allow_plaintext_loopback: false,
            propagate_trace: false,
            request_templates: Vec::new(),
            signed_download: None,
        }),
        ..ExecutionConstraints::default()
    };
    let mut legacy = AuthorityEncoder::new();
    legacy.number("execution.timeoutMs", 30_000);
    legacy.byte("execution.http.present", 1);
    legacy.number("execution.http.allowedHostCount", 1);
    legacy.text("execution.http.allowedHost", "a.example:443");
    legacy.number("execution.http.allowedMethodCount", 1);
    legacy.text("execution.http.allowedMethod", "GET");
    legacy.number("execution.http.maxRequests", 2);
    legacy.number("execution.http.maxRequestBytes", 3);
    legacy.number("execution.http.maxResponseBytes", 4);
    legacy.boolean("execution.http.allowPlaintextLoopback", false);
    legacy.byte("execution.storage.present", 0);
    legacy.byte("execution.asset.present", 0);

    let mut encoded = AuthorityEncoder::new();
    encode_execution_constraints(&mut encoded, &constraints);
    assert_eq!(encoded.finish(), legacy.finish());
}

#[test]
fn execution_authority_normalizes_sets_but_commits_every_constraint() {
    fn bytes(constraints: &ExecutionConstraints) -> Vec<u8> {
        let mut encoded = AuthorityEncoder::new();
        encode_execution_constraints(&mut encoded, constraints);
        encoded.finish()
    }
    let baseline = ExecutionConstraints {
        asset: None,
        timeout_ms: 30_000,
        http: Some(HttpConstraints {
            allowed_hosts: vec!["b.example:443".to_owned(), "a.example:443".to_owned()],
            allowed_methods: vec!["POST".to_owned(), "GET".to_owned()],
            max_requests: 2,
            max_request_bytes: 3,
            max_response_bytes: 4,
            allow_plaintext_loopback: false,
            propagate_trace: false,
            request_templates: Vec::new(),
            signed_download: None,
        }),
        storage: None,
        secret_use: None,
    };
    let mut reordered = baseline.clone();
    let http = reordered.http.as_mut().expect("HTTP");
    http.allowed_hosts.reverse();
    http.allowed_methods.reverse();
    http.allowed_hosts.push("a.example:443".to_owned());
    assert_eq!(bytes(&baseline), bytes(&reordered));

    macro_rules! changes {
        ($mutation:expr) => {{
            let mut changed = baseline.clone();
            ($mutation)(&mut changed);
            assert_ne!(bytes(&baseline), bytes(&changed));
        }};
    }
    changes!(|v: &mut ExecutionConstraints| v.timeout_ms += 1);
    changes!(|v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").max_requests += 1);
    changes!(|v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").max_request_bytes += 1);
    changes!(|v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").max_response_bytes += 1);
    changes!(|v: &mut ExecutionConstraints| v
        .http
        .as_mut()
        .expect("HTTP")
        .allow_plaintext_loopback = true);
    changes!(|v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").propagate_trace = true);
    changes!(
        |v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").signed_download =
            Some(dekopon_capability::SignedDownload::GithubActionsLogsV1)
    );
    changes!(|v: &mut ExecutionConstraints| v
        .http
        .as_mut()
        .expect("HTTP")
        .allowed_hosts
        .push("c.example:443".to_owned()));
    changes!(|v: &mut ExecutionConstraints| v
        .http
        .as_mut()
        .expect("HTTP")
        .allowed_methods
        .push("PATCH".to_owned()));
    let template = |value: serde_json::Value| {
        serde_json::from_value::<RequestTemplate>(value).expect("template fixture")
    };
    let replies = template(serde_json::json!({
        "method": "GET",
        "path": "/api/conversations.replies",
        "query": {"pinned": {"channel": "conversation.id"}, "allowed": ["limit"]}
    }));
    let history = template(serde_json::json!({
        "method": "GET",
        "path": "/channels/{conversation.apiChannel}/messages"
    }));
    let mut templated = baseline.clone();
    templated.http.as_mut().expect("HTTP").request_templates =
        vec![replies.clone(), history.clone()];
    assert_ne!(bytes(&baseline), bytes(&templated));
    let mut reordered_templates = templated.clone();
    reordered_templates
        .http
        .as_mut()
        .expect("HTTP")
        .request_templates
        .reverse();
    assert_eq!(bytes(&templated), bytes(&reordered_templates));
    for changed in [
        serde_json::json!({"method": "GET", "path": "/api/conversations.history",
            "query": {"pinned": {"channel": "conversation.id"}, "allowed": ["limit"]}}),
        serde_json::json!({"method": "POST", "path": "/api/conversations.replies",
            "query": {"pinned": {"channel": "conversation.id"}, "allowed": ["limit"]}}),
        serde_json::json!({"method": "GET", "path": "/api/conversations.replies",
            "query": {"pinned": {"channel": "conversation.apiChannel"}, "allowed": ["limit"]}}),
        serde_json::json!({"method": "GET", "path": "/api/conversations.replies",
            "query": {"pinned": {"channel": "conversation.id"}, "allowed": ["cursor"]}}),
        serde_json::json!({"method": "GET", "path": "/api/conversations.replies",
            "query": {"pinned": {"channel": "conversation.id", "ts": "conversation.thread"},
                      "allowed": ["limit"]}}),
    ] {
        let mut mutated = templated.clone();
        mutated.http.as_mut().expect("HTTP").request_templates =
            vec![template(changed.clone()), history.clone()];
        assert_ne!(bytes(&templated), bytes(&mutated), "{changed}");
    }

    let storage = ExecutionConstraints {
        http: None,
        storage: Some(StorageConstraints {
            interface: StorageInterface::Jsonl,
            access: StorageAccess::ReadOnly,
            scope: StorageScope::PrivateConversation,
            retention: Default::default(),
            namespace: None,
        }),
        ..ExecutionConstraints::default()
    };
    let mut retained = storage.clone();
    retained.storage.as_mut().expect("storage").retention =
        dekopon_capability::StorageRetention::IdleTtl(std::time::Duration::from_secs(30));
    assert_eq!(
        bytes(&storage),
        bytes(&retained),
        "retention changes must not rotate private authority generations"
    );
    let mut shared = storage.clone();
    shared.storage.as_mut().expect("storage").scope = StorageScope::SharedConversation;
    assert_ne!(bytes(&storage), bytes(&shared));
    let mut durable = storage.clone();
    durable.storage.as_mut().expect("storage").interface = StorageInterface::DurableFiles;
    assert_ne!(bytes(&storage), bytes(&durable));
    let mut writable = storage.clone();
    writable.storage.as_mut().expect("storage").access = StorageAccess::ReadWrite;
    assert_ne!(bytes(&storage), bytes(&writable));

    let mut legacy = AuthorityEncoder::new();
    legacy.number("execution.timeoutMs", 30_000);
    legacy.byte("execution.http.present", 0);
    legacy.byte("execution.storage.present", 1);
    legacy.byte("execution.storage.interface", 0);
    legacy.byte("execution.storage.access", 0);
    legacy.byte("execution.storage.namespace", 0);
    legacy.byte("execution.asset.present", 0);
    assert_eq!(
        bytes(&storage),
        legacy.finish(),
        "an absent shared namespace encodes as before the field existed"
    );
    let mut named = storage.clone();
    named.storage.as_mut().expect("storage").namespace = Some("turso".parse().expect("name"));
    let mut renamed = storage.clone();
    renamed.storage.as_mut().expect("storage").namespace = Some("sqlite".parse().expect("name"));
    assert_ne!(bytes(&storage), bytes(&named));
    assert_ne!(bytes(&named), bytes(&renamed));
}

#[test]
fn pre_execution_storage_failures_keep_their_public_category() {
    for (source, expected) in [
        (
            dekopon_storage_host::StorageHostError::QuotaExceeded,
            "storage-quota",
        ),
        (dekopon_storage_host::StorageHostError::Busy, "storage-busy"),
        (
            dekopon_storage_host::StorageHostError::Timeout,
            "storage-timeout",
        ),
        (
            dekopon_storage_host::StorageHostError::corrupt("test"),
            "storage-corrupt",
        ),
        (dekopon_storage_host::StorageHostError::Io, "storage-io"),
    ] {
        let error = super::BrokerError::Storage { source };
        assert_eq!(error.storage_failure_code(), Some(expected));
        assert!(!error.storage_namespace_reset(), "{expected}");
    }

    let reset = super::BrokerError::Storage {
        source: dekopon_storage_host::StorageHostError::Corrupt {
            scope: "authority-pointer",
            site: Some(Box::new(dekopon_storage_host::CorruptionSite {
                reset: Some("fresh".to_owned()),
                ..dekopon_storage_host::CorruptionSite::default()
            })),
        },
    };
    assert_eq!(reset.storage_failure_code(), Some("storage-corrupt"));
    assert!(reset.storage_namespace_reset());
}

#[test]
fn exhausted_bounds_are_terminal_rather_than_retriable() {
    for error in [
        super::BrokerError::DecisionAudit {
            source: super::AuditError::Full { maximum: 200_000 },
        },
        super::BrokerError::AuthorizedFailureAudit {
            source: super::AuditError::Full { maximum: 200_000 },
        },
    ] {
        assert_eq!(error.capacity_failure_code(), Some("capacity-exhausted"));
        assert_eq!(
            error.unaudited_outcome(),
            None,
            "an exhaustion refuses before execution"
        );
        assert_eq!(error.storage_failure_code(), None);
    }

    let invocation = "invoke-terminal"
        .parse::<InvocationId>()
        .expect("valid invocation fixture");
    let terminal = super::BrokerError::OutcomeAudit {
        invocation: invocation.clone(),
        source: super::AuditError::Full { maximum: 200_000 },
    };
    assert_eq!(terminal.capacity_failure_code(), None);
    assert_eq!(terminal.unaudited_outcome(), Some(&invocation));
}

#[tokio::test]
async fn a_panicking_storage_materialization_keeps_its_panic_and_its_public_category() {
    let source = tokio::task::spawn_blocking(|| panic!("namespace generation pointer is missing"))
        .await
        .expect_err("the blocking task panics");
    let error = super::BrokerError::StorageTask { source };

    assert_eq!(error.storage_failure_code(), Some("storage-io"));
    assert!(error.unaudited_outcome().is_none());
    let cause = std::error::Error::source(&error).expect("the join failure is the cause");
    assert!(
        cause
            .to_string()
            .contains("namespace generation pointer is missing"),
        "the panic message was discarded: {cause}"
    );
}

#[test]
fn authenticated_human_identity_must_match_transport_principal() {
    let error = AuthenticatedContext::new(
        "alice"
            .parse::<PrincipalId>()
            .expect("valid principal fixture"),
        Actor::Human {
            principal: "mallory"
                .parse::<PrincipalId>()
                .expect("valid principal fixture"),
        },
    )
    .expect_err("payload identity cannot override transport identity");
    assert_eq!(error, ContextError::PrincipalMismatch);
}

#[tokio::test]
async fn in_memory_audit_fails_closed_at_its_bound() {
    assert!(matches!(
        InMemoryAuditLog::new(0),
        Err(AuditConfigurationError::ZeroMaximum)
    ));
    let audit = InMemoryAuditLog::new(1).expect("valid audit bound");
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("first append succeeds");
    let error = audit
        .append(decision("invoke-two", true))
        .await
        .expect_err("second append exceeds bound");
    assert!(matches!(error, AuditError::Full { maximum: 1 }));
}

#[test]
fn policy_http_scope_values_are_bounded() {
    fn constrain(http: dekopon_capability::HttpConstraints) -> ConstraintSet {
        ConstraintSet {
            default_secret_use: None,
            route: CapabilityRoute::Generic,
            provider: "cli-probe".parse().expect("provider"),
            effect: dekopon_capability::EffectKind::ReadOnly,
            risk: dekopon_core::RiskLevel::Low,
            constraints: ExecutionConstraints {
                http: Some(http),
                ..ExecutionConstraints::default()
            },
        }
    }

    let valid = dekopon_capability::HttpConstraints {
        allowed_hosts: vec!["api.github.com".to_owned(), "127.0.0.1:8080".to_owned()],
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned(), "PATCH".to_owned()],
        max_requests: 1,
        max_request_bytes: 1,
        max_response_bytes: 1,
        allow_plaintext_loopback: false,
        propagate_trace: false,
        request_templates: Vec::new(),
        signed_download: None,
    };
    assert!(super::validate_set_constraints(&constrain(valid.clone())).is_ok());

    let rejected_hosts = [
        String::new(),
        " ".to_owned(),
        " api.github.com".to_owned(),
        "api.github.com ".to_owned(),
        "*".to_owned(),
        "a/b".to_owned(),
        "user@host".to_owned(),
        "host?query".to_owned(),
        "host#fragment".to_owned(),
        "host\tname".to_owned(),
        "h".repeat(513),
    ];
    for host in rejected_hosts {
        let set = constrain(dekopon_capability::HttpConstraints {
            allowed_hosts: vec![host.clone()],
            ..valid.clone()
        });
        assert!(
            super::validate_set_constraints(&set).is_err(),
            "host {host:?} must not start this broker"
        );
    }

    let rejected_methods = [
        String::new(),
        "GET POST".to_owned(),
        "GE\tT".to_owned(),
        "G/T".to_owned(),
        "M".repeat(65),
    ];
    for method in rejected_methods {
        let set = constrain(dekopon_capability::HttpConstraints {
            allowed_methods: vec![method.clone()],
            ..valid.clone()
        });
        assert!(
            super::validate_set_constraints(&set).is_err(),
            "method {method:?} must not start this broker"
        );
    }

    let unbounded: &[fn(&mut HttpConstraints)] = &[
        |v| v.allowed_hosts.clear(),
        |v| v.allowed_methods.clear(),
        |v| v.allowed_hosts = (0..65).map(|index| format!("h{index}.test")).collect(),
        |v| v.allowed_methods = (0..65).map(|index| format!("M{index}")).collect(),
        |v| v.max_requests = 0,
        |v| v.max_request_bytes = 0,
        |v| v.max_response_bytes = 0,
    ];
    for mutate in unbounded {
        let mut http = valid.clone();
        mutate(&mut http);
        assert!(
            super::validate_set_constraints(&constrain(http.clone())).is_err(),
            "{http:?} must not start this broker"
        );
    }
}

#[test]
fn an_agent_swaps_only_the_default_its_set_already_names() {
    let document = r#"{
        "defaultSecretUse": { "drn": "drn:secret:github/app-ville", "sink": "httpBearer" },
        "provider": "gh",
        "effect": "external-write",
        "risk": "Medium",
        "constraints": { "timeoutMs": 1000 }
    }"#;
    let named =
        serde_json::from_str::<super::ConstraintSet>(document).expect("authored set decodes");
    let unnamed = super::ConstraintSet {
        default_secret_use: None,
        ..named.clone()
    };
    let agent = |name: &str| name.parse::<AgentId>().expect("valid agent");
    let drn = |value: &str| value.parse::<SecretDrn>().expect("secret DRN");
    let catalog = super::ConstraintCatalog::new([(
        "gh.pull-request.merge"
            .parse::<CapabilityId>()
            .expect("valid capability"),
        named.clone(),
    )])
    .expect("one set builds a catalog")
    .with_agent_secrets(BTreeMap::from([(
        agent("lange-family"),
        BTreeMap::from([(
            drn("drn:secret:github/app-ville"),
            drn("drn:secret:github/pat-chat-is-this-real"),
        )]),
    )]));
    let actor = |name: &str| Actor::Agent { agent: agent(name) };
    let effective = |set: &ConstraintSet, actor: &Actor| -> Option<(String, SecretSinkKind)> {
        catalog
            .effective_default(set, actor)
            .map(|(secret, sink)| (secret.as_str().to_owned(), sink))
    };

    assert_eq!(
        effective(&named, &actor("lange-family")),
        Some((
            "drn:secret:github/pat-chat-is-this-real".to_owned(),
            SecretSinkKind::HttpBearer
        ))
    );
    assert_eq!(
        effective(&named, &actor("ville-github")),
        Some((
            "drn:secret:github/app-ville".to_owned(),
            SecretSinkKind::HttpBearer
        ))
    );
    assert_eq!(
        effective(
            &named,
            &Actor::Service {
                principal: "local-user"
                    .parse::<PrincipalId>()
                    .expect("valid principal"),
            }
        ),
        Some((
            "drn:secret:github/app-ville".to_owned(),
            SecretSinkKind::HttpBearer
        ))
    );
    assert_eq!(effective(&unnamed, &actor("lange-family")), None);

    assert!(
        serde_json::from_str::<super::ConstraintSet>(&document.replace(
            r#""provider": "gh","#,
            r#""provider": "gh", "credential": "github-pat","#,
        ))
        .is_err(),
        "the retired credential key must not decode"
    );
}

#[test]
fn a_provider_exit_keeps_its_classification_and_carries_its_status_and_stderr() {
    let message = "the image route refused the request with HTTP 400 (moderation_blocked: the \
                   request was rejected)";
    for (message, expected) in [
        (message.to_owned(), message.to_owned()),
        (
            "m".repeat(MAX_FAILURE_MESSAGE_BYTES + 1),
            format!(
                "{}\u{2026}[truncated]",
                "m".repeat(MAX_FAILURE_MESSAGE_BYTES)
            ),
        ),
    ] {
        let failure = BrokerHostError::ProviderFailure {
            provider: "gpt-image".parse::<ProviderId>().expect("valid provider"),
            capability: "gpt-image.edit"
                .parse::<CapabilityId>()
                .expect("valid capability"),
            status: 1,
            stderr: message,
        };
        assert_eq!(public_host_error(&failure), "provider-failure");
        assert_eq!(
            provider_exit_status(&failure).map(std::num::NonZeroU8::get),
            Some(1)
        );
        assert_eq!(
            provider_failure_detail(&failure),
            Some(ProviderFailureDetail {
                code: "provider-exit".to_owned(),
                message: expected,
            })
        );
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("\\{byte:02x}")).collect()
}

fn stdio_probe_manifest() -> String {
    serde_json::to_string(&serde_json::json!({
        "apiVersion": "dekopon.dev/provider/v1alpha1",
        "id": "stdio-probe",
        "description": "Inline standard-stream probe",
        "commandWords": ["stdioprobe"],
        "capabilities": [{
            "id": "stdio-probe.run",
            "description": "Runs the inline body",
            "effect": dekopon_capability::EffectKind::ReadOnly,
            "risk": dekopon_core::RiskLevel::Low,
            "inputSchema": {"type": "object"}
        }]
    }))
    .unwrap()
}

fn manifest_descriptor(offset: u32, length: usize) -> String {
    hex(&[
        offset.to_le_bytes(),
        u32::try_from(length).unwrap().to_le_bytes(),
    ]
    .concat())
}

fn stdout_provider_exiting_eight() -> tempfile::NamedTempFile {
    let manifest = stdio_probe_manifest();
    let wat = format!(
        r#"(component
        (import "dekopon:stdio/streams@0.1.0" (instance $streams
            (type $error (enum "closed"))
            (export "write-error" (type $write-error (eq $error)))
            (export "writer" (type $writer (sub resource)))
            (export "[method]writer.write" (func (param "self" (borrow $writer)) (param "bytes" (list u8)) (result (result (error $write-error)))))
            (export "stdout" (func (result (own $writer))))))
        (core module $memory
            (memory (export "memory") 1)
            (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096))
        (core instance $memory (instantiate $memory))
        (alias core export $memory "memory" (core memory $mem))
        (core func $write (canon lower (func $streams "[method]writer.write") (memory $mem)))
        (core func $stdout (canon lower (func $streams "stdout")))
        (core module $provider
            (import "memory" "memory" (memory 1))
            (import "streams" "write" (func $write (param i32 i32 i32 i32)))
            (import "streams" "stdout" (func $stdout (result i32)))
            (data (i32.const 0) "{descriptor}")
            (data (i32.const 64) "{manifest}")
            (data (i32.const 2048) "boom\0a")
            (data (i32.const 16) "\01\08")
            (func (export "describe") (result i32) i32.const 0)
            (func (export "command") (param i32 i32 i32) (result i32) i32.const 0)
            (func (export "invoke") (param i32 i32 i32 i32) (result i32)
                call $stdout i32.const 2048 i32.const 5 i32.const 32 call $write
                i32.const 16))
        (core instance $provider (instantiate $provider
            (with "memory" (instance $memory))
            (with "streams" (instance (export "write" (func $write)) (export "stdout" (func $stdout))))))
        (func (export "describe") (result string) (canon lift (core func $provider "describe") (memory $mem)))
        (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
            (canon lift (core func $provider "invoke") (memory $mem) (realloc (core func $memory "realloc"))))
        (func (export "run-command") (param "argv" (list string)) (param "stdin-piped" bool) (result string)
            (canon lift (core func $provider "command") (memory $mem) (realloc (core func $memory "realloc")))))"#,
        descriptor = manifest_descriptor(64, manifest.len()),
        manifest = hex(manifest.as_bytes()),
    );
    let mut provider = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut provider, wat.as_bytes()).unwrap();
    provider
}

fn provider_succeeding_after_writing_stderr() -> tempfile::NamedTempFile {
    inline_provider_declaring(&stdio_probe_manifest())
}

fn inline_provider_declaring(manifest: &str) -> tempfile::NamedTempFile {
    let wat = format!(
        r#"(component
        (import "dekopon:stdio/streams@0.1.0" (instance $streams
            (export "write-stderr" (func (param "text" string)))))
        (core module $memory
            (memory (export "memory") 1)
            (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096))
        (core instance $memory (instantiate $memory))
        (alias core export $memory "memory" (core memory $mem))
        (core func $stderr (canon lower (func $streams "write-stderr") (memory $mem)))
        (core module $provider
            (import "memory" "memory" (memory 1))
            (import "streams" "stderr" (func $stderr (param i32 i32)))
            (data (i32.const 0) "{descriptor}")
            (data (i32.const 64) "{manifest}")
            (data (i32.const 2048) "warn\0a")
            (data (i32.const 16) "\00\00")
            (func (export "describe") (result i32) i32.const 0)
            (func (export "command") (param i32 i32 i32) (result i32) i32.const 0)
            (func (export "invoke") (param i32 i32 i32 i32) (result i32)
                i32.const 2048 i32.const 5 call $stderr
                i32.const 16))
        (core instance $provider (instantiate $provider
            (with "memory" (instance $memory))
            (with "streams" (instance (export "stderr" (func $stderr))))))
        (func (export "describe") (result string) (canon lift (core func $provider "describe") (memory $mem)))
        (func (export "invoke") (param "capability" string) (param "input-json" string) (result (result (error u8)))
            (canon lift (core func $provider "invoke") (memory $mem) (realloc (core func $memory "realloc"))))
        (func (export "run-command") (param "argv" (list string)) (param "stdin-piped" bool) (result string)
            (canon lift (core func $provider "command") (memory $mem) (realloc (core func $memory "realloc")))))"#,
        descriptor = manifest_descriptor(64, manifest.len()),
        manifest = hex(manifest.as_bytes()),
    );
    let mut provider = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut provider, wat.as_bytes()).unwrap();
    provider
}

#[tokio::test]
async fn a_provider_exit_status_is_recorded_on_the_execution_record() {
    use dekopon_broker_host::{BrokerProviderRegistry, Streams, asset::AssetInputs};
    use dekopon_capability::EffectKind;
    use dekopon_core::RiskLevel;
    use dekopon_test_support::CaptureLayer;
    use tracing_subscriber::prelude::*;

    let provider = stdout_provider_exiting_eight();
    let registry = BrokerProviderRegistry::load([provider.path()], BrokerHostLimits::default())
        .await
        .expect("stdout-writing provider loads");
    let principal: PrincipalId = "caller".parse().unwrap();
    let capability: CapabilityId = "stdio-probe.run".parse().unwrap();
    let provider_id: ProviderId = "stdio-probe".parse().unwrap();
    let world = PolicyWorld::new(
        [principal.clone()],
        [(capability.clone(), provider_id.clone())],
    )
    .unwrap();
    let broker = Broker::new(
        registry,
        "broker".parse().unwrap(),
        PolicyEngine::new(
            r#"@id("exit-status") permit(principal, action, resource);"#,
            &world,
        )
        .unwrap(),
        ConstraintCatalog::new([(
            capability.clone(),
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider: provider_id,
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                constraints: ExecutionConstraints::default(),
            },
        )])
        .unwrap(),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(4).unwrap()),
        BrokerLimits::default(),
    )
    .unwrap();
    let captured = CaptureLayer::workspace();
    let json = tempfile::NamedTempFile::new().unwrap();
    let _subscriber = tracing_subscriber::registry()
        .with(captured.clone())
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::sync::Mutex::new(json.reopen().unwrap())),
        )
        .set_default();
    let context = AuthenticatedContext::attested(
        principal,
        Actor::Agent {
            agent: "provider-test".parse().unwrap(),
        },
        "gateway".parse().unwrap(),
        "drn:slack:user:team:user".parse().unwrap(),
    )
    .unwrap();
    let (host, mut stdout) = std::os::unix::net::UnixStream::pair().unwrap();
    let _result = broker
        .invoke(
            &context,
            None,
            None,
            super::InvocationRequest {
                id: "invoke-exit".parse().unwrap(),
                capability,
                trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-01"
                    .parse()
                    .unwrap(),
                input: serde_json::json!({}),
                secret_use: None,
            },
            AssetInputs {
                streams: Some(Streams {
                    stdin: None,
                    stdout: host.into(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut output = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut output).unwrap();
    assert_eq!(output, "boom\n", "{_result:?} {}", captured.events_text());
    let records = captured.events_where(
        "dekopon_broker::audit",
        &[("audit.event", "broker.execution")],
    );
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].field("outcome"), Some("Failed"));
    let json = std::fs::read(json.path()).unwrap();
    let records: Vec<serde_json::Value> = json
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .filter(|record: &serde_json::Value| record["fields"]["audit.event"] == "broker.execution")
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["fields"]["process.exit.code"].as_u64(), Some(8));
}

#[test]
fn a_host_failure_no_provider_reported_carries_no_detail() {
    for failure in [
        BrokerHostError::Timeout {
            operation: "invoke gpt-image.edit".to_owned(),
            timeout_ms: 30_000,
        },
        BrokerHostError::StorageDisabled,
    ] {
        assert_eq!(provider_failure_detail(&failure), None);
        assert_eq!(provider_exit_status(&failure), None);
    }
}

#[test]
fn asset_grants_preserve_effect_classes_and_the_http_storage_exclusion() {
    use dekopon_capability::{
        AssetConstraints, EffectKind, StorageAccess, StorageConstraints, StorageInterface,
        StorageScope,
    };
    let mut set = ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: "probe".parse().unwrap(),
        effect: EffectKind::LocalWrite,
        risk: dekopon_core::RiskLevel::Low,
        constraints: ExecutionConstraints {
            asset: Some(AssetConstraints {
                attach: true,
                remove: true,
                send: false,
            }),
            ..Default::default()
        },
    };
    assert!(super::validate_set_constraints(&set).is_ok());
    set.constraints.storage = Some(StorageConstraints {
        interface: StorageInterface::DurableFiles,
        access: StorageAccess::ReadWrite,
        scope: StorageScope::PrivateConversation,
        retention: Default::default(),
        namespace: None,
    });
    assert!(super::validate_set_constraints(&set).is_ok());
    set.constraints.http = Some(dekopon_capability::HttpConstraints {
        allowed_hosts: vec!["example.com".to_owned()],
        allowed_methods: vec!["POST".to_owned()],
        max_requests: 1,
        max_request_bytes: 1,
        max_response_bytes: 1,
        allow_plaintext_loopback: false,
        propagate_trace: false,
        request_templates: Vec::new(),
        signed_download: None,
    });
    assert!(matches!(
        super::validate_set_constraints(&set),
        Err(super::BrokerBuildError::InvalidPolicyConstraints)
    ));
    set.constraints.storage = None;
    set.effect = EffectKind::ExternalWrite;
    set.constraints.asset.as_mut().unwrap().send = true;
    assert!(super::validate_set_constraints(&set).is_ok());
    set.effect = EffectKind::ReadOnly;
    assert!(matches!(
        super::validate_set_constraints(&set),
        Err(super::BrokerBuildError::InvalidPolicyConstraints)
    ));
    set.effect = EffectKind::LocalWrite;
    assert!(matches!(
        super::validate_set_constraints(&set),
        Err(super::BrokerBuildError::InvalidPolicyConstraints)
    ));
}

#[test]
fn storage_retention_policy_rejects_conflicts_for_one_resource_family() {
    use super::{BrokerBuildError, ConstraintCatalog};
    use dekopon_capability::{EffectKind, StorageRetention};
    let make = |retention| ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: "storage-probe".parse().unwrap(),
        effect: EffectKind::ReadOnly,
        risk: dekopon_core::RiskLevel::Low,
        constraints: ExecutionConstraints {
            storage: Some(StorageConstraints {
                interface: StorageInterface::DurableFiles,
                access: StorageAccess::ReadOnly,
                scope: StorageScope::Agent,
                retention,
                namespace: None,
            }),
            ..Default::default()
        },
    };
    let keep = make(StorageRetention::Keep);
    let ttl = make(StorageRetention::IdleTtl(std::time::Duration::from_secs(
        20,
    )));
    let catalog = ConstraintCatalog::new([
        ("probe.vfs".parse().unwrap(), keep),
        ("probe.jsonl".parse().unwrap(), ttl),
    ])
    .unwrap();
    assert!(matches!(
        catalog.retention_policies(),
        Err(BrokerBuildError::ConflictingStorageRetention { .. })
    ));
    let zero = ConstraintCatalog::new([(
        "probe.vfs".parse().unwrap(),
        make(StorageRetention::IdleTtl(std::time::Duration::ZERO)),
    )])
    .unwrap();
    assert!(matches!(
        zero.retention_policies(),
        Err(BrokerBuildError::InvalidStorageRetention)
    ));
}

#[test]
fn chat_slots_render_the_attested_scope_in_each_transport_s_api_form() {
    use dekopon_broker_protocol::{
        ChatScopeClaim, ChatTransportKind, Conversation, ConversationKind, Trigger,
    };

    let scope =
        |kind, conversation_kind, container: Option<&str>, id: &str, thread: Option<&str>| {
            ChatScopeClaim {
                participant: None,
                github: None,
                transport: "family-chat".parse().expect("transport"),
                kind,
                conversation: Conversation {
                    kind: conversation_kind,
                    container: container.map(str::to_owned),
                    id: id.to_owned(),
                    thread: thread.map(str::to_owned),
                },
                delivery: None,
                trigger: Trigger::Message,
            }
        };
    let every_slot = HttpConstraints {
        allowed_hosts: vec!["chat.example:443".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        max_requests: 1,
        max_request_bytes: 1024,
        max_response_bytes: 1024,
        allow_plaintext_loopback: false,
        propagate_trace: false,
        signed_download: None,
        request_templates: vec![
            serde_json::from_value::<RequestTemplate>(serde_json::json!({
                "method": "GET",
                "path": "/{transport}/{conversation.apiChannel}",
                "query": {"pinned": {"c": "conversation.id", "t": "conversation.thread"}}
            }))
            .expect("template fixture"),
        ],
    };

    let discord_thread = scope(
        ChatTransportKind::Discord,
        ConversationKind::Thread,
        Some("111"),
        "222",
        Some("333"),
    );
    let values = render_chat_slots(Some(&every_slot), Some(&discord_thread)).expect("renders");
    assert_eq!(values.get(ChatSlot::ConversationApiChannel), Some("333"));
    assert_eq!(values.get(ChatSlot::ConversationId), Some("222"));
    assert_eq!(values.get(ChatSlot::ConversationThread), Some("333"));
    assert_eq!(values.get(ChatSlot::Transport), Some("family-chat"));

    let slack_thread = scope(
        ChatTransportKind::Slack,
        ConversationKind::Thread,
        Some("t0123abc"),
        "c0123abc",
        Some("1712345678.000100"),
    );
    let values = render_chat_slots(Some(&every_slot), Some(&slack_thread)).expect("renders");
    assert_eq!(
        values.get(ChatSlot::ConversationApiChannel),
        Some("C0123ABC")
    );
    assert_eq!(values.get(ChatSlot::ConversationId), Some("C0123ABC"));
    assert_eq!(
        values.get(ChatSlot::ConversationThread),
        Some("1712345678.000100")
    );

    let discord_channel = scope(
        ChatTransportKind::Discord,
        ConversationKind::Channel,
        Some("111"),
        "222",
        None,
    );
    assert_eq!(
        render_chat_slots(Some(&every_slot), Some(&discord_channel)),
        Err("request-template-slot-absent")
    );
    assert_eq!(
        render_chat_slots(Some(&every_slot), None),
        Err("chat-scope-required")
    );
    let untemplated = HttpConstraints {
        request_templates: Vec::new(),
        signed_download: None,
        ..every_slot
    };
    assert_eq!(
        render_chat_slots(Some(&untemplated), None),
        Ok(dekopon_capability::ChatSlotValues::default())
    );
}

fn storage_set(
    provider: &str,
    interface: StorageInterface,
    access: StorageAccess,
    scope: StorageScope,
    retention: dekopon_capability::StorageRetention,
    namespace: Option<&str>,
) -> ConstraintSet {
    ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: provider.parse().unwrap(),
        effect: match access {
            StorageAccess::ReadOnly => dekopon_capability::EffectKind::ReadOnly,
            StorageAccess::ReadWrite => dekopon_capability::EffectKind::LocalWrite,
        },
        risk: dekopon_core::RiskLevel::Low,
        constraints: ExecutionConstraints {
            storage: Some(StorageConstraints {
                interface,
                access,
                scope,
                retention,
                namespace: namespace.map(|name| name.parse().unwrap()),
            }),
            ..Default::default()
        },
    }
}

#[test]
fn a_shared_namespace_keys_retention_and_conflicting_retention_fails() {
    use super::{BrokerBuildError, ConstraintCatalog};
    use dekopon_capability::StorageRetention;
    let ttl = StorageRetention::IdleTtl(std::time::Duration::from_secs(20));
    let catalog = |python_retention| {
        ConstraintCatalog::new([
            (
                "turso.exec".parse().unwrap(),
                storage_set(
                    "turso",
                    StorageInterface::DurableFiles,
                    StorageAccess::ReadWrite,
                    StorageScope::Agent,
                    StorageRetention::Keep,
                    None,
                ),
            ),
            (
                "python.eval".parse().unwrap(),
                storage_set(
                    "python",
                    StorageInterface::DurableFiles,
                    StorageAccess::ReadOnly,
                    StorageScope::Agent,
                    python_retention,
                    Some("turso"),
                ),
            ),
        ])
        .unwrap()
    };
    let shared = catalog(StorageRetention::Keep);
    assert!(shared.validate_storage_namespaces().is_ok());
    assert_eq!(
        shared.retention_policies().unwrap(),
        dekopon_storage_host::RetentionPolicies::from([(
            ("turso".parse().unwrap(), StorageScope::Agent),
            StorageRetention::Keep
        )])
    );
    assert!(matches!(
        catalog(ttl).retention_policies(),
        Err(BrokerBuildError::ConflictingStorageRetention { namespace, scope: StorageScope::Agent })
            if namespace.as_str() == "turso"
    ));
}

#[test]
fn a_named_namespace_outside_agent_scope_and_every_interface_mismatch_are_refused_together() {
    use super::{BrokerBuildError, ConstraintCatalog, NamespaceConflict};
    let keep = dekopon_capability::StorageRetention::Keep;
    let set = |provider, interface, scope, namespace| {
        storage_set(
            provider,
            interface,
            StorageAccess::ReadOnly,
            scope,
            keep,
            namespace,
        )
    };
    let unchanged = ConstraintCatalog::new([
        (
            "turso.exec".parse().unwrap(),
            set(
                "turso",
                StorageInterface::DurableFiles,
                StorageScope::Agent,
                None,
            ),
        ),
        (
            "turso.log".parse().unwrap(),
            set("turso", StorageInterface::Jsonl, StorageScope::Agent, None),
        ),
    ])
    .unwrap();
    assert!(
        unchanged.validate_storage_namespaces().is_ok(),
        "configs that name no namespace keep today's checks"
    );

    let conflicting = ConstraintCatalog::new([
        (
            "turso.exec".parse().unwrap(),
            set(
                "turso",
                StorageInterface::DurableFiles,
                StorageScope::Agent,
                None,
            ),
        ),
        (
            "python.eval".parse().unwrap(),
            set(
                "python",
                StorageInterface::Jsonl,
                StorageScope::Agent,
                Some("turso"),
            ),
        ),
        (
            "memory.recent".parse().unwrap(),
            set(
                "memory",
                StorageInterface::Jsonl,
                StorageScope::Space,
                Some("chat"),
            ),
        ),
        (
            "chat.read".parse().unwrap(),
            set("chat", StorageInterface::Jsonl, StorageScope::Agent, None),
        ),
    ])
    .unwrap();
    let Err(BrokerBuildError::ConflictingStorageNamespaces { conflicts }) =
        conflicting.validate_storage_namespaces()
    else {
        panic!("expected every namespace conflict");
    };
    assert_eq!(
        conflicts,
        vec![
            NamespaceConflict::NotAgentScope {
                capability: "memory.recent".parse().unwrap(),
                namespace: "chat".parse().unwrap(),
                scope: StorageScope::Space,
            },
            NamespaceConflict::Mismatch {
                namespace: "turso".parse().unwrap(),
                capabilities: vec![
                    ("python.eval".parse().unwrap(), StorageInterface::Jsonl),
                    (
                        "turso.exec".parse().unwrap(),
                        StorageInterface::DurableFiles
                    ),
                ],
            },
        ]
    );
}

#[test]
fn naming_a_namespace_leaves_its_owners_conversation_scoped_sets_alone() {
    use super::ConstraintCatalog;
    let keep = dekopon_capability::StorageRetention::Keep;
    let catalog = ConstraintCatalog::new([
        (
            "turso.exec".parse().unwrap(),
            storage_set(
                "turso",
                StorageInterface::DurableFiles,
                StorageAccess::ReadWrite,
                StorageScope::Agent,
                keep,
                None,
            ),
        ),
        (
            "turso.notes".parse().unwrap(),
            storage_set(
                "turso",
                StorageInterface::Jsonl,
                StorageAccess::ReadWrite,
                StorageScope::PrivateConversation,
                keep,
                None,
            ),
        ),
        (
            "python.eval".parse().unwrap(),
            storage_set(
                "python",
                StorageInterface::DurableFiles,
                StorageAccess::ReadOnly,
                StorageScope::Agent,
                keep,
                Some("turso"),
            ),
        ),
    ])
    .unwrap();
    assert!(catalog.validate_storage_namespaces().is_ok());
}

#[test]
fn a_host_redirect_refusal_records_no_provider_exit_code() {
    use dekopon_capability::{EffectKind, InvocationOutcome};
    use dekopon_core::RiskLevel;
    use dekopon_test_support::CaptureLayer;
    use tracing_subscriber::prelude::*;

    let failure = BrokerHostError::HostCallRejected {
        provider: "gh".parse().unwrap(),
        capability: "gh.actions.logs.read".parse().unwrap(),
        reason: dekopon_broker_host::HostCallRejection::Http(
            dekopon_broker_host::PolicyViolation::RedirectRefused,
        ),
    };
    let principal: PrincipalId = "caller".parse().unwrap();
    let context = AuthenticatedContext::new(
        principal.clone(),
        Actor::Service {
            principal: principal.clone(),
        },
    )
    .unwrap();
    let set = ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: "gh".parse().unwrap(),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        constraints: ExecutionConstraints::default(),
    };
    let captured = CaptureLayer::workspace();
    let _subscriber = tracing_subscriber::registry()
        .with(captured.clone())
        .set_default();
    super::emit_audit_event(&super::execution_event(
        &context,
        &"invoke-redirect-refused".parse().unwrap(),
        "0000000000000000000000000000f1c7".parse().unwrap(),
        &"gh.actions.logs.read".parse().unwrap(),
        "decision-redirect-refused",
        &[],
        "policy-digest",
        None,
        &principal,
        &set,
        InvocationOutcome::Failed,
        Some(&failure),
        0,
        Some(public_host_error(&failure).to_owned()),
        provider_failure_detail(&failure),
        Vec::new(),
        None,
        None,
    ));
    let records = captured.events_where(
        "dekopon_broker::audit",
        &[
            ("audit.event", "broker.execution"),
            ("invocation.id", "invoke-redirect-refused"),
        ],
    );
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].field("process.exit.code"), None);
}

#[test]
fn terminal_redirect_refusal_keeps_its_safe_label_detail_and_exit_three() {
    let failure = BrokerHostError::HostCallRejected {
        provider: "gh".parse().unwrap(),
        capability: "gh.actions.logs.read".parse().unwrap(),
        reason: dekopon_broker_host::HostCallRejection::Http(
            dekopon_broker_host::PolicyViolation::RedirectRefused,
        ),
    };
    assert_eq!(public_host_error(&failure), "redirect-refused");
    assert_eq!(
        provider_exit_status(&failure).map(std::num::NonZeroU8::get),
        Some(3)
    );
    assert_eq!(
        provider_failure_detail(&failure),
        Some(ProviderFailureDetail::new(
            "redirect-refused",
            "signed download redirect refused"
        ))
    );
}

fn valid_memory_config() -> ChatMemoryConfig {
    ChatMemoryConfig {
        continuity_policy: dekopon_storage_host::ContinuityPolicy::Paranoid,
        enabled_agents: vec!["reviewer".parse().expect("agent")],
        max_lookback_turns: 200,
        max_recent_turns: 20,
        max_search_results: 20,
        max_query_bytes: 256,
        max_result_bytes: 65_536,
        max_turn_bytes: 32_768,
        compaction_target_bytes: 8_388_608,
        compaction_threshold_bytes: 12_582_912,
        continuity_migration: None,
    }
}

#[test]
fn chat_memory_config_refuses_each_invalid_limit() {
    use super::BrokerBuildError;
    use dekopon_storage_host::StorageLimits;

    let storage = StorageLimits::default();
    let valid = valid_memory_config();
    valid.validate(&storage).expect("the baseline is valid");
    let mut wide_query = valid.clone();
    wide_query.max_query_bytes = 100_000;
    wide_query
        .validate(&storage)
        .expect("a query below the ceiling is valid");

    type Break = fn(&mut ChatMemoryConfig, &mut StorageLimits);
    let breaks: &[(&str, Break)] = &[
        ("no enabled agent", |c, _| c.enabled_agents.clear()),
        ("duplicate agents", |c, _| {
            c.enabled_agents = vec!["reviewer".parse().unwrap(); 2];
        }),
        ("zero lookback", |c, _| c.max_lookback_turns = 0),
        ("zero recent", |c, _| c.max_recent_turns = 0),
        ("zero search results", |c, _| c.max_search_results = 0),
        ("zero query", |c, _| c.max_query_bytes = 0),
        ("zero result", |c, _| c.max_result_bytes = 0),
        ("zero turn", |c, _| c.max_turn_bytes = 0),
        ("zero target", |c, _| c.compaction_target_bytes = 0),
        ("zero threshold", |c, _| c.compaction_threshold_bytes = 0),
        ("recent beyond lookback", |c, _| c.max_recent_turns = 201),
        ("search beyond lookback", |c, _| c.max_search_results = 201),
        ("target above threshold", |c, _| {
            c.compaction_target_bytes = c.compaction_threshold_bytes + 1;
        }),
        ("threshold above the file ceiling", |c, s| {
            c.compaction_threshold_bytes = s.max_file_bytes + 1;
        }),
        ("turn below one line", |c, _| c.max_turn_bytes = 240),
        ("result below an empty history", |c, _| {
            c.max_result_bytes = 29
        }),
        ("lookback that cannot fit the target", |c, _| {
            c.max_lookback_turns = 1_000;
            c.max_recent_turns = 1;
            c.max_search_results = 1;
        }),
        ("read chunk above the call limit", |_, s| {
            s.max_read_bytes_per_call = 256 * 1024 - 1;
        }),
        ("turn above the write call limit", |c, s| {
            s.max_write_bytes_per_call = c.max_turn_bytes - 1;
        }),
        ("target above the write call limit", |c, s| {
            s.max_write_bytes_per_call = c.compaction_target_bytes - 1;
        }),
        ("threshold with append above the file ceiling", |c, s| {
            s.max_file_bytes = c.compaction_threshold_bytes + c.max_turn_bytes - 1;
        }),
        ("read budget above the invocation limit", |_, s| {
            s.max_read_bytes_per_invocation = 0;
        }),
        ("write budget above the invocation limit", |_, s| {
            s.max_write_bytes_per_invocation = 0;
        }),
        ("host calls above the invocation limit", |_, s| {
            s.max_host_calls_per_invocation = 0;
        }),
        ("no file slot", |_, s| s.max_files_per_namespace = 0),
        ("namespace too small for the headroom", |c, s| {
            s.max_namespace_bytes = c.compaction_threshold_bytes + c.max_turn_bytes;
        }),
        ("query above 256 KiB", |c, _| {
            c.max_query_bytes = 256 * 1024 + 1
        }),
        ("result above 1 MiB", |c, _| {
            c.max_result_bytes = 1024 * 1024 + 1
        }),
    ];
    for (name, apply) in breaks {
        let (mut config, mut limits) = (valid.clone(), storage.clone());
        apply(&mut config, &mut limits);
        assert!(
            matches!(
                config.validate(&limits),
                Err(BrokerBuildError::InvalidChatMemory)
            ),
            "{name}"
        );
    }
}

#[test]
fn chat_memory_is_enabled_only_for_its_listed_agents() {
    let config = valid_memory_config();
    assert!(config.enabled_for(&"reviewer".parse().expect("agent")));
    assert!(!config.enabled_for(&"stranger".parse().expect("agent")));
}

#[test]
fn round_up_rounds_a_value_to_the_next_multiple() {
    assert_eq!(super::round_up(300_000, 262_144).unwrap(), 524_288);
    assert_eq!(super::round_up(262_144, 262_144).unwrap(), 262_144);
}

#[test]
fn capability_routes_have_distinct_stable_names() {
    let names: std::collections::BTreeSet<_> = [
        CapabilityRoute::Generic,
        CapabilityRoute::ChatMemoryRecord,
        CapabilityRoute::ChatMemoryRecent,
        CapabilityRoute::ChatMemorySearch,
    ]
    .map(CapabilityRoute::as_str)
    .into();
    assert_eq!(names.len(), 4);
    assert!(!names.contains(""));
    assert!(!names.contains("xyzzy"));
}

#[test]
fn a_default_secret_use_field_deserializes_to_some() {
    let set = |extra: serde_json::Value| {
        let mut value = serde_json::to_value(storage_set(
            "storage-probe",
            StorageInterface::DurableFiles,
            StorageAccess::ReadOnly,
            StorageScope::Agent,
            dekopon_capability::StorageRetention::Keep,
            None,
        ))
        .expect("constraint set serializes");
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<ConstraintSet>(value).expect("constraint set")
    };
    assert_eq!(set(serde_json::json!({})).default_secret_use, None);
    let secret = set(serde_json::json!({
        "defaultSecretUse": {"drn": "drn:secret:api/token", "sink": "httpBearer"}
    }));
    assert_eq!(
        secret
            .default_secret_use
            .expect("a present field decodes")
            .drn
            .as_str(),
        "drn:secret:api/token"
    );
}

fn two_probe_sets() -> ConstraintCatalog {
    use dekopon_capability::StorageRetention;

    let set = |namespace: Option<&str>| {
        storage_set(
            "storage-probe",
            StorageInterface::DurableFiles,
            StorageAccess::ReadOnly,
            StorageScope::Agent,
            StorageRetention::Keep,
            namespace,
        )
    };
    ConstraintCatalog::new([
        ("probe.one".parse().unwrap(), set(None)),
        ("probe.two".parse().unwrap(), set(None)),
    ])
    .unwrap()
}

#[test]
fn a_constraint_catalog_reports_its_length() {
    let catalog = two_probe_sets();
    assert_eq!(catalog.len(), 2);
    assert!(!catalog.is_empty());
    assert_eq!(ConstraintCatalog::empty().len(), 0);
    assert!(ConstraintCatalog::empty().is_empty());
}

#[tokio::test]
async fn a_catalog_over_the_constraint_set_ceiling_is_refused() {
    use super::BrokerBuildError;

    let registry = dekopon_broker_host::BrokerProviderRegistry::load(
        [dekopon_test_support::provider_fixture(
            "cli-probe-provider.wasm",
        )],
        BrokerHostLimits::default(),
    )
    .await
    .expect("a real component loads");
    let error = two_probe_sets()
        .validate(&registry, 1)
        .expect_err("two sets exceed a ceiling of one");
    assert!(matches!(
        error,
        BrokerBuildError::TooManyConstraintSets {
            count: 2,
            maximum: 1
        }
    ));
}

#[test]
fn an_idle_ttl_beyond_u64_millis_is_refused() {
    use super::BrokerBuildError;
    use dekopon_capability::StorageRetention;

    let catalog = ConstraintCatalog::new([(
        "probe.vfs".parse().unwrap(),
        storage_set(
            "storage-probe",
            StorageInterface::DurableFiles,
            StorageAccess::ReadOnly,
            StorageScope::Agent,
            StorageRetention::IdleTtl(std::time::Duration::MAX),
            None,
        ),
    )])
    .unwrap();
    assert!(matches!(
        catalog.retention_policies(),
        Err(BrokerBuildError::InvalidStorageRetention)
    ));
}

#[test]
fn a_chat_route_needs_each_storage_attribute_to_match() {
    use super::{BrokerBuildError, RouteConflict};
    use dekopon_capability::StorageRetention;

    let route_set = |interface, scope, access| ConstraintSet {
        route: CapabilityRoute::ChatMemoryRecord,
        ..storage_set(
            "memory",
            interface,
            access,
            scope,
            StorageRetention::Keep,
            None,
        )
    };
    let catalog = |set| ConstraintCatalog::new([("memory.record".parse().unwrap(), set)]).unwrap();
    catalog(route_set(
        StorageInterface::Jsonl,
        StorageScope::PrivateConversation,
        StorageAccess::ReadWrite,
    ))
    .validate_routes()
    .expect("a fully matching chat route");
    for (what, set) in [
        (
            "interface",
            route_set(
                StorageInterface::DurableFiles,
                StorageScope::PrivateConversation,
                StorageAccess::ReadWrite,
            ),
        ),
        (
            "scope",
            route_set(
                StorageInterface::Jsonl,
                StorageScope::Agent,
                StorageAccess::ReadWrite,
            ),
        ),
        (
            "access",
            route_set(
                StorageInterface::Jsonl,
                StorageScope::PrivateConversation,
                StorageAccess::ReadOnly,
            ),
        ),
    ] {
        assert!(
            matches!(
                catalog(set).validate_routes(),
                Err(BrokerBuildError::ConflictingRoutes { conflicts })
                    if matches!(conflicts.as_slice(), [RouteConflict::MissingChatStorage { .. }])
            ),
            "{what}"
        );
    }
}

fn chat_scope(
    kind: dekopon_broker_protocol::ChatTransportKind,
    conversation_kind: dekopon_broker_protocol::ConversationKind,
    trigger: dekopon_broker_protocol::Trigger,
) -> dekopon_broker_protocol::ChatScopeClaim {
    dekopon_broker_protocol::ChatScopeClaim {
        participant: None,
        github: None,
        transport: "scientist-slack".parse().expect("transport"),
        kind,
        conversation: dekopon_broker_protocol::Conversation {
            kind: conversation_kind,
            container: Some("t0123abc".to_owned()),
            id: "c0123abc".to_owned(),
            thread: None,
        },
        delivery: None,
        trigger,
    }
}

fn attested_agent_context(
    subject: &str,
    scope: dekopon_broker_protocol::ChatScopeClaim,
) -> AuthenticatedContext {
    AuthenticatedContext::attested_chat(
        "maintainer".parse().expect("principal"),
        Actor::Agent {
            agent: "reviewer".parse().expect("agent"),
        },
        "gateway".parse().expect("via"),
        subject.parse().expect("subject"),
        scope,
    )
    .expect("attested context")
}

#[test]
fn each_effect_and_risk_level_encodes_to_its_own_authority_tag() {
    use dekopon_capability::EffectKind;
    use dekopon_core::RiskLevel;

    let effects = [
        EffectKind::ReadOnly,
        EffectKind::LocalWrite,
        EffectKind::ExternalWrite,
    ]
    .map(super::effect_tag);
    let risks = [
        RiskLevel::Low,
        RiskLevel::Medium,
        RiskLevel::High,
        RiskLevel::Critical,
    ]
    .map(super::risk_tag);
    assert_eq!(
        effects
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        effects.len()
    );
    assert_eq!(
        risks
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        risks.len()
    );
}

#[test]
fn a_secret_map_revision_must_be_nonempty_unpadded_control_free_and_bounded() {
    use super::{BrokerBuildError, SecretCatalog};

    let overlong = "r".repeat(1_000);
    for revision in [
        "",
        overlong.as_str(),
        " padded",
        "padded ",
        "bell\u{7}inside",
        "tab\tinside",
    ] {
        assert!(
            matches!(
                SecretCatalog::empty().with_authority_revision(revision.to_owned()),
                Err(BrokerBuildError::InvalidSecretMapRevision)
            ),
            "{revision:?}"
        );
    }
    let longest_accepted = "r".repeat(128);
    for revision in ["rev-1", longest_accepted.as_str()] {
        let catalog = SecretCatalog::empty()
            .with_authority_revision(revision.to_owned())
            .unwrap_or_else(|error| panic!("{revision:?}: {error}"));
        assert_eq!(catalog.authority_revision(), Some(revision));
    }
}

#[test]
fn an_attestor_grant_is_bounded() {
    use super::{AttestorGrant, BrokerBuildError};

    let scope: dekopon_core::SubjectScope = "drn:slack:*:t0123abc:*".parse().unwrap();
    let too_many = AttestorGrant {
        namespaces: Some(vec![scope.clone(); super::MAX_POLICY_SCOPE_ENTRIES + 5]),
    };
    assert!(matches!(
        too_many.validate(),
        Err(BrokerBuildError::InvalidAttestorScope { .. })
    ));
    AttestorGrant {
        namespaces: Some(vec![scope]),
    }
    .validate()
    .expect("one scope is within bounds");
}

#[test]
fn an_audit_error_names_its_category() {
    assert_eq!(AuditError::Full { maximum: 1 }.category(), "full");
}

#[test]
fn each_storage_host_error_maps_to_its_public_category() {
    use dekopon_storage_host::StorageHostError;

    for (source, expected) in [
        (StorageHostError::QuotaExceeded, "storage-quota"),
        (StorageHostError::Busy, "storage-busy"),
        (StorageHostError::Timeout, "storage-timeout"),
        (StorageHostError::corrupt("test"), "storage-corrupt"),
        (
            StorageHostError::CorruptLayout {
                path: "layout".into(),
            },
            "storage-corrupt",
        ),
        (StorageHostError::Io, "storage-io"),
    ] {
        assert_eq!(
            public_host_error(&BrokerHostError::Storage { source }),
            expected
        );
    }
}

#[test]
fn a_rendered_evidence_value_is_absent_when_it_is_an_empty_list_or_null() {
    assert_eq!(super::rendered(&Vec::<String>::new()), None);
    assert_eq!(super::rendered(&None::<u8>), None);
}

#[test]
fn the_outcome_evidence_digest_is_the_digest_of_the_outcome() {
    let invocation: InvocationId = "invoke-digest".parse().expect("invocation");
    let digest = super::outcome_evidence_digest(&invocation, "outcome", &"ok").expect("digest");
    assert!(digest.starts_with("sha256:") && digest.len() == "sha256:".len() + 64);
    assert_ne!(
        digest,
        super::outcome_evidence_digest(&invocation, "outcome", &"other").expect("digest")
    );
}

fn permit(principal: &str, action: &str, kind: &str, resource: &str) -> String {
    format!(
        r#"@id("{principal}-{action}") permit(principal == Dekopon::Principal::"{principal}",
            action == Dekopon::Action::"{action}", resource == Dekopon::{kind}::"{resource}");"#
    )
}

const SECRET_DRN: &str = "drn:secret:http-probe/token";

fn capability_policy(permitted: &[(&str, &str)]) -> String {
    permitted
        .iter()
        .map(|(capability, provider)| permit("maintainer", capability, "Provider", provider))
        .collect()
}

fn memory_route_sets() -> Vec<(CapabilityId, ConstraintSet)> {
    use dekopon_capability::EffectKind;
    use dekopon_core::RiskLevel;

    [
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
    .map(|(name, route, effect, risk, access)| {
        (
            name.parse().expect("capability"),
            ConstraintSet {
                route,
                provider: "memory".parse().expect("provider"),
                effect,
                risk,
                ..storage_set(
                    "memory",
                    StorageInterface::Jsonl,
                    access,
                    StorageScope::PrivateConversation,
                    Default::default(),
                    None,
                )
            },
        )
    })
    .collect()
}

fn http_probe_set() -> ConstraintSet {
    ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: "http-probe".parse().expect("provider"),
        effect: dekopon_capability::EffectKind::ReadOnly,
        risk: dekopon_core::RiskLevel::Low,
        constraints: ExecutionConstraints {
            http: Some(HttpConstraints {
                allowed_hosts: vec!["127.0.0.1:1".to_owned()],
                allowed_methods: vec!["GET".to_owned()],
                max_requests: 1,
                max_request_bytes: 4_096,
                max_response_bytes: 4_096,
                allow_plaintext_loopback: true,
                propagate_trace: false,
                request_templates: Vec::new(),
                signed_download: None,
            }),
            ..ExecutionConstraints::default()
        },
    }
}

fn cli_probe_set(
    effect: dekopon_capability::EffectKind,
    risk: dekopon_core::RiskLevel,
) -> ConstraintSet {
    ConstraintSet {
        default_secret_use: None,
        route: CapabilityRoute::Generic,
        provider: "cli-probe".parse().expect("provider"),
        effect,
        risk,
        constraints: ExecutionConstraints::default(),
    }
}

fn http_probe_binding() -> super::SecretUseBinding {
    super::SecretUseBinding {
        binding_id: "http-probe-token".to_owned(),
        secret: SECRET_DRN.parse().expect("secret"),
        capabilities: ["http-probe.fetch".parse().expect("capability")].into(),
        sink: dekopon_core::SecretSinkKind::HttpBearer,
        basic_username: None,
        allowed_hosts: vec!["127.0.0.1:1".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        allowed_paths: vec![dekopon_capability::HttpPathRule::Exact {
            path: "/api/v1/thing".to_owned(),
        }],
        allow_query: false,
        max_injections: 1,
    }
}

struct SurfaceBroker<'a> {
    root: &'a std::path::Path,
    policy: String,
    constraints: Vec<(CapabilityId, ConstraintSet)>,
    memory: bool,
    secret_binding: Option<Option<&'a str>>,
    identities: Vec<(&'a str, &'a str)>,
    storage: bool,
    storage_limits: dekopon_storage_host::StorageLimits,
    memory_config: ChatMemoryConfig,
    binding: super::SecretUseBinding,
    groups: Vec<(&'a str, &'a str)>,
}

impl<'a> SurfaceBroker<'a> {
    fn new(root: &'a std::path::Path, permitted: &[(&str, &str)]) -> Self {
        Self {
            root,
            policy: capability_policy(permitted),
            constraints: vec![
                (
                    "cli-probe.upper".parse().expect("capability"),
                    cli_probe_set(
                        dekopon_capability::EffectKind::ReadOnly,
                        dekopon_core::RiskLevel::Low,
                    ),
                ),
                (
                    "cli-probe.count".parse().expect("capability"),
                    cli_probe_set(
                        dekopon_capability::EffectKind::ReadOnly,
                        dekopon_core::RiskLevel::Low,
                    ),
                ),
                (
                    "http-probe.fetch".parse().expect("capability"),
                    http_probe_set(),
                ),
                (
                    "storage-probe.run".parse().expect("capability"),
                    ConstraintSet {
                        risk: dekopon_core::RiskLevel::Medium,
                        ..storage_set(
                            "storage-probe",
                            StorageInterface::DurableFiles,
                            StorageAccess::ReadWrite,
                            StorageScope::PrivateConversation,
                            Default::default(),
                            None,
                        )
                    },
                ),
            ],
            memory: false,
            secret_binding: None,
            identities: Vec::new(),
            storage: true,
            storage_limits: dekopon_storage_host::StorageLimits::default(),
            memory_config: valid_memory_config(),
            binding: http_probe_binding(),
            groups: Vec::new(),
        }
    }

    fn with_group_members(mut self, members: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        self.groups = members.into_iter().collect();
        self
    }

    fn with_policy(mut self, policy: &str) -> Self {
        self.policy.push_str(policy);
        self
    }

    fn with_storage_limits(mut self, limits: dekopon_storage_host::StorageLimits) -> Self {
        self.storage_limits = limits;
        self
    }

    fn with_memory_config(mut self, config: ChatMemoryConfig) -> Self {
        self.memory_config = config;
        self
    }

    fn with_binding(mut self, binding: super::SecretUseBinding) -> Self {
        self.binding = binding;
        self
    }

    fn with_identities(mut self, identities: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        self.identities = identities.into_iter().collect();
        self
    }

    fn with_memory(mut self) -> Self {
        self.constraints.extend(memory_route_sets());
        self.memory = true;
        self
    }

    fn with_secret(mut self, permitted: bool, revision: Option<&'a str>) -> Self {
        if permitted {
            self.policy
                .push_str(&permit("maintainer", "secret.use", "Secret", SECRET_DRN));
        }
        self.secret_binding = Some(revision);
        self
    }

    async fn build(self) -> Broker<InMemoryAuditLog> {
        self.try_build().await.expect("broker")
    }

    async fn try_build(self) -> Result<Broker<InMemoryAuditLog>, super::BrokerBuildError> {
        let _memoization = dekopon_broker_host::enable_test_component_memoization();
        let storage = self.storage.then(|| {
            dekopon_storage_host::StorageHost::open(
                self.root
                    .canonicalize()
                    .expect("canonical tempdir")
                    .join("storage"),
                self.storage_limits.clone(),
            )
            .expect("storage")
        });
        let registry = dekopon_broker_host::BrokerProviderRegistry::load_with_storage(
            [
                dekopon_test_support::provider_fixture("cli-probe-provider.wasm"),
                dekopon_test_support::provider_fixture("http-probe-provider.wasm"),
                dekopon_test_support::provider_fixture("storage-probe-provider.wasm"),
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixture/memory-smoke-probe-provider.wasm"),
            ],
            BrokerHostLimits::default(),
            storage,
        )
        .await
        .expect("fixtures load");
        let world = PolicyWorld::new(
            ["maintainer".parse().expect("principal")],
            registry
                .capabilities()
                .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
        )
        .expect("world")
        .with_secrets([SECRET_DRN.parse().expect("secret")])
        .with_group_members(self.groups.iter().map(|(principal, group)| {
            (
                principal.parse().expect("principal"),
                group.parse().expect("group"),
            )
        }));
        let mut broker = Broker::new(
            registry,
            "broker".parse().expect("principal"),
            PolicyEngine::new(&self.policy, &world).expect("policy"),
            ConstraintCatalog::new(self.constraints).expect("constraints"),
            IdentityDirectory::new(self.identities.iter().map(|(subject, principal)| {
                (
                    subject.parse().expect("subject"),
                    principal.parse().expect("principal"),
                )
            }))
            .expect("identities"),
            Arc::new(InMemoryAuditLog::new(8).expect("audit")),
            BrokerLimits::default(),
        )?;
        if self.memory {
            broker = broker.with_chat_memory(self.memory_config)?;
        }
        if let Some(revision) = self.secret_binding {
            let mut secrets =
                super::SecretCatalog::new(vec![self.binding], Arc::new(super::EmptySecretResolver))
                    .expect("binding");
            if let Some(revision) = revision {
                secrets = secrets
                    .with_authority_revision(revision.to_owned())
                    .expect("revision");
            }
            broker = broker.with_secret_catalog(secrets)?;
        }
        Ok(broker)
    }
}

fn surface(broker: &Broker<InMemoryAuditLog>, memory_route: bool) -> Vec<u8> {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};

    let context = attested_agent_context(
        "drn:slack:user:t0123abc:u9xyz",
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Message,
        ),
    );
    broker
        .authority_surface_v1(
            &context,
            StorageInterface::Jsonl,
            memory_route,
            &broker.current_operational().expect("operational"),
        )
        .expect("surface")
}

#[tokio::test]
async fn the_authority_generation_covers_exactly_the_effective_capabilities_and_changes_when_any_is_added_or_denied()
 {
    let dirs = [(); 6].map(|()| tempfile::tempdir().expect("tempdir"));
    let upper = [("cli-probe.upper", "cli-probe")];
    let broker = |index: usize, permitted: &[(&str, &str)]| {
        SurfaceBroker::new(dirs[index].path(), permitted)
    };

    let only_upper = surface(&broker(0, &upper).build().await, false);
    assert!(only_upper.len() > 1);

    let upper_and_count = surface(
        &broker(1, &[upper[0], ("cli-probe.count", "cli-probe")])
            .build()
            .await,
        false,
    );
    assert_ne!(
        upper_and_count, only_upper,
        "an added capability rotates the generation"
    );

    let mut wrong_provider = broker(2, &upper);
    wrong_provider.policy.push_str(&permit(
        "maintainer",
        "cli-probe.count",
        "Provider",
        "http-probe",
    ));
    assert_eq!(
        surface(&wrong_provider.build().await, false),
        only_upper,
        "a permit that does not name the capability's provider does not make it effective"
    );

    let memory = broker(
        3,
        &[
            upper[0],
            ("memory.chat.record", "memory"),
            ("memory.chat.recent", "memory"),
            ("memory.chat.search", "memory"),
        ],
    )
    .with_memory()
    .build()
    .await;
    assert_eq!(
        surface(&memory, false),
        only_upper,
        "memory routes stay out of a surface that does not serve them"
    );
    assert_ne!(surface(&memory, true), only_upper);

    let mut space_scoped = broker(4, &[upper[0], ("cli-probe.count", "cli-probe")]);
    space_scoped.constraints[1].1 = ConstraintSet {
        constraints: ExecutionConstraints {
            storage: Some(StorageConstraints {
                interface: StorageInterface::Jsonl,
                access: StorageAccess::ReadOnly,
                scope: StorageScope::Space,
                retention: Default::default(),
                namespace: None,
            }),
            ..ExecutionConstraints::default()
        },
        ..space_scoped.constraints[1].1.clone()
    };
    assert_eq!(
        surface(&space_scoped.build().await, false),
        only_upper,
        "a space-scoped capability is not effective without a space"
    );
}

#[tokio::test]
async fn the_authority_generation_changes_with_the_secret_bindings_and_map_revision() {
    let dirs = [(); 7].map(|()| tempfile::tempdir().expect("tempdir"));
    let fetch = [("http-probe.fetch", "http-probe")];
    let broker = |index: usize| SurfaceBroker::new(dirs[index].path(), &fetch);

    let unbound = surface(&broker(0).build().await, false);
    let first = surface(
        &broker(1).with_secret(true, Some("r1")).build().await,
        false,
    );
    let second = surface(
        &broker(2).with_secret(true, Some("r2")).build().await,
        false,
    );
    let unrevised = surface(&broker(3).with_secret(true, None).build().await, false);
    let denied = surface(
        &broker(4).with_secret(false, Some("r1")).build().await,
        false,
    );

    assert_ne!(
        first, unbound,
        "a usable secret binding is part of the authority"
    );
    assert_ne!(first, second, "the map revision is part of the authority");
    assert_ne!(first, unrevised);
    assert_eq!(
        denied, unbound,
        "a binding the policy does not permit changes no authority"
    );

    let none_effective = SurfaceBroker::new(dirs[5].path(), &[("cli-probe.upper", "cli-probe")])
        .with_secret(true, Some("r1"))
        .build()
        .await;
    assert_eq!(
        surface(&none_effective, false),
        surface(
            &SurfaceBroker::new(dirs[6].path(), &[("cli-probe.upper", "cli-probe")])
                .build()
                .await,
            false
        ),
        "a binding for a capability that is not effective changes no authority"
    );
}

#[test]
fn a_secret_binding_with_an_invalid_grant_is_refused_by_its_own_validation() {
    use dekopon_capability::SecretUseGrantError;

    http_probe_binding()
        .validate()
        .expect("the baseline binding is valid");
    let empty_scope = super::SecretUseBinding {
        allowed_hosts: Vec::new(),
        ..http_probe_binding()
    };
    assert_eq!(empty_scope.validate(), Err(SecretUseGrantError::EmptyScope));
}

#[test]
fn a_secret_catalog_lists_the_secrets_its_bindings_name() {
    let catalog = super::SecretCatalog::new(
        vec![http_probe_binding()],
        Arc::new(super::EmptySecretResolver),
    )
    .expect("catalog");
    assert_eq!(
        catalog.drns().map(|drn| drn.as_str()).collect::<Vec<_>>(),
        [SECRET_DRN]
    );
}

#[test]
fn secret_catalog_refuses_each_binding_that_exceeds_its_capability_http_constraint() {
    use super::{BrokerBuildError, SecretCatalog, SecretUseBinding};

    fn broker_error(binding: SecretUseBinding) -> Option<BrokerBuildError> {
        let constraints = ConstraintCatalog::new([
            (
                "http-probe.fetch".parse().expect("capability"),
                http_probe_set(),
            ),
            (
                "cli-probe.upper".parse().expect("capability"),
                cli_probe_set(
                    dekopon_capability::EffectKind::ReadOnly,
                    dekopon_core::RiskLevel::Low,
                ),
            ),
        ])
        .expect("constraints");
        SecretCatalog::new(vec![binding], Arc::new(super::EmptySecretResolver))
            .expect("a well-formed binding builds")
            .validate(&constraints)
            .err()
    }

    assert!(broker_error(http_probe_binding()).is_none());
    assert!(matches!(
        broker_error(SecretUseBinding {
            capabilities: ["missing.capability".parse().expect("capability")].into(),
            ..http_probe_binding()
        }),
        Some(BrokerBuildError::SecretBindingUnknownCapability { .. })
    ));
    assert!(matches!(
        broker_error(SecretUseBinding {
            capabilities: ["cli-probe.upper".parse().expect("capability")].into(),
            ..http_probe_binding()
        }),
        Some(BrokerBuildError::SecretBindingWithoutHttp { .. })
    ));
    for (name, binding) in [
        (
            "more injections than the capability may send requests",
            SecretUseBinding {
                max_injections: 2,
                ..http_probe_binding()
            },
        ),
        (
            "a host the capability does not allow",
            SecretUseBinding {
                allowed_hosts: vec!["127.0.0.1:2".to_owned()],
                ..http_probe_binding()
            },
        ),
        (
            "a method the capability does not allow",
            SecretUseBinding {
                allowed_methods: vec!["POST".to_owned()],
                ..http_probe_binding()
            },
        ),
    ] {
        assert!(
            matches!(
                broker_error(binding),
                Some(BrokerBuildError::SecretBindingExceedsCapability { .. })
            ),
            "{name}"
        );
    }
}

#[test]
fn validate_set_constraints_refuses_each_inconsistent_constraint_set() {
    use super::{BrokerBuildError, validate_set_constraints};
    use dekopon_capability::{AssetConstraints, EffectKind, SecretUseGrant};

    let valid = |set: &ConstraintSet| validate_set_constraints(set);
    let with = |constraints: ExecutionConstraints, effect| ConstraintSet {
        effect,
        constraints,
        ..cli_probe_set(EffectKind::ReadOnly, dekopon_core::RiskLevel::Low)
    };
    let storage = |access| StorageConstraints {
        interface: StorageInterface::Jsonl,
        access,
        scope: StorageScope::PrivateConversation,
        retention: Default::default(),
        namespace: None,
    };
    let asset = |attach, remove, send| {
        Some(AssetConstraints {
            attach,
            remove,
            send,
        })
    };
    let secret_use = SecretUseGrant {
        secret: SECRET_DRN.parse().expect("secret"),
        sink: dekopon_core::SecretSinkKind::HttpBearer,
        basic_username: None,
        allowed_hosts: vec!["127.0.0.1:1".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        allowed_paths: vec![dekopon_capability::HttpPathRule::Exact {
            path: "/".to_owned(),
        }],
        allow_query: false,
        max_injections: 1,
        binding_id: "binding".to_owned(),
        map_revision: None,
    };

    valid(&with(
        ExecutionConstraints {
            storage: Some(storage(StorageAccess::ReadWrite)),
            asset: asset(true, false, false),
            ..ExecutionConstraints::default()
        },
        EffectKind::LocalWrite,
    ))
    .expect("a consistent set is valid");

    let storage_and_http = ExecutionConstraints {
        storage: Some(storage(StorageAccess::ReadOnly)),
        ..http_probe_set().constraints
    };
    let rows: Vec<(&str, ConstraintSet)> = vec![
        (
            "a zero timeout",
            with(
                ExecutionConstraints {
                    timeout_ms: 0,
                    ..ExecutionConstraints::default()
                },
                EffectKind::ReadOnly,
            ),
        ),
        (
            "a resolved secret use",
            with(
                ExecutionConstraints {
                    secret_use: Some(secret_use),
                    ..ExecutionConstraints::default()
                },
                EffectKind::ReadOnly,
            ),
        ),
        (
            "http beside storage",
            with(storage_and_http, EffectKind::ReadOnly),
        ),
        (
            "write access under a read-only effect",
            with(
                ExecutionConstraints {
                    storage: Some(storage(StorageAccess::ReadWrite)),
                    ..ExecutionConstraints::default()
                },
                EffectKind::ReadOnly,
            ),
        ),
        (
            "an external write over storage",
            with(
                ExecutionConstraints {
                    storage: Some(storage(StorageAccess::ReadWrite)),
                    ..ExecutionConstraints::default()
                },
                EffectKind::ExternalWrite,
            ),
        ),
        (
            "send without an external write",
            with(
                ExecutionConstraints {
                    asset: asset(false, false, true),
                    ..ExecutionConstraints::default()
                },
                EffectKind::LocalWrite,
            ),
        ),
        (
            "attach under a read-only effect",
            with(
                ExecutionConstraints {
                    asset: asset(true, false, false),
                    ..ExecutionConstraints::default()
                },
                EffectKind::ReadOnly,
            ),
        ),
        (
            "remove under a read-only effect",
            with(
                ExecutionConstraints {
                    asset: asset(false, true, false),
                    ..ExecutionConstraints::default()
                },
                EffectKind::ReadOnly,
            ),
        ),
    ];
    for (name, set) in rows {
        assert!(
            matches!(valid(&set), Err(BrokerBuildError::InvalidPolicyConstraints)),
            "{name}"
        );
    }
}

fn invocation_with_input(input: serde_json::Value) -> dekopon_broker_protocol::InvocationRequest {
    dekopon_broker_protocol::InvocationRequest {
        id: "invoke-memory-input".parse().expect("invocation"),
        capability: "memory.chat.recent".parse().expect("capability"),
        trace_parent: "00-0000000000000000000000000000f1c7-00000000000000f1-00"
            .parse()
            .expect("traceparent"),
        input,
        secret_use: None,
    }
}

#[tokio::test]
async fn memory_inputs_outside_their_limits_or_with_extra_keys_are_refused() {
    use serde_json::json;

    let root = tempfile::tempdir().expect("tempdir");
    let broker = SurfaceBroker::new(root.path(), &[])
        .with_memory()
        .build()
        .await;
    let curate = |route, input| {
        let mut request = invocation_with_input(input);
        let outcome = broker.curate_memory_input(route, &mut request);
        (outcome, request.input)
    };
    let config = valid_memory_config();

    let (outcome, input) = curate(CapabilityRoute::ChatMemoryRecent, json!({"last": 3}));
    assert_eq!(outcome, Ok(()));
    assert_eq!(
        input,
        json!({
            "operation": "recent", "last": 3,
            "maxLookbackTurns": config.max_lookback_turns,
            "maxRecentTurns": config.max_recent_turns,
            "maxResultBytes": config.max_result_bytes,
        })
    );
    let (outcome, input) = curate(
        CapabilityRoute::ChatMemorySearch,
        json!({"query": "needle"}),
    );
    assert_eq!(outcome, Ok(()));
    assert_eq!(
        input,
        json!({
            "operation": "search", "query": "needle",
            "maxLookbackTurns": config.max_lookback_turns,
            "maxSearchResults": config.max_search_results,
            "maxResultBytes": config.max_result_bytes,
        })
    );

    let too_long = "q".repeat(usize::try_from(config.max_query_bytes).expect("query bytes") + 100);
    for (name, route, input) in [
        (
            "zero turns",
            CapabilityRoute::ChatMemoryRecent,
            json!({"last": 0}),
        ),
        (
            "more turns than the recent limit",
            CapabilityRoute::ChatMemoryRecent,
            json!({"last": u64::from(config.max_recent_turns) + 100}),
        ),
        (
            "a negative count",
            CapabilityRoute::ChatMemoryRecent,
            json!({"last": -1}),
        ),
        (
            "a missing count",
            CapabilityRoute::ChatMemoryRecent,
            json!({}),
        ),
        (
            "an extra key beside the count",
            CapabilityRoute::ChatMemoryRecent,
            json!({"last": 1, "extra": true}),
        ),
        (
            "an empty query",
            CapabilityRoute::ChatMemorySearch,
            json!({"query": ""}),
        ),
        (
            "a query beyond the byte limit",
            CapabilityRoute::ChatMemorySearch,
            json!({"query": too_long}),
        ),
        (
            "a non-text query",
            CapabilityRoute::ChatMemorySearch,
            json!({"query": 5}),
        ),
        (
            "an extra key beside the query",
            CapabilityRoute::ChatMemorySearch,
            json!({"query": "needle", "extra": true}),
        ),
        (
            "a generic route",
            CapabilityRoute::Generic,
            json!({"query": "needle"}),
        ),
        (
            "the record route",
            CapabilityRoute::ChatMemoryRecord,
            json!({"last": 1}),
        ),
    ] {
        assert_eq!(
            curate(route, input).0,
            Err("invalid-memory-input"),
            "{name}"
        );
    }

    let root = tempfile::tempdir().expect("tempdir");
    let without_memory = SurfaceBroker::new(root.path(), &[]).build().await;
    let mut request = invocation_with_input(json!({"last": 1}));
    assert_eq!(
        without_memory.curate_memory_input(CapabilityRoute::ChatMemoryRecent, &mut request),
        Err("memory-unavailable")
    );
}

#[test]
fn merge_space_decision_ands_each_verdict_and_unions_policy_ids() {
    use super::{DenyingPrincipal, SpaceDecision, merge_space_decision};

    let decision = |asker: bool, space: bool, ids: &[&str]| SpaceDecision {
        id: "space".parse().expect("principal"),
        asker_allowed: Some(asker),
        allowed: Some(space),
        policy_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        denied_by: None,
    };
    for (previous, next, asker, space, denied_by) in [
        ((true, true), (true, true), true, true, None),
        (
            (true, true),
            (false, true),
            false,
            true,
            Some(DenyingPrincipal::Asker),
        ),
        (
            (true, true),
            (true, false),
            true,
            false,
            Some(DenyingPrincipal::Space),
        ),
        (
            (false, true),
            (true, true),
            false,
            true,
            Some(DenyingPrincipal::Asker),
        ),
        (
            (true, false),
            (true, true),
            true,
            false,
            Some(DenyingPrincipal::Space),
        ),
        (
            (false, false),
            (true, true),
            false,
            false,
            Some(DenyingPrincipal::Both),
        ),
    ] {
        let mut merged = Some(decision(previous.0, previous.1, &["b", "a"]));
        merge_space_decision(&mut merged, Some(decision(next.0, next.1, &["c", "a"])));
        let merged = merged.expect("a decision");
        assert_eq!(
            (merged.asker_allowed, merged.allowed, merged.denied_by),
            (Some(asker), Some(space), denied_by),
            "{previous:?} then {next:?}"
        );
        assert_eq!(merged.policy_ids, ["a", "b", "c"]);
    }

    let mut kept = Some(decision(true, true, &["a"]));
    merge_space_decision(&mut kept, None);
    assert_eq!(kept, Some(decision(true, true, &["a"])));
}

#[test]
fn an_identity_directory_lists_its_principals_and_the_ceiling_is_the_longest() {
    let directory = IdentityDirectory::new([
        (
            "drn:slack:user:t0123abc:u1".parse().expect("subject"),
            "ann".parse().expect("principal"),
        ),
        (
            "drn:slack:user:t0123abc:u2".parse().expect("subject"),
            "maintainer".parse().expect("principal"),
        ),
    ])
    .expect("directory");
    let mut principals = directory
        .principals()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    principals.sort();
    assert_eq!(principals, ["ann", "maintainer"]);
}

#[tokio::test]
async fn the_capability_ceiling_lists_each_available_capability_with_its_words_and_help() {
    let root = tempfile::tempdir().expect("tempdir");
    let broker = SurfaceBroker::new(root.path(), &[])
        .with_memory()
        .with_identities([
            ("drn:slack:user:t0123abc:u1", "ann"),
            ("drn:slack:user:t0123abc:u2", "maintainer"),
        ])
        .build()
        .await;

    let (capabilities, words, help) = broker.capability_ceiling();
    let ids = capabilities
        .iter()
        .map(|available| available.capability.id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            "cli-probe.count",
            "cli-probe.upper",
            "http-probe.fetch",
            "memory.chat.recent",
            "memory.chat.record",
            "memory.chat.search",
            "storage-probe.run",
        ]
    );
    for available in &capabilities {
        let set = broker
            .constraints
            .get(&available.capability.id)
            .expect("constrained");
        assert_eq!(available.provider, set.provider);
        assert_eq!(available.capability.effect, set.effect);
        assert_eq!(available.capability.risk, set.risk);
    }
    assert!(words.iter().any(|word| word == "smokerecent"), "{words:?}");
    assert_eq!(
        help.keys().collect::<std::collections::BTreeSet<_>>(),
        words.iter().collect::<std::collections::BTreeSet<_>>()
    );
    assert!(help.values().all(|page| !page.is_empty()));

    assert_eq!(
        broker.principal_ceiling(),
        Some("maintainer".parse().expect("principal"))
    );
    assert_eq!(
        broker.display_principal(
            &dekopon_core::AttestedSubject::one(
                "drn:slack:user:t0123abc:u1".parse().expect("subject")
            )
            .expect("one form"),
        ),
        Some("ann".parse().expect("principal"))
    );
    let memory = broker.chat_memory_ceiling().expect("memory is configured");
    assert_eq!(
        memory.max_lookback_turns,
        valid_memory_config().max_lookback_turns
    );
    assert_eq!(memory.prompt_note, super::MEMORY_PROMPT_NOTE);
}

#[tokio::test]
async fn without_memory_or_identities_the_ceilings_are_absent() {
    let root = tempfile::tempdir().expect("tempdir");
    let broker = SurfaceBroker::new(root.path(), &[]).build().await;
    assert_eq!(broker.principal_ceiling(), None);
    assert_eq!(
        broker.display_principal(
            &dekopon_core::AttestedSubject::one(
                "drn:slack:user:t0123abc:u1".parse().expect("subject")
            )
            .expect("one form"),
        ),
        None
    );
    assert!(broker.chat_memory_ceiling().is_none());
}

#[tokio::test]
async fn a_chat_route_and_a_storage_constrained_capability_each_use_storage() {
    let root = tempfile::tempdir().expect("tempdir");
    let broker = SurfaceBroker::new(root.path(), &[])
        .with_memory()
        .build()
        .await;
    for (capability, uses_storage) in [
        ("memory.chat.recent", true),
        ("storage-probe.run", true),
        ("cli-probe.upper", false),
        ("http-probe.fetch", false),
        ("unconstrained.capability", false),
    ] {
        assert_eq!(
            broker.capability_uses_storage(&capability.parse().expect("capability")),
            uses_storage,
            "{capability}"
        );
    }
}

fn memory_policy() -> [(&'static str, &'static str); 3] {
    [
        ("memory.chat.record", "memory"),
        ("memory.chat.recent", "memory"),
        ("memory.chat.search", "memory"),
    ]
}

#[tokio::test]
async fn memory_surface_requires_each_of_its_conditions() {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};

    let dirs = [(); 2].map(|()| tempfile::tempdir().expect("tempdir"));
    let scope = || {
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Message,
        )
    };
    let broker = SurfaceBroker::new(dirs[0].path(), &memory_policy())
        .with_memory()
        .build()
        .await;
    let reviewer: AgentId = "reviewer".parse().expect("agent");
    let stranger: AgentId = "stranger".parse().expect("agent");
    let with_scope = attested_agent_context("drn:slack:user:t0123abc:u9xyz", scope());
    let without_scope = AuthenticatedContext::new(
        "maintainer".parse().expect("principal"),
        Actor::Agent {
            agent: reviewer.clone(),
        },
    )
    .expect("context");

    let surface = broker
        .memory_surface(&with_scope, &reviewer)
        .expect("an enabled agent in a chat scope with every route permitted");
    assert_eq!(
        surface.max_lookback_turns,
        valid_memory_config().max_lookback_turns
    );
    assert!(broker.memory_surface(&with_scope, &stranger).is_none());
    assert!(broker.memory_surface(&without_scope, &reviewer).is_none());

    let partial = SurfaceBroker::new(
        dirs[1].path(),
        &[
            ("memory.chat.record", "memory"),
            ("memory.chat.recent", "memory"),
        ],
    )
    .with_memory()
    .build()
    .await;
    assert!(
        partial.memory_surface(&with_scope, &reviewer).is_none(),
        "a surface needs all three routes permitted"
    );
}

#[tokio::test]
async fn reachable_providers_hides_storage_providers_outside_a_chat_scope() {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};

    let root = tempfile::tempdir().expect("tempdir");
    let broker = SurfaceBroker::new(root.path(), &[])
        .with_memory()
        .build()
        .await;
    let authorized = broker.constraints.iter().collect::<Vec<_>>();
    let providers = |context: &AuthenticatedContext| {
        broker
            .reachable_providers(context, &authorized)
            .into_iter()
            .map(|provider| provider.to_string())
            .collect::<Vec<_>>()
    };
    let chat = attested_agent_context(
        "drn:slack:user:t0123abc:u9xyz",
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Message,
        ),
    );
    let direct = AuthenticatedContext::new(
        "maintainer".parse().expect("principal"),
        Actor::Service {
            principal: "maintainer".parse().expect("principal"),
        },
    )
    .expect("context");

    assert_eq!(
        providers(&chat),
        ["cli-probe", "http-probe", "storage-probe"]
    );
    assert_eq!(providers(&direct), ["cli-probe", "http-probe"]);
}

fn entry_count(path: &std::path::Path) -> usize {
    std::fs::read_dir(path)
        .expect("directory")
        .map(|entry| {
            let path = entry.expect("entry").path();
            1 + if path.is_dir() { entry_count(&path) } else { 0 }
        })
        .sum()
}

#[tokio::test]
async fn a_private_conversation_namespace_rotates_with_authority_and_other_scopes_do_not() {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};

    let context = attested_agent_context(
        "drn:slack:user:t0123abc:u9xyz",
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Message,
        ),
    );
    let request = dekopon_broker_protocol::InvocationRequest {
        capability: "storage-probe.run".parse().expect("capability"),
        ..invocation_with_input(serde_json::json!({}))
    };
    for (scope, rotates) in [
        (StorageScope::PrivateConversation, true),
        (StorageScope::Agent, false),
    ] {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sizes = Vec::new();
        for permitted in [
            vec![("storage-probe.run", "storage-probe")],
            vec![
                ("storage-probe.run", "storage-probe"),
                ("cli-probe.upper", "cli-probe"),
            ],
        ] {
            let mut broker = SurfaceBroker::new(root.path(), &permitted);
            let run = broker
                .constraints
                .iter_mut()
                .find(|(capability, _)| capability.as_str() == "storage-probe.run")
                .expect("storage route");
            run.1.constraints.storage.as_mut().expect("storage").scope = scope;
            let broker = broker.build().await;
            let set = broker
                .constraints
                .get(&request.capability)
                .expect("constrained")
                .clone();
            let grant = broker
                .prepare_storage_grant(&context, &request, &set)
                .expect("grant")
                .expect("a storage capability prepares a grant")
                .materialize()
                .expect("materialized");
            drop(grant);
            drop(broker);
            sizes.push(entry_count(
                &root.path().canonicalize().expect("root").join("storage"),
            ));
        }
        assert_eq!(sizes[0] != sizes[1], rotates, "{scope:?}");
    }
}

fn secret_proposal() -> super::SecretUseProposal {
    super::SecretUseProposal::HttpBearer {
        secret: SECRET_DRN.parse().expect("secret"),
    }
}

#[tokio::test]
async fn a_refusal_from_authorization_carries_its_policy_evaluation() {
    use super::AttestorGrant;
    use dekopon_broker_protocol::Attestation;
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};

    let dirs = [(); 2].map(|()| tempfile::tempdir().expect("tempdir"));
    let scope = || {
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Message,
        )
    };
    let context = attested_agent_context("drn:slack:user:t0123abc:u9xyz", scope());

    let erroring_secret_policy = permit("maintainer", "secret.use", "Secret", SECRET_DRN)
        .replace(");", ") when { 9223372036854775807 + 1 > 0 };");
    let mut secret_broker =
        SurfaceBroker::new(dirs[0].path(), &[("http-probe.fetch", "http-probe")])
            .with_secret(false, None);
    secret_broker.policy.push_str(&erroring_secret_policy);
    let secret_broker = secret_broker.build().await;
    let capability = "http-probe.fetch".parse().expect("capability");
    let mut decision = secret_broker
        .authorize_invocation(&context, &capability)
        .ok()
        .expect("the capability is permitted");
    assert!(!decision.errors_present);
    let refusal = secret_broker
        .bind_secret_use(
            &context,
            &capability,
            &mut decision,
            Some(&secret_proposal()),
        )
        .expect_err("the secret policy errors, so the use is denied");
    assert_eq!(refusal.reason, "secret-denied");
    let evaluation = refusal
        .evaluation
        .expect("a secret denial carries its evaluation");
    assert!(evaluation.errors_present);
    assert!(
        decision.errors_present,
        "a secret policy error is remembered on the capability decision"
    );

    let identities = [("drn:slack:user:t0123abc:u9xyz", "maintainer")];
    let agents = SurfaceBroker::new(dirs[1].path(), &[])
        .with_identities(identities)
        .build()
        .await;
    let gateway = AuthenticatedContext::new(
        "gateway".parse().expect("principal"),
        Actor::Service {
            principal: "gateway".parse().expect("principal"),
        },
    )
    .expect("gateway");
    let grant = AttestorGrant { namespaces: None };
    let mut thread_scope = scope();
    thread_scope.conversation.kind = ConversationKind::Thread;
    thread_scope.conversation.thread = Some("1712345678.000100".to_owned());
    let claim = Attestation::for_chat(
        dekopon_core::AttestedSubject::one(
            "drn:slack:user:t0123abc:u9xyz".parse().expect("subject"),
        )
        .expect("subject"),
        "reviewer".parse().expect("agent"),
        thread_scope,
    );
    let (_, refusal) = agents.resolve_context(&gateway, Some(&grant), &claim);
    let refusal = refusal.expect("no policy lets the maintainer prompt the agent");
    assert_eq!(refusal.reason, "agent-denied");
    assert!(refusal.evaluation.is_some());
}

fn only_cli_probe(mut broker: SurfaceBroker<'_>) -> SurfaceBroker<'_> {
    broker
        .constraints
        .retain(|(capability, _)| capability.as_str().starts_with("cli-probe."));
    broker
}

#[tokio::test]
async fn a_denial_digests_the_same_with_or_without_a_storage_host_unless_the_capability_stores() {
    let dirs = [(); 2].map(|()| tempfile::tempdir().expect("tempdir"));
    let context = AuthenticatedContext::new(
        "maintainer".parse().expect("principal"),
        Actor::Service {
            principal: "maintainer".parse().expect("principal"),
        },
    )
    .expect("context");
    let request = dekopon_broker_protocol::InvocationRequest {
        capability: "cli-probe.upper".parse().expect("capability"),
        ..invocation_with_input(serde_json::json!({"text": "hi"}))
    };
    let mut digests = Vec::new();
    for with_storage in [true, false] {
        let mut broker = only_cli_probe(SurfaceBroker::new(
            dirs[usize::from(with_storage)].path(),
            &[],
        ));
        broker.storage = with_storage;
        let broker = broker.build().await;
        let result = broker
            .deny(
                &context,
                &request,
                super::unevaluated_refusal("policy-denied"),
            )
            .await
            .expect("a denial is recorded");
        assert_eq!(
            result.outcome,
            dekopon_capability::InvocationOutcome::Denied
        );
        digests.push(result.evidence[0].digest.clone());
    }
    assert_eq!(digests[0], digests[1]);
}

#[tokio::test]
async fn an_authorized_failure_before_the_provider_names_the_loaded_artifact() {
    use dekopon_capability::{Evidence, InvocationOutcome};

    let root = tempfile::tempdir().expect("tempdir");
    let broker = only_cli_probe(SurfaceBroker::new(
        root.path(),
        &[("cli-probe.upper", "cli-probe")],
    ))
    .build()
    .await;
    let context = attested_agent_context(
        "drn:slack:user:t0123abc:u9xyz",
        chat_scope(
            dekopon_broker_protocol::ChatTransportKind::Slack,
            dekopon_broker_protocol::ConversationKind::Channel,
            dekopon_broker_protocol::Trigger::Message,
        ),
    );
    let provider: ProviderId = "cli-probe".parse().expect("provider");
    let artifact = broker
        .registry
        .loaded_provider_metadata()
        .find(|metadata| metadata.manifest.id == provider)
        .map(|metadata| metadata.artifact_sha256)
        .expect("the cli-probe artifact is loaded");
    let artifacts = |broker: &Broker<InMemoryAuditLog>| {
        broker
            .audit
            .records()
            .into_iter()
            .filter_map(|event| match event {
                AuditEvent::Execution {
                    provider_artifact_sha256,
                    outcome,
                    ..
                } => Some((outcome, provider_artifact_sha256)),
                AuditEvent::Decision { .. } => None,
            })
            .collect::<Vec<_>>()
    };

    let capability: CapabilityId = "cli-probe.upper".parse().expect("capability");
    let set = broker
        .constraints
        .get(&capability)
        .expect("constrained")
        .clone();
    let invocation: InvocationId = "invoke-failed-early".parse().expect("invocation");
    broker
        .fail_authorized_before_provider(
            &context,
            &invocation,
            "0000000000000000000000000000f1c7".parse().expect("trace"),
            &capability,
            "decision-early",
            broker.decision_reference("decision-early"),
            &set,
            &[],
            Evidence {
                kind: "policy-decision".to_owned(),
                digest: "sha256:early".to_owned(),
                media_type: "application/json".to_owned(),
                uri: None,
            },
            "storage-io",
        )
        .await
        .expect("the failure is recorded");
    assert_eq!(
        artifacts(&broker),
        [(InvocationOutcome::Failed, Some(artifact))]
    );
}

#[tokio::test]
async fn a_successful_provider_that_wrote_stderr_carries_it_as_detail() {
    use dekopon_broker_host::BrokerProviderRegistry;
    use dekopon_capability::{EffectKind, InvocationOutcome};
    use dekopon_core::RiskLevel;

    let provider = provider_succeeding_after_writing_stderr();
    let registry = BrokerProviderRegistry::load([provider.path()], BrokerHostLimits::default())
        .await
        .expect("stderr-writing provider loads");
    let principal: PrincipalId = "caller".parse().unwrap();
    let capability: CapabilityId = "stdio-probe.run".parse().unwrap();
    let provider_id: ProviderId = "stdio-probe".parse().unwrap();
    let world = PolicyWorld::new(
        [principal.clone()],
        [(capability.clone(), provider_id.clone())],
    )
    .unwrap();
    let broker = Broker::new(
        registry,
        "broker".parse().unwrap(),
        PolicyEngine::new(
            r#"@id("stderr") permit(principal, action, resource);"#,
            &world,
        )
        .unwrap(),
        ConstraintCatalog::new([(
            capability.clone(),
            ConstraintSet {
                default_secret_use: None,
                route: CapabilityRoute::Generic,
                provider: provider_id,
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                constraints: ExecutionConstraints::default(),
            },
        )])
        .unwrap(),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(4).unwrap()),
        BrokerLimits::default(),
    )
    .unwrap();
    let context = AuthenticatedContext::attested(
        principal,
        Actor::Agent {
            agent: "provider-test".parse().unwrap(),
        },
        "gateway".parse().unwrap(),
        "drn:slack:user:team:user".parse().unwrap(),
    )
    .unwrap();
    let result = broker
        .invoke(
            &context,
            None,
            None,
            invocation_with_capability("invoke-stderr", capability),
            Default::default(),
        )
        .await
        .unwrap()
        .result;
    assert_eq!(result.outcome, InvocationOutcome::Succeeded, "{result:?}");
    let detail = result.detail.expect("stderr is kept as detail");
    assert_eq!(detail.code, "provider-stderr");
    assert_eq!(detail.message, "warn\n");
}

fn invocation_with_capability(
    id: &str,
    capability: CapabilityId,
) -> dekopon_broker_protocol::InvocationRequest {
    dekopon_broker_protocol::InvocationRequest {
        id: id.parse().expect("invocation"),
        capability,
        ..invocation_with_input(serde_json::json!({}))
    }
}

fn stdout_assets() -> (
    dekopon_broker_host::asset::AssetInputs,
    std::thread::JoinHandle<Vec<u8>>,
) {
    let (host, mut reader) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    let captured = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut bytes).expect("stdout");
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

#[tokio::test]
async fn an_http_request_decision_digests_its_own_verdict_and_denial_reason() {
    use super::{AuditEvent, DecisionMaterial};
    use dekopon_test_support::LoopbackServer;

    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
    );
    let authority = server.authority().to_owned();
    let root = tempfile::tempdir().expect("tempdir");
    let mut builder = SurfaceBroker::new(root.path(), &[("http-probe.fetch", "http-probe")]);
    builder.policy.push_str(
        r#"@id("only-ok-path") forbid(principal, action == Dekopon::Action::"http-probe.fetch", resource)
            when { context has request } unless { context.request.url.path == "/ok" };"#,
    );
    builder
        .constraints
        .iter_mut()
        .find(|(capability, _)| capability.as_str() == "http-probe.fetch")
        .expect("http route")
        .1
        .constraints
        .http
        .as_mut()
        .expect("http constraints")
        .allowed_hosts = vec![authority.clone(), "127.0.0.1:1".to_owned()];
    let broker = builder.build().await;
    let context = attested_agent_context(
        "drn:slack:user:t0123abc:u9xyz",
        chat_scope(
            dekopon_broker_protocol::ChatTransportKind::Slack,
            dekopon_broker_protocol::ConversationKind::Channel,
            dekopon_broker_protocol::Trigger::Message,
        ),
    );
    for (id, uri) in [
        ("invoke-http-ok", format!("http://{authority}/ok")),
        ("invoke-http-denied", "http://127.0.0.1:1/denied".to_owned()),
    ] {
        let (assets, captured) = stdout_assets();
        broker
            .invoke(
                &context,
                None,
                None,
                dekopon_broker_protocol::InvocationRequest {
                    input: serde_json::json!({"uri": uri, "method": "GET"}),
                    ..invocation_with_capability(
                        id,
                        "http-probe.fetch".parse().expect("capability"),
                    )
                },
                assets,
            )
            .await
            .expect("the invocation is accounted");
        captured.join().expect("stdout reader");
    }

    let decisions = broker
        .audit
        .records()
        .into_iter()
        .filter_map(|event| match event {
            AuditEvent::Decision {
                invocation,
                trace,
                principal: Some(principal),
                actor: Some(actor),
                via,
                attested_subject,
                capability,
                provider,
                authorized_by: Some(authorized_by),
                policy_ids,
                decision_point,
                policy_digest: Some(policy_digest),
                allowed,
                reason,
                decision_digest,
                ..
            } if decision_point == "http.request" => {
                let material = DecisionMaterial {
                    invocation: &invocation,
                    trace,
                    principal: &principal,
                    actor: &actor,
                    via: via.as_ref(),
                    attested_subject: attested_subject.as_ref(),
                    capability: &capability,
                    secret_use: None,
                    provider: provider.as_ref(),
                    authorized_by: &authorized_by,
                    policy_ids: &policy_ids,
                    policy_digest: &policy_digest,
                    constraints: None,
                    allowed,
                    reason: reason.as_deref(),
                    space: None,
                };
                let expected =
                    super::decision_evidence_digest("policy-decision", &material).expect("digest");
                Some((
                    invocation.to_string(),
                    allowed,
                    reason,
                    decision_digest == expected,
                ))
            }
            AuditEvent::Decision { .. } | AuditEvent::Execution { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        decisions,
        [
            ("invoke-http-ok".to_owned(), true, None, true),
            (
                "invoke-http-denied".to_owned(),
                false,
                Some("policy-denied".to_owned()),
                true
            ),
        ]
    );
}

#[tokio::test]
async fn with_chat_memory_refuses_routes_whose_matching_risk_is_not_the_fixed_risk() {
    use super::BrokerBuildError;
    use dekopon_capability::EffectKind;
    use dekopon_core::RiskLevel;

    let capability = |id: &str, effect: EffectKind, risk: RiskLevel| {
        serde_json::json!({
            "id": id,
            "description": "Declared memory route",
            "effect": effect,
            "risk": risk,
            "inputSchema": {"type": "object"}
        })
    };
    let manifest = serde_json::to_string(&serde_json::json!({
        "apiVersion": "dekopon.dev/provider/v1alpha1",
        "id": "memory",
        "description": "Memory routes declaring a risk the broker does not accept",
        "commandWords": ["memrisk"],
        "capabilities": [
            capability("memory.chat.record", EffectKind::LocalWrite, RiskLevel::Medium),
            capability("memory.chat.recent", EffectKind::ReadOnly, RiskLevel::Low),
            capability("memory.chat.search", EffectKind::ReadOnly, RiskLevel::High),
        ]
    }))
    .unwrap();
    let provider = inline_provider_declaring(&manifest);
    let root = tempfile::tempdir().expect("tempdir");
    let storage = dekopon_storage_host::StorageHost::open(
        root.path().canonicalize().expect("root").join("storage"),
        dekopon_storage_host::StorageLimits::default(),
    )
    .expect("storage");
    let registry = dekopon_broker_host::BrokerProviderRegistry::load_with_storage(
        [provider.path()],
        BrokerHostLimits::default(),
        Some(storage),
    )
    .await
    .expect("the component loads");
    let world = PolicyWorld::new(
        ["maintainer".parse().expect("principal")],
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .expect("world");
    let mut routes = memory_route_sets();
    let recent = routes
        .iter_mut()
        .find(|(capability, _)| capability.as_str() == "memory.chat.recent")
        .expect("recent route");
    recent.1.risk = RiskLevel::Low;
    let broker = Broker::new(
        registry,
        "broker".parse().expect("principal"),
        PolicyEngine::new("", &world).expect("policy"),
        ConstraintCatalog::new(routes).expect("constraints"),
        IdentityDirectory::empty(),
        Arc::new(InMemoryAuditLog::new(4).expect("audit")),
        BrokerLimits::default(),
    )
    .expect("constraints that match the declared risk start");

    assert!(matches!(
        broker.with_chat_memory(valid_memory_config()),
        Err(BrokerBuildError::InvalidChatMemory)
    ));
}

const MEMBER: &str = "drn:slack:user:t0123abc:u9xyz";

fn member_context() -> AuthenticatedContext {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};

    attested_agent_context(
        MEMBER,
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Message,
        ),
    )
}

fn memory_broker(root: &std::path::Path) -> SurfaceBroker<'_> {
    SurfaceBroker::new(root, &memory_policy())
        .with_memory()
        .with_identities([(MEMBER, "maintainer")])
}

fn paranoid_surface(broker: &Broker<InMemoryAuditLog>) -> Vec<u8> {
    broker
        .memory_surface_v2(&member_context())
        .expect("paranoid surface")
}

fn memory_handle(
    broker: &Broker<InMemoryAuditLog>,
    capability: &str,
) -> Result<dekopon_storage_host::StorageHandle, super::BrokerError> {
    memory_handle_in(broker, &member_context(), capability)
}

fn memory_handle_in(
    broker: &Broker<InMemoryAuditLog>,
    context: &AuthenticatedContext,
    capability: &str,
) -> Result<dekopon_storage_host::StorageHandle, super::BrokerError> {
    let request = dekopon_broker_protocol::InvocationRequest {
        capability: capability.parse().expect("capability"),
        ..invocation_with_input(serde_json::json!({}))
    };
    let set = broker
        .constraints
        .get(&request.capability)
        .expect("constrained")
        .clone();
    let grant = broker
        .prepare_storage_grant(context, &request, &set)?
        .expect("a memory route prepares a grant")
        .materialize()
        .map_err(|source| super::BrokerError::Storage { source })?;
    Ok(broker
        .registry
        .storage_host()
        .expect("storage")
        .begin(grant)
        .expect("transaction"))
}

fn remembered_bytes(broker: &Broker<InMemoryAuditLog>) -> Result<u64, super::BrokerError> {
    let mut handle = memory_handle(broker, "memory.chat.recent")?;
    let size = handle.jsonl_size("turns.jsonl").unwrap_or(0);
    handle.finish_read().expect("finish read");
    Ok(size)
}

fn remember(broker: &Broker<InMemoryAuditLog>, line: &[u8]) {
    let mut handle = memory_handle(broker, "memory.chat.record").expect("record grant");
    let offset = handle.jsonl_size("turns.jsonl").unwrap_or(0);
    handle
        .jsonl_append("turns.jsonl", offset, line)
        .expect("append");
    handle.commit().expect("commit");
}

fn remember_as_released_broker(broker: &Broker<InMemoryAuditLog>, line: &[u8]) {
    let context = member_context();
    let capability: CapabilityId = "memory.chat.record".parse().expect("capability");
    let set = broker.constraints.get(&capability).expect("record route");
    let storage = set.constraints.storage.as_ref().expect("storage route");
    let scope = context.chat_scope().expect("chat scope");
    let surface = broker
        .authority_surface_v1(
            &context,
            storage.interface,
            true,
            &broker.current_operational().expect("operational"),
        )
        .expect("v1 surface");
    let request = dekopon_storage_host::StorageGrantRequest::new(
        "released-record".parse().expect("invocation"),
        capability.clone(),
        set.provider.clone(),
        storage.interface,
        storage.access,
        storage.scope,
        "reviewer".parse().expect("agent"),
        context.attested_subject().expect("subject").clone(),
        scope.kind.to_string(),
        scope.transport.to_string(),
        scope.conversation.id.clone(),
        scope.conversation.key(),
        dekopon_storage_host::ContinuityPolicy::Paranoid,
        surface,
    )
    .with_shared_namespace(storage.namespace.clone())
    .with_space_key(context.space_binding().map(|binding| binding.key));
    let host = broker.registry.storage_host().expect("storage");
    let mut handle = host
        .begin(host.grant(request).expect("released grant"))
        .expect("released transaction");
    handle
        .jsonl_append("turns.jsonl", 0, line)
        .expect("released append");
    handle.commit().expect("released commit");
}

fn unproven(result: Result<u64, super::BrokerError>) -> dekopon_storage_host::ContinuityCheck {
    match result {
        Err(super::BrokerError::Storage {
            source: dekopon_storage_host::StorageHostError::ContinuityUnproven { check },
        }) => check,
        other => panic!("expected an unproven continuity refusal, got {other:?}"),
    }
}

fn migration(
    authority: &super::ContinuityDigest,
    resolved: &super::ContinuityDigest,
    overrides: super::OperationalOverrides,
) -> super::ContinuityMigration {
    super::ContinuityMigration {
        authority_digest: Some(authority.clone()),
        candidates: vec![super::ContinuityCandidate {
            resolved_digest: resolved.clone(),
            overrides,
        }],
        ..super::ContinuityMigration::default()
    }
}

fn report(broker: &Broker<InMemoryAuditLog>) -> super::ContinuityReport {
    broker.continuity_report().expect("memory report").clone()
}

#[tokio::test]
async fn the_paranoid_fingerprint_ignores_operational_artifact_and_unrelated_changes() {
    let dirs = [(); 6].map(|()| tempfile::tempdir().expect("tempdir"));
    let fetch = [("http-probe.fetch", "http-probe")];
    let permitted = [memory_policy().as_slice(), &fetch].concat();
    let base = |index: usize| {
        SurfaceBroker::new(dirs[index].path(), &permitted)
            .with_memory()
            .with_identities([(MEMBER, "maintainer")])
            .with_secret(true, Some("revision-a"))
    };
    let baseline = paranoid_surface(&base(0).build().await);

    let operational = base(1)
        .with_storage_limits(dekopon_storage_host::StorageLimits {
            max_namespaces: 500,
            max_root_bytes: 1 << 28,
            ..dekopon_storage_host::StorageLimits::default()
        })
        .with_memory_config(ChatMemoryConfig {
            max_recent_turns: 10,
            ..valid_memory_config()
        })
        .build()
        .await;
    let unrelated_policy = base(2)
        .with_policy(
            r#"@id("other-agent") forbid(principal, action, resource)
            when { context has agent && context.agent == "someone-else" };"#,
        )
        .build()
        .await;
    let reordered = base(3)
        .with_binding(super::SecretUseBinding {
            binding_id: "renamed-binding".to_owned(),
            ..http_probe_binding()
        })
        .with_secret(false, Some("revision-b"))
        .build()
        .await;
    for (change, broker) in [
        ("storage, memory and namespace tuning", operational),
        ("another principal's policy", unrelated_policy),
        ("binding label and map revision", reordered),
    ] {
        assert_eq!(
            paranoid_surface(&broker),
            baseline,
            "{change} rotated memory"
        );
    }
    let mut timeouts = base(4);
    for (_, set) in &mut timeouts.constraints {
        set.constraints.timeout_ms -= 1;
        if let Some(http) = set.constraints.http.as_mut() {
            http.max_response_bytes -= 1;
            http.allowed_hosts.reverse();
        }
    }
    assert_eq!(paranoid_surface(&timeouts.build().await), baseline);
}

#[tokio::test]
async fn the_paranoid_fingerprint_follows_effective_authority() {
    let dirs = [(); 7].map(|()| tempfile::tempdir().expect("tempdir"));
    let fetch = [("http-probe.fetch", "http-probe")];
    let permitted = [memory_policy().as_slice(), &fetch].concat();
    let base = |index: usize| {
        SurfaceBroker::new(dirs[index].path(), &permitted)
            .with_memory()
            .with_identities([(MEMBER, "maintainer")])
            .with_secret(true, None)
    };
    let baseline = paranoid_surface(&base(0).build().await);

    let removed = SurfaceBroker::new(dirs[1].path(), &memory_policy())
        .with_memory()
        .with_identities([(MEMBER, "maintainer")])
        .with_secret(true, None)
        .build()
        .await;
    let mut narrowed = base(2);
    for (_, set) in &mut narrowed.constraints {
        if let Some(http) = set.constraints.http.as_mut() {
            http.allow_plaintext_loopback = false;
        }
    }
    let rebound = base(3)
        .with_binding(super::SecretUseBinding {
            allowed_paths: vec![dekopon_capability::HttpPathRule::Exact {
                path: "/api/v1/other".to_owned(),
            }],
            ..http_probe_binding()
        })
        .build()
        .await;
    let path_fence = base(4)
        .with_policy(
            r#"@id("path-fence") forbid(principal, action, resource)
            when { context has request }
            when { context.request.url.path == "/api/v1/thing" };"#,
        )
        .build()
        .await;
    let risk_fence = |index: usize, risk: &str| {
        base(index).with_policy(&format!(
            r#"@id("risk-fence") forbid(principal, action, resource)
            when {{ context has request }}
            when {{ context.risk == "{risk}" }};"#
        ))
    };
    let inapplicable = risk_fence(5, "High").build().await;
    let applicable = risk_fence(6, "Low").build().await;
    for (change, broker) in [
        ("a removed capability", &removed),
        ("plaintext loopback withdrawn", &narrowed.build().await),
        ("a changed secret path", &rebound),
        ("a request-only path forbid", &path_fence),
    ] {
        assert_ne!(
            paranoid_surface(broker),
            baseline,
            "{change} did not rotate"
        );
    }
    assert_eq!(
        paranoid_surface(&inapplicable),
        baseline,
        "a risk fence that cannot apply"
    );
    assert_ne!(paranoid_surface(&applicable), baseline);
}

#[tokio::test]
async fn a_released_memory_pointer_migrates_under_unchanged_authority_in_both_legacy_states() {
    use dekopon_storage_host::StorageLimits;

    let raised = StorageLimits {
        max_namespaces: 500,
        ..StorageLimits::default()
    };
    for (written_under, overrides) in [
        (raised.clone(), super::OperationalOverrides::default()),
        (
            StorageLimits {
                max_namespaces: 64,
                ..raised.clone()
            },
            super::OperationalOverrides {
                storage: super::StorageOverrides {
                    max_namespaces: Some(64),
                    ..super::StorageOverrides::default()
                },
                ..super::OperationalOverrides::default()
            },
        ),
    ] {
        let root = tempfile::tempdir().expect("tempdir");
        let released = memory_broker(root.path())
            .with_storage_limits(written_under)
            .build()
            .await;
        remember_as_released_broker(&released, br#"{"turn":"released"}"#);
        drop(released);

        let probe = memory_broker(root.path())
            .with_storage_limits(raised.clone())
            .with_memory_config(ChatMemoryConfig {
                continuity_migration: Some(migration(
                    &report(
                        &memory_broker(tempfile::tempdir().expect("t").path())
                            .build()
                            .await,
                    )
                    .legacy_authority_digest,
                    &report(
                        &memory_broker(tempfile::tempdir().expect("t").path())
                            .build()
                            .await,
                    )
                    .current_resolved_digest,
                    overrides.clone(),
                )),
                ..valid_memory_config()
            })
            .build()
            .await;
        let attested = report(&probe);
        drop(probe);
        let fixed = memory_broker(root.path())
            .with_storage_limits(raised.clone())
            .with_memory_config(ChatMemoryConfig {
                continuity_migration: Some(migration(
                    &attested.legacy_authority_digest,
                    &attested.candidates[0].resolved,
                    overrides,
                )),
                ..valid_memory_config()
            })
            .build()
            .await;
        assert_eq!(remembered_bytes(&fixed).expect("migrated"), 20);
        drop(fixed);
        let restarted = memory_broker(root.path())
            .with_storage_limits(raised.clone())
            .build()
            .await;
        assert_eq!(
            remembered_bytes(&restarted).expect("migrated pointer needs no key"),
            20
        );
    }
}

#[tokio::test]
async fn migration_never_launders_an_authority_change_and_refuses_without_proof() {
    use dekopon_storage_host::ContinuityCheck;

    let root = tempfile::tempdir().expect("tempdir");
    let fetch = [("http-probe.fetch", "http-probe")];
    let permitted = [memory_policy().as_slice(), &fetch].concat();
    let fetching = || {
        SurfaceBroker::new(root.path(), &permitted)
            .with_memory()
            .with_identities([(MEMBER, "maintainer")])
    };
    let released = fetching().build().await;
    remember_as_released_broker(&released, br#"{"turn":"released"}"#);
    let attested = report(&released);
    drop(released);
    let current = migration(
        &attested.legacy_authority_digest,
        &attested.current_resolved_digest,
        super::OperationalOverrides::default(),
    );
    let with = |migration: Option<super::ContinuityMigration>| ChatMemoryConfig {
        continuity_migration: migration,
        ..valid_memory_config()
    };

    let absent = fetching().build().await;
    assert_eq!(unproven(remembered_bytes(&absent)), ContinuityCheck::Absent);
    drop(absent);
    let mut narrowed = fetching().with_memory_config(with(Some(current.clone())));
    for (_, set) in &mut narrowed.constraints {
        if let Some(http) = set.constraints.http.as_mut() {
            http.allowed_methods = vec!["HEAD".to_owned()];
        }
    }
    let narrowed = narrowed.build().await;
    assert_eq!(
        unproven(remembered_bytes(&narrowed)),
        ContinuityCheck::NoCandidate
    );
    drop(narrowed);
    let fenced = fetching()
        .with_policy(
            r#"@id("late-fence") forbid(principal, action, resource)
            when { context has request }
            when { context.request.method == "POST" };"#,
        )
        .with_memory_config(with(Some(current.clone())))
        .build()
        .await;
    assert_eq!(
        unproven(remembered_bytes(&fenced)),
        ContinuityCheck::DigestMismatch
    );
    drop(fenced);
    let accepted = fetching()
        .with_memory_config(with(Some(current)))
        .build()
        .await;
    assert_eq!(remembered_bytes(&accepted).expect("still provable"), 20);
}

#[tokio::test]
async fn legacy_proof_refuses_space_routing_and_id_only_contexts() {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};
    use dekopon_storage_host::{ContinuityCheck, LegacyContinuity};

    let digest = |broker: &Broker<InMemoryAuditLog>| {
        let attested = report(broker);
        migration(
            &attested.legacy_authority_digest,
            &attested.current_resolved_digest,
            super::OperationalOverrides::default(),
        )
    };
    let check = |broker: &Broker<InMemoryAuditLog>, context: &AuthenticatedContext| match broker
        .legacy_continuity(context)
        .expect("legacy proof")
    {
        LegacyContinuity::Unproven(check) => Some(check),
        LegacyContinuity::Candidates(_) => None,
    };
    let attested = |root: &std::path::Path, build: fn(SurfaceBroker<'_>) -> SurfaceBroker<'_>| {
        let root = root.to_path_buf();
        async move {
            let probe = build(memory_broker(&root)).build().await;
            let migration = digest(&probe);
            drop(probe);
            build(memory_broker(&root))
                .with_memory_config(ChatMemoryConfig {
                    continuity_migration: Some(migration),
                    ..valid_memory_config()
                })
                .build()
                .await
        }
    };
    let dirs = [(); 4].map(|()| tempfile::tempdir().expect("tempdir"));

    let eligible = attested(dirs[0].path(), |broker| broker).await;
    assert_eq!(check(&eligible, &member_context()), None);

    let space = attested(dirs[1].path(), |broker| {
        broker.with_identities([
            (MEMBER, "maintainer"),
            ("drn:slack:channel:t0123abc:c0123abc", "team-channel"),
        ])
    })
    .await;
    let as_direct_message = attested_agent_context(
        MEMBER,
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::DirectMessage,
            Trigger::Message,
        ),
    );
    for context in [member_context(), as_direct_message] {
        assert_eq!(check(&space, &context), Some(ContinuityCheck::SpaceBound));
    }

    let routing = attested(dirs[2].path(), |broker| {
        broker.with_policy(
            r#"@id("via-fence") forbid(principal, action, resource)
            when { context has request }
            when { context.request.method == "POST" && ({ routing: context }).routing.via == "other" };"#,
        )
    })
    .await;
    assert_eq!(
        check(&routing, &member_context()),
        Some(ContinuityCheck::RoutingDependent)
    );

    let id_only = attested(dirs[3].path(), |broker| broker.with_identities([])).await;
    assert_eq!(
        check(&id_only, &member_context()),
        Some(ContinuityCheck::IdOnlySubject)
    );
}

#[tokio::test]
async fn an_unproven_pointer_starts_fresh_only_under_the_explicit_disposition() {
    let root = tempfile::tempdir().expect("tempdir");
    let released = memory_broker(root.path()).build().await;
    remember_as_released_broker(&released, br#"{"turn":"released"}"#);
    drop(released);
    let fresh = memory_broker(root.path())
        .with_memory_config(ChatMemoryConfig {
            continuity_migration: Some(super::ContinuityMigration {
                on_unproven: dekopon_storage_host::UnprovenDisposition::FreshEpoch,
                ..super::ContinuityMigration::default()
            }),
            ..valid_memory_config()
        })
        .build()
        .await;
    assert_eq!(remembered_bytes(&fresh).expect("fresh epoch"), 0);
}

#[tokio::test]
async fn relaxed_memory_survives_authority_changes_and_paranoid_does_not() {
    let relaxed = ChatMemoryConfig {
        continuity_policy: dekopon_storage_host::ContinuityPolicy::Relaxed,
        ..valid_memory_config()
    };
    for (config, expected) in [(relaxed, 22), (valid_memory_config(), 0)] {
        let root = tempfile::tempdir().expect("tempdir");
        let before = memory_broker(root.path())
            .with_memory_config(config.clone())
            .build()
            .await;
        remember(&before, br#"{"turn":"remembered"}"#);
        drop(before);
        let widened = SurfaceBroker::new(
            root.path(),
            &[
                memory_policy().as_slice(),
                &[("cli-probe.upper", "cli-probe")],
            ]
            .concat(),
        )
        .with_memory()
        .with_identities([(MEMBER, "maintainer")])
        .with_memory_config(config)
        .build()
        .await;
        assert_eq!(remembered_bytes(&widened).expect("read"), expected);
    }
}

#[tokio::test]
async fn the_legacy_surface_reproduces_the_released_bytes() {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};
    use sha2::Digest as _;

    let root = tempfile::tempdir().expect("tempdir");
    let permitted = [
        memory_policy().as_slice(),
        &[("http-probe.fetch", "http-probe")],
    ]
    .concat();
    let broker = SurfaceBroker::new(root.path(), &permitted)
        .with_memory()
        .with_secret(true, Some("revision-a"))
        .build()
        .await;
    let context = attested_agent_context(
        "drn:slack:user:t0123abc:u9xyz",
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Message,
        ),
    );
    // The golden pins every encoder byte except provider digests, which production takes from deployed artifacts.
    for (memory_route, released) in [
        (
            false,
            "770427d39dffba4caffd6c8bcfa5a8791751b154e0133bf0c77ab1f242cc32de",
        ),
        (
            true,
            "7a8325d3119fb0d1730ece3ff3835b084f885b27e99e938f13693d633a268246",
        ),
    ] {
        let mut released_surface = surface(&broker, memory_route);
        let effective = broker.effective_authority(&context, memory_route);
        for (provider, released_digest) in [
            (
                "http-probe",
                "3793561c425d317cf259a54555e4b85ec90f4464fd2edb0c5678c11fa8591eb1",
            ),
            (
                "memory",
                "c00baf58470ea726654e43a04d806760aa8f191d5c1c43839b9132eb10383422",
            ),
        ] {
            let provider: ProviderId = provider.parse().expect("provider");
            let current_digest = broker
                .registry
                .loaded_provider_metadata()
                .find(|metadata| metadata.manifest.id == provider)
                .map(|metadata| metadata.artifact_sha256)
                .expect("the fixture artifact is loaded");
            assert_eq!(current_digest.len(), released_digest.len());
            let expected = effective
                .iter()
                .filter(|(_, set)| set.provider == provider)
                .count();
            let offsets = released_surface
                .windows(current_digest.len())
                .enumerate()
                .filter_map(|(offset, bytes)| {
                    (bytes == current_digest.as_bytes()).then_some(offset)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                offsets.len(),
                expected,
                "provider {provider}, memory route {memory_route}"
            );
            for offset in offsets {
                released_surface[offset..offset + current_digest.len()]
                    .copy_from_slice(released_digest.as_bytes());
            }
        }
        let digest = sha2::Sha256::digest(released_surface);
        let digest = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(digest, released, "memory route {memory_route}");
    }
}

#[test]
fn continuity_configuration_refuses_old_spellings_and_malformed_migrations() {
    let parse = |extra: serde_json::Value| {
        let mut config = serde_json::to_value(valid_memory_config()).expect("config");
        config
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        serde_json::from_value::<ChatMemoryConfig>(config)
    };
    let digest = format!("sha256:{}", "0".repeat(64));
    for spelling in ["relaxed", "paranoid"] {
        parse(serde_json::json!({ "continuityPolicy": spelling })).expect(spelling);
    }
    for refused in [
        serde_json::json!({ "continuityPolicy": "stable" }),
        serde_json::json!({ "continuityPolicy": "authority-bound" }),
        serde_json::json!({ "continuityMigration": { "authorityDigest": "sha256:ABC" } }),
        serde_json::json!({ "continuityMigration": { "onUnproven": "reset" } }),
        serde_json::json!({ "continuityMigration": { "candidates": [{
            "resolvedDigest": digest, "overrides": { "storage": { "allowedHosts": ["x"] } }
        }] } }),
        serde_json::json!({ "continuityMigration": { "candidates": [{
            "resolvedDigest": digest, "overrides": { "memory": { "enabledAgents": ["x"] } }
        }] } }),
        serde_json::json!({ "continuityMigration": { "candidates": [{
            "resolvedDigest": digest, "overrides": { "hostLimits": { "allowedHosts": ["x"] } }
        }] } }),
    ] {
        assert!(parse(refused.clone()).is_err(), "{refused}");
    }
    let unpaired = parse(serde_json::json!({
        "continuityMigration": { "authorityDigest": digest }
    }))
    .expect("parses");
    assert!(matches!(
        unpaired.validate(&dekopon_storage_host::StorageLimits::default()),
        Err(super::BrokerBuildError::InvalidChatMemory)
    ));
    let disposition = parse(serde_json::json!({
        "continuityMigration": { "onUnproven": "fresh-epoch" }
    }))
    .expect("a disposition alone is valid");
    disposition
        .validate(&dekopon_storage_host::StorageLimits::default())
        .expect("valid");
}

#[test]
fn probe_admission_is_the_only_effect_authority_in_the_paranoid_fingerprint() {
    let encode = |effect| {
        let mut encoded = AuthorityEncoder::new();
        super::continuity::encode_capability_identity(
            &mut encoded,
            &"cli-probe.upper".parse().expect("capability"),
            &ConstraintSet {
                effect,
                ..cli_probe_set(
                    dekopon_capability::EffectKind::ReadOnly,
                    dekopon_core::RiskLevel::Low,
                )
            },
        );
        encoded.finish()
    };
    use dekopon_capability::EffectKind;
    assert_ne!(
        encode(EffectKind::ReadOnly),
        encode(EffectKind::ExternalWrite)
    );
    assert_eq!(
        encode(EffectKind::LocalWrite),
        encode(EffectKind::ExternalWrite)
    );
}

#[tokio::test]
async fn a_membership_a_request_selector_tests_rotates_paranoid_memory() {
    let dirs = [(); 2].map(|()| tempfile::tempdir().expect("tempdir"));
    let fetch = [("http-probe.fetch", "http-probe")];
    let permitted = [memory_policy().as_slice(), &fetch].concat();
    let fenced = |index: usize, member: &'static str, other: &'static str| {
        SurfaceBroker::new(dirs[index].path(), &permitted)
            .with_memory()
            .with_identities([(MEMBER, "maintainer")])
            .with_group_members([(member, "restricted"), (other, "elsewhere")])
            .with_policy(
                r#"@id("selected-fence") forbid(principal, action, resource)
                when { context has request }
                when {
                    (if context.request.method == "POST" then principal
                     else Dekopon::Principal::"someone") in Dekopon::Group::"restricted"
                };"#,
            )
    };
    let outside = paranoid_surface(&fenced(0, "someone", "maintainer").build().await);
    let inside = paranoid_surface(&fenced(1, "maintainer", "someone").build().await);
    assert_ne!(outside, inside);
}

#[tokio::test]
async fn turns_that_alternate_a_fact_a_request_policy_reads_never_reuse_a_generation() {
    use dekopon_broker_protocol::{ChatTransportKind, ConversationKind, Trigger};

    let root = tempfile::tempdir().expect("tempdir");
    let fetch = [("http-probe.fetch", "http-probe")];
    let permitted = [memory_policy().as_slice(), &fetch].concat();
    let broker = SurfaceBroker::new(root.path(), &permitted)
        .with_memory()
        .with_identities([(MEMBER, "maintainer")])
        .with_policy(
            r#"@id("wake-fence") forbid(principal, action, resource)
            when { context has request }
            when { context.trigger == "wake" && context.request.method == "POST" };"#,
        )
        .build()
        .await;
    let message = member_context();
    let wake = attested_agent_context(
        MEMBER,
        chat_scope(
            ChatTransportKind::Slack,
            ConversationKind::Channel,
            Trigger::Wake,
        ),
    );
    let bytes = |context: &AuthenticatedContext| {
        let mut handle =
            memory_handle_in(&broker, context, "memory.chat.recent").expect("read grant");
        let size = handle.jsonl_size("turns.jsonl").unwrap_or(0);
        handle.finish_read().expect("finish read");
        size
    };
    let mut writer = memory_handle_in(&broker, &message, "memory.chat.record").expect("record");
    writer
        .jsonl_append("turns.jsonl", 0, br#"{"turn":"message"}"#)
        .expect("append");
    writer.commit().expect("commit");
    assert_eq!(bytes(&message), 19);
    assert_eq!(bytes(&wake), 0, "the wake turn's request authority differs");
    assert_eq!(
        bytes(&message),
        0,
        "returning never reopens the earlier generation"
    );
}

#[tokio::test]
async fn an_http_method_case_change_rotates_paranoid_memory() {
    let dirs = [(); 2].map(|()| tempfile::tempdir().expect("tempdir"));
    let fetch = [("http-probe.fetch", "http-probe")];
    let permitted = [memory_policy().as_slice(), &fetch].concat();
    let spelled = |index: usize, method: &str| {
        let mut broker = SurfaceBroker::new(dirs[index].path(), &permitted)
            .with_memory()
            .with_identities([(MEMBER, "maintainer")]);
        for (_, set) in &mut broker.constraints {
            if let Some(http) = set.constraints.http.as_mut() {
                http.allowed_methods = vec![method.to_owned()];
            }
        }
        broker
    };
    let lowercase = paranoid_surface(&spelled(0, "get").build().await);
    let uppercase = paranoid_surface(&spelled(1, "GET").build().await);
    assert_ne!(lowercase, uppercase);
}
