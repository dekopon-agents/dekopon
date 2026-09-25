use std::collections::BTreeMap;

use dekopon_broker_host::BrokerHostError;
use dekopon_broker_host::BrokerHostLimits;
use dekopon_capability::{
    ExecutionConstraints, HttpConstraints, StorageAccess, StorageConstraints, StorageInterface,
    StorageNamespace,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, InvocationId, MAX_FAILURE_MESSAGE_BYTES, PrincipalId,
    ProviderFailureDetail, ProviderId, TraceId,
};

use super::{
    AuditConfigurationError, AuditError, AuditEvent, AuditLog, AuthenticatedContext,
    AuthorityEncoder, CapabilityRoute, ChatMemoryConfig, ConstraintSet, ContextError,
    InMemoryAuditLog, encode_capability_authority, encode_execution_constraints,
    encode_host_limits, encode_memory_config, encode_storage_limits, provider_failure_detail,
    public_host_error,
};

fn decision(invocation: &str, allowed: bool) -> AuditEvent {
    AuditEvent::Decision {
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
        policy_revision: Some("policy-test".to_owned()),
        policy_ids: Vec::new(),
        policy_digest: None,
        allowed,
        reason: (!allowed).then(|| "policy-denied".to_owned()),
        decision_digest: format!("sha256:{}", "a".repeat(64)),
        storage_scope_commitment: None,
        storage: None,
    }
}

