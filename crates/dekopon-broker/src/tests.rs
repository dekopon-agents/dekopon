use dekopon_broker_host::BrokerHostLimits;
use dekopon_capability::{
    ExecutionConstraints, HttpConstraints, StorageAccess, StorageConstraints, StorageInterface,
    StorageNamespace,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, TraceId,
};

use super::{
    AttestorGrant, AuditConfigurationError, AuditError, AuditEvent, AuditLog, AuthenticatedContext,
    AuthorityEncoder, BrokerBuildError, CapabilityRoute, ChatMemoryConfig, ChatScopeClaim,
    ChatScopeGrant, ChatTransportKind, ConstraintSet, ContextError, InMemoryAuditLog,
    canonical_chat_scope, encode_execution_constraints, encode_host_limits, encode_memory_config,
    encode_storage_limits,
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
        capability: "echo.echo"
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
    // An operator-chosen `memory.chat.export` on a provider named `memory-chat`, with no route:
    // reserved-looking in every spelling and generic in the only place that decides.
    let authored = serde_json::json!({
        "provider": "memory-chat",
        "effect": "read-only",
        "risk": "Low",
        "idempotency": "idempotent",
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
    minimal.max_turn_bytes = 251;
    minimal.max_dedup_records = 1;
    minimal.max_dedup_bytes = 256;
    minimal.compaction_target_bytes = 251;
    minimal.compaction_threshold_bytes = 252;
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

    // The direct live peak includes the post-append turn file, permanent dedup, and metadata.
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

    // The minimum fixture reads one chunk from each file, makes both size calls, appends both
    // records, and may replace turns: seven host calls exactly.
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
fn execution_authority_normalizes_sets_but_commits_every_constraint() {
    fn bytes(constraints: &ExecutionConstraints) -> Vec<u8> {
        let mut encoded = AuthorityEncoder::new();
        encode_execution_constraints(&mut encoded, constraints);
        encoded.finish()
    }
    let baseline = ExecutionConstraints {
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

    // A reset keeps the corrupt code; only whether the store is already usable again differs.
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

/// A permanent exhaustion is not a momentary outage. The bounded in-memory audit does not evict,
/// so a client told to resubmit under a fresh identifier would loop against a broker that is
/// capped forever.
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

    // The same exhaustion *after* execution stays an unaudited outcome: the effect may already
    // have happened, and that classification outranks how the append failed.
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

/// A materialization task that panicked reported itself as `StorageHostError::Io`, which sent an
/// operator to the filesystem for a bug that is in the code. The wire category stays `storage-io`
/// — the same step failed before any provider ran — but the panic's own account now survives in
/// the error chain instead of being replaced by a fabricated I/O failure.
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

/// `localSubjectService` is the one chat-scope field that is free-form operator text rather than
/// a structural rule, and a typo in it used to produce "attestor chat scope is invalid" — the
/// same sentence four other rejections produce. The refusal now carries the parse failure, which
/// quotes the word to change.
#[test]
fn an_unknown_local_subject_service_names_itself_in_the_refusal() {
    let grant = |service: &str| AttestorGrant {
        namespaces: vec!["slack".to_owned()],
        chat_scopes: vec![ChatScopeGrant::TransportWide {
            kind: ChatTransportKind::Local,
            transport: "local".parse().expect("transport"),
            local_subject_service: Some(service.to_owned()),
        }],
    };

    let error = grant("slcak")
        .validate()
        .expect_err("an unknown local subject service is refused");
    assert!(matches!(
        error,
        BrokerBuildError::InvalidChatScopeService { .. }
    ));
    let cause = std::error::Error::source(&error).expect("the parse failure is the cause");
    assert!(
        cause.to_string().contains("slcak"),
        "the refusal does not name the offending service: {cause}"
    );

    grant("slack")
        .validate()
        .expect("a canonical local subject service is accepted");
}

#[test]
fn exact_chat_scope_configuration_requires_service_canonical_forms() {
    for scope in [
        ChatScopeGrant::ExactChannel {
            kind: ChatTransportKind::Discord,
            transport: "discord".parse().expect("transport"),
            channel: "00123".to_owned(),
            local_subject_service: None,
        },
        ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Discord,
            transport: "discord".parse().expect("transport"),
            channel: "123".to_owned(),
            conversation: "456".to_owned(),
            local_subject_service: None,
        },
        ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Slack,
            transport: "slack".parse().expect("transport"),
            channel: "c0123abc".to_owned(),
            conversation: "c0123abc:01712345678.1".to_owned(),
            local_subject_service: None,
        },
        ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Telegram,
            transport: "telegram".parse().expect("transport"),
            channel: "-1001".to_owned(),
            conversation: "-1001:topic:00".to_owned(),
            local_subject_service: None,
        },
        ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Telegram,
            transport: "telegram".parse().expect("transport"),
            channel: "-1001".to_owned(),
            conversation: "-1001:topic:9223372036854775808".to_owned(),
            local_subject_service: None,
        },
        ChatScopeGrant::ExactChannel {
            kind: ChatTransportKind::Slack,
            transport: "slack".parse().expect("transport"),
            channel: format!("c{}", "x".repeat(256)),
            local_subject_service: None,
        },
    ] {
        assert!(
            AttestorGrant {
                namespaces: vec!["slack".to_owned()],
                chat_scopes: vec![scope],
            }
            .validate()
            .is_err()
        );
    }

    AttestorGrant {
        namespaces: vec!["telegram".to_owned()],
        chat_scopes: vec![ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Telegram,
            transport: "telegram".parse().expect("transport"),
            channel: i64::MIN.to_string(),
            conversation: format!("{}:topic:{}", i64::MIN, i64::MAX),
            local_subject_service: None,
        }],
    }
    .validate()
    .expect("signed Telegram service limits are accepted exactly");

    AttestorGrant {
        namespaces: vec!["whatsapp".to_owned()],
        chat_scopes: vec![ChatScopeGrant::ExactConversation {
            kind: ChatTransportKind::Whatsapp,
            transport: "whatsapp".parse().expect("transport"),
            channel: "123:456:16034700182".to_owned(),
            conversation: "123:456:16034700182".to_owned(),
            local_subject_service: None,
        }],
    }
    .validate()
    .expect("exact WhatsApp WABA, phone, and sender scope is accepted");

    let scope = ChatScopeClaim {
        transport: "whatsapp".parse().expect("transport"),
        kind: ChatTransportKind::Whatsapp,
        channel: "123:456:16034700182".to_owned(),
        conversation: "123:456:16034700182".to_owned(),
    };
    assert!(canonical_chat_scope(
        &ExternalSubject::whatsapp("16034700182").expect("subject"),
        &scope,
    ));
    assert!(
        !canonical_chat_scope(
            &ExternalSubject::whatsapp("16034700999").expect("subject"),
            &scope,
        ),
        "the signed sender in scope cannot be detached from the attested subject"
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

/// Pins exactly which HTTP scopes this broker starts with, now that the grammar is shared.
///
/// The rules moved into `HttpConstraints::validate` so the capability gate and the HTTP host stop
/// carrying weaker copies. Nothing here may become acceptable, and nothing already acceptable may
/// start failing: this is the same broker startup decision, made in one place.
#[test]
fn policy_http_scope_values_are_bounded() {
    fn constrain(http: dekopon_capability::HttpConstraints) -> ConstraintSet {
        ConstraintSet {
            route: CapabilityRoute::Generic,
            provider: "echo".parse().expect("provider"),
            effect: dekopon_capability::EffectKind::ReadOnly,
            risk: dekopon_core::RiskLevel::Low,
            idempotency: dekopon_capability::Idempotency::Idempotent,
            credential: None,
            credential_by_agent: Default::default(),
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

/// The authored spelling of a per-agent credential, and what selection does with it.
///
/// The map key is an `AgentId`, so a name no agent could carry is a decode failure rather than an
/// override that silently never matches. An absent map stays off the wire, which keeps a
/// constraint set written before this existed serializing exactly as it did.
#[test]
fn per_agent_credentials_decode_validate_their_keys_and_select_by_actor() {
    let document = r#"{
        "provider": "gh",
        "effect": "external-write",
        "risk": "Medium",
        "idempotency": "non-idempotent",
        "credential": "github-pat",
        "credentialByAgent": { "nestedset-github": "github-pat-scientist-hq" },
        "constraints": { "timeoutMs": 1000, "maxOutputBytes": 1024 }
    }"#;
    let set = serde_json::from_str::<super::ConstraintSet>(document).expect("authored set decodes");
    assert_eq!(
        set.credential_by_agent
            .get(&"nestedset-github".parse::<AgentId>().expect("valid agent")),
        Some(&"github-pat-scientist-hq".to_owned())
    );

    let agent = |name: &str| Actor::Agent {
        agent: name.parse::<AgentId>().expect("valid agent"),
    };
    assert_eq!(
        set.credential_for(&agent("nestedset-github")),
        Some("github-pat-scientist-hq")
    );
    assert_eq!(
        set.credential_for(&agent("dekoponville-github")),
        Some("github-pat")
    );
    // No agent, no override: the shape a direct service peer arrives in.
    assert_eq!(
        set.credential_for(&Actor::Service {
            principal: "local-user"
                .parse::<PrincipalId>()
                .expect("valid principal"),
        }),
        Some("github-pat")
    );

    assert!(
        serde_json::from_str::<super::ConstraintSet>(
            &document.replace("nestedset-github", "Nested Set")
        )
        .is_err(),
        "a map key that is not a valid agent identifier must not decode"
    );

    let without = serde_json::from_str::<super::ConstraintSet>(&document.replace(
        r#""credentialByAgent": { "nestedset-github": "github-pat-scientist-hq" },"#,
        "",
    ))
    .expect("a set with no overrides decodes");
    assert!(without.credential_by_agent.is_empty());
    assert!(
        !serde_json::to_string(&without)
            .expect("serializes")
            .contains("credentialByAgent"),
        "an empty override map must stay off the wire"
    );
}