#[test]
fn the_declared_route_is_what_reserves_a_capability_not_its_spelling() {
    let authored = serde_json::json!({
        "provider": "memory-chat",
        "effect": "read-only",
        "risk": "Low",
        "constraints": {"timeoutMs": 1000, "maxOutputBytes": 1024},
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
fn memory_composition_reserves_dedup_calls_and_pre_compaction_peak() {
    let memory = ChatMemoryConfig {
        continuity_policy: dekopon_storage_host::ContinuityPolicy::AuthorityBound,
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
    };

    let mut minimal = memory.clone();
    minimal.max_lookback_turns = 1;
    minimal.max_recent_turns = 1;
    minimal.max_search_results = 1;
    minimal.max_turn_bytes = 241;
    minimal.max_dedup_records = 1;
    minimal.max_dedup_bytes = 256;
    minimal.compaction_target_bytes = 241;
    minimal.compaction_threshold_bytes = 242;
    let too_small_call = dekopon_storage_host::StorageLimits {
        max_write_bytes_per_call: 255,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(minimal.validate(&too_small_call).is_err());

    let unaligned_read_budget = dekopon_storage_host::StorageLimits {
        max_read_bytes_per_invocation: 300_000,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(
        minimal.validate(&unaligned_read_budget).is_err(),
        "two final partial chunks are charged at their requested 256 KiB bounds"
    );

    let too_small_namespace = dekopon_storage_host::StorageLimits {
        max_namespace_bytes: 16 * 1024 * 1024,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&too_small_namespace).is_err());
    let formerly_staged_copy_rejection = dekopon_storage_host::StorageLimits {
        max_namespace_bytes: 30 * 1024 * 1024,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&formerly_staged_copy_rejection).is_ok());

    let exact_write_call = dekopon_storage_host::StorageLimits {
        max_write_bytes_per_call: memory.compaction_target_bytes,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&exact_write_call).is_ok());
    let one_below_write_call = dekopon_storage_host::StorageLimits {
        max_write_bytes_per_call: memory.compaction_target_bytes - 1,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&one_below_write_call).is_err());

    let exact_file = memory.compaction_threshold_bytes + memory.max_turn_bytes;
    let exact_file_limit = dekopon_storage_host::StorageLimits {
        max_file_bytes: exact_file,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&exact_file_limit).is_ok());
    let one_below_file_limit = dekopon_storage_host::StorageLimits {
        max_file_bytes: exact_file - 1,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&one_below_file_limit).is_err());

    let exact_namespace = memory.compaction_threshold_bytes
        + memory.max_turn_bytes
        + memory.max_dedup_bytes
        + 32 * 4_096;
    let exact_namespace_limit = dekopon_storage_host::StorageLimits {
        max_namespace_bytes: exact_namespace,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&exact_namespace_limit).is_ok());
    let one_below_namespace_limit = dekopon_storage_host::StorageLimits {
        max_namespace_bytes: exact_namespace - 1,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&one_below_namespace_limit).is_err());

    let exact_host_calls = dekopon_storage_host::StorageLimits {
        max_host_calls_per_invocation: 7,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(minimal.validate(&exact_host_calls).is_ok());
    let one_below_host_calls = dekopon_storage_host::StorageLimits {
        max_host_calls_per_invocation: 6,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(minimal.validate(&one_below_host_calls).is_err());

    let exact_file_count = dekopon_storage_host::StorageLimits {
        max_files_per_namespace: 2,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(minimal.validate(&exact_file_count).is_ok());
    let one_below_file_count = dekopon_storage_host::StorageLimits {
        max_files_per_namespace: 1,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(minimal.validate(&one_below_file_count).is_err());

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
            + memory.max_dedup_bytes * 2
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
        (memory.max_dedup_bytes
            + memory.compaction_threshold_bytes
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
            max_output_bytes: usize::try_from(
                memory.max_result_bytes + super::MEMORY_PROVIDER_OUTPUT_OVERHEAD_BYTES - 1,
            )
            .expect("output fits usize"),
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
        ("maxPendingTransactions", |v| {
            v.max_pending_transactions += 1
        }),
        ("startupMaxEntries", |v| v.startup_max_entries += 1),
    ];
    assert_rotations(&storage, storage_mutations, encoded_storage);

    let memory = ChatMemoryConfig {
        continuity_policy: dekopon_storage_host::ContinuityPolicy::AuthorityBound,
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
        max_dedup_records: 16_000,
        max_dedup_bytes: 4_194_304,
        compaction_target_bytes: 8_388_608,
        compaction_threshold_bytes: 12_582_912,
    };
    let memory_mutations: &[Mutation<ChatMemoryConfig>] = &[
        ("continuityPolicy", |v| {
            v.continuity_policy = dekopon_storage_host::ContinuityPolicy::Stable;
        }),
        ("maxLookbackTurns", |v| v.max_lookback_turns += 1),
        ("maxRecentTurns", |v| v.max_recent_turns += 1),
        ("maxSearchResults", |v| v.max_search_results += 1),
        ("maxQueryBytes", |v| v.max_query_bytes += 1),
        ("maxResultBytes", |v| v.max_result_bytes += 1),
        ("maxTurnBytes", |v| v.max_turn_bytes += 1),
        ("maxDedupRecords", |v| v.max_dedup_records += 1),
        ("maxDedupBytes", |v| v.max_dedup_bytes += 1),
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
        route: CapabilityRoute::Generic,
        provider: "cli-probe".parse().expect("valid provider fixture"),
        effect: dekopon_capability::EffectKind::ReadOnly,
        risk: dekopon_core::RiskLevel::Low,
        credential: Some("probe-token".to_owned()),
        constraints: ExecutionConstraints::default(),
    };
    let mut encoded = AuthorityEncoder::new();
    encode_capability_authority(
        &mut encoded,
        &"cli-probe.upper"
            .parse::<CapabilityId>()
            .expect("valid fixture"),
        &set,
        Some("probe-token"),
        "sha256:artifact",
    );

    assert_eq!(
        labels(&encoded.finish()),
        vec![
            "capability",
            "provider",
            "effect",
            "risk",
            "credential.present",
            "credential",
            "execution.timeoutMs",
            "execution.maxOutputBytes",
            "execution.http.present",
            "execution.storage.present",
            "execution.asset.present",
            "providerArtifactSha256",
        ],
        "the storage authority surface gained or lost a field"
    );
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
        max_output_bytes: 1_048_576,
        http: Some(HttpConstraints {
            allowed_hosts: vec!["b.example:443".to_owned(), "a.example:443".to_owned()],
            allowed_methods: vec!["POST".to_owned(), "GET".to_owned()],
            max_requests: 2,
            max_request_bytes: 3,
            max_response_bytes: 4,
            allow_plaintext_loopback: false,
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
    changes!(|v: &mut ExecutionConstraints| v.max_output_bytes += 1);
    changes!(|v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").max_requests += 1);
    changes!(|v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").max_request_bytes += 1);
    changes!(|v: &mut ExecutionConstraints| v.http.as_mut().expect("HTTP").max_response_bytes += 1);
    changes!(|v: &mut ExecutionConstraints| v
        .http
        .as_mut()
        .expect("HTTP")
        .allow_plaintext_loopback = true);
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

    let storage = ExecutionConstraints {
        http: None,
        storage: Some(StorageConstraints {
            interface: StorageInterface::Jsonl,
            access: StorageAccess::ReadOnly,
            namespace: StorageNamespace::Chat,
        }),
        ..ExecutionConstraints::default()
    };
    let mut durable = storage.clone();
    durable.storage.as_mut().expect("storage").interface = StorageInterface::DurableFiles;
    assert_ne!(bytes(&storage), bytes(&durable));
    let mut writable = storage.clone();
    writable.storage.as_mut().expect("storage").access = StorageAccess::ReadWrite;
    assert_ne!(bytes(&storage), bytes(&writable));
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
            route: CapabilityRoute::Generic,
            provider: "cli-probe".parse().expect("provider"),
            effect: dekopon_capability::EffectKind::ReadOnly,
            risk: dekopon_core::RiskLevel::Low,
            credential: None,
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

    let unbounded = [
        dekopon_capability::HttpConstraints {
            allowed_hosts: Vec::new(),
            ..valid.clone()
        },
        dekopon_capability::HttpConstraints {
            allowed_methods: Vec::new(),
            ..valid.clone()
        },
        dekopon_capability::HttpConstraints {
            allowed_hosts: (0..65).map(|index| format!("h{index}.test")).collect(),
            ..valid.clone()
        },
        dekopon_capability::HttpConstraints {
            allowed_methods: (0..65).map(|index| format!("M{index}")).collect(),
            ..valid.clone()
        },
        dekopon_capability::HttpConstraints {
            max_requests: 0,
            ..valid.clone()
        },
        dekopon_capability::HttpConstraints {
            max_request_bytes: 0,
            ..valid.clone()
        },
        dekopon_capability::HttpConstraints {
            max_response_bytes: 0,
            ..valid
        },
    ];
    for http in unbounded {
        assert!(
            super::validate_set_constraints(&constrain(http.clone())).is_err(),
            "{http:?} must not start this broker"
        );
    }
}

#[test]
fn an_agent_rebinds_only_the_credential_name_its_set_already_uses() {
    let document = r#"{
        "provider": "gh",
        "effect": "external-write",
        "risk": "Medium",
        "credential": "github-pat",
        "constraints": { "timeoutMs": 1000, "maxOutputBytes": 1024 }
    }"#;
    let named =
        serde_json::from_str::<super::ConstraintSet>(document).expect("authored set decodes");
    let unnamed = super::ConstraintSet {
        credential: None,
        ..named.clone()
    };
    let agent = |name: &str| name.parse::<AgentId>().expect("valid agent");
    let catalog = super::ConstraintCatalog::new([(
        "gh.pull-request.merge"
            .parse::<CapabilityId>()
            .expect("valid capability"),
        named.clone(),
    )])
    .expect("one set builds a catalog")
    .with_agent_credentials(BTreeMap::from([(
        agent("nestedset-github"),
        BTreeMap::from([(
            "github-pat".to_owned(),
            "github-pat-scientist-hq".to_owned(),
        )]),
    )]));
    let actor = |name: &str| Actor::Agent { agent: agent(name) };

    assert_eq!(
        catalog.credential_for(&named, &actor("nestedset-github")),
        Some("github-pat-scientist-hq")
    );
    assert_eq!(
        catalog.credential_for(&named, &actor("dekoponville-github")),
        Some("github-pat")
    );
    assert_eq!(
        catalog.credential_for(
            &named,
            &Actor::Service {
                principal: "local-user"
                    .parse::<PrincipalId>()
                    .expect("valid principal"),
            }
        ),
        Some("github-pat")
    );
    assert_eq!(
        catalog.credential_for(&unnamed, &actor("nestedset-github")),
        None
    );

    assert!(
        serde_json::from_str::<super::ConstraintSet>(&document.replace(
            r#""credential": "github-pat","#,
            r#""credential": "github-pat", "credentialByAgent": {},"#,
        ))
        .is_err(),
        "the retired per-set override key must not decode"
    );
}

#[test]
fn a_typed_provider_failure_keeps_its_classification_and_carries_the_providers_own_code() {
    let failure = BrokerHostError::ProviderFailure {
        provider: "gpt-image".parse::<ProviderId>().expect("valid provider"),
        capability: "gpt-image.edit"
            .parse::<CapabilityId>()
            .expect("valid capability"),
        code: "upstream-rejected".to_owned(),
        message: "the image route refused the request with HTTP 400 (moderation_blocked: the \
                  request was rejected)"
            .to_owned(),
    };

    assert_eq!(
        public_host_error(&failure, CapabilityRoute::Generic),
        "provider-failure"
    );
    assert_eq!(
        provider_failure_detail(&failure),
        Some(ProviderFailureDetail::new(
            "upstream-rejected",
            "the image route refused the request with HTTP 400 (moderation_blocked: the request \
             was rejected)"
        ))
    );
}

#[test]
fn a_provider_message_past_its_bound_is_cut_before_it_leaves_the_broker() {
    let failure = BrokerHostError::ProviderFailure {
        provider: "gpt-image".parse::<ProviderId>().expect("valid provider"),
        capability: "gpt-image.edit"
            .parse::<CapabilityId>()
            .expect("valid capability"),
        code: "upstream-rejected".to_owned(),
        message: "m".repeat(MAX_FAILURE_MESSAGE_BYTES + 1),
    };

    let detail = provider_failure_detail(&failure).expect("a typed provider failure has a detail");

    assert_eq!(
        detail.message,
        format!(
            "{}\u{2026}[truncated]",
            "m".repeat(MAX_FAILURE_MESSAGE_BYTES)
        )
    );
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
    }
}

#[test]
fn asset_grants_preserve_effect_classes_and_the_http_storage_exclusion() {
    use dekopon_capability::{
        AssetConstraints, EffectKind, StorageAccess, StorageConstraints, StorageInterface,
        StorageNamespace,
    };
    let mut set = ConstraintSet {
        route: CapabilityRoute::Generic,
        provider: "probe".parse().unwrap(),
        effect: EffectKind::LocalWrite,
        risk: dekopon_core::RiskLevel::Low,
        credential: None,
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
        namespace: StorageNamespace::Chat,
    });
    assert!(super::validate_set_constraints(&set).is_ok());
    set.constraints.http = Some(dekopon_capability::HttpConstraints {
        allowed_hosts: vec!["example.com".to_owned()],
        allowed_methods: vec!["POST".to_owned()],
        max_requests: 1,
        max_request_bytes: 1,
        max_response_bytes: 1,
        allow_plaintext_loopback: false,
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
