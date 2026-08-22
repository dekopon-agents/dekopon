use std::{fs, sync::Arc};

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt as _, symlink};

use dekopon_broker_host::BrokerHostLimits;
use dekopon_capability::{
    ExecutionConstraints, HttpConstraints, StorageAccess, StorageConstraints, StorageInterface,
    StorageNamespace,
};
use dekopon_core::{
    Actor, AgentId, CapabilityId, ExternalSubject, InvocationId, PrincipalId, TraceId,
};

use super::{
    AttestorGrant, AuditConfigurationError, AuditError, AuditEvent, AuditIntegrityError, AuditLog,
    AuditRecord, AuthenticatedContext, AuthorityEncoder, ChatMemoryConfig, ChatScopeClaim,
    ChatScopeGrant, ChatTransportKind, ConstraintSet, ContextError, FileAuditError, FileAuditLog,
    InMemoryAuditLog, canonical_chat_scope, encode_execution_constraints, encode_host_limits,
    encode_memory_config, encode_storage_limits, is_reserved_memory_route, verify_audit_chain,
};

fn decision(invocation: &str, allowed: bool) -> AuditEvent {
    AuditEvent::Decision {
        invocation: invocation
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        trace: "trace-test"
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
fn the_complete_memory_prefix_and_provider_are_reserved() {
    let provider_route = ConstraintSet {
        provider: "memory-chat".parse().expect("provider"),
        effect: dekopon_capability::EffectKind::ReadOnly,
        risk: dekopon_core::RiskLevel::Low,
        idempotency: dekopon_capability::Idempotency::Idempotent,
        credential: None,
        credential_by_agent: Default::default(),
        constraints: ExecutionConstraints::default(),
    };
    assert!(is_reserved_memory_route(
        &"memory.chat.export".parse().expect("capability"),
        None,
    ));
    assert!(is_reserved_memory_route(
        &"unrelated.extra".parse().expect("capability"),
        Some(&provider_route),
    ));
    assert!(!is_reserved_memory_route(
        &"ordinary.read".parse().expect("capability"),
        None,
    ));
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

    // The old threshold + target estimate fit in 30 MiB; the real old+staged peak can hold two
    // near-threshold turn files plus both permanent-dedup copies and transaction metadata.
    let too_small_namespace = dekopon_storage_host::StorageLimits {
        max_namespace_bytes: 30 * 1024 * 1024,
        ..dekopon_storage_host::StorageLimits::default()
    };
    assert!(memory.validate(&too_small_namespace).is_err());

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

    let exact_namespace = memory.compaction_threshold_bytes * 2
        + memory.max_turn_bytes
        + memory.max_dedup_bytes * 2
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
        ("startupMaxTransactions", |v| {
            v.startup_max_transactions += 1
        }),
        ("maxQuarantinedNamespaces", |v| {
            v.max_quarantined_namespaces += 1
        }),
        ("retiredGenerationGraceMs", |v| {
            v.retired_generation_grace_ms += 1
        }),
        ("retiredGenerationTtlMs", |v| {
            v.retired_generation_ttl_ms += 1
        }),
        ("inactiveNamespaceTtlMs", |v| {
            v.inactive_namespace_ttl_ms += 1
        }),
        ("gcIntervalMs", |v| v.gc_interval_ms += 1),
        ("gcMaxNamespacesPerPass", |v| {
            v.gc_max_namespaces_per_pass += 1
        }),
        ("gcMaxBytesPerPass", |v| v.gc_max_bytes_per_pass += 1),
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
            dekopon_storage_host::StorageHostError::Corrupt { scope: "test" },
            "storage-corrupt",
        ),
        (dekopon_storage_host::StorageHostError::Io, "storage-io"),
    ] {
        assert_eq!(
            super::BrokerError::Storage { source }.storage_failure_code(),
            Some(expected)
        );
    }
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
async fn audit_chain_detects_content_and_link_mutation() {
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("first append succeeds");
    audit
        .append(decision("invoke-two", false))
        .await
        .expect("second append succeeds");
    let records = audit.records().await;
    verify_audit_chain(&records).expect("fresh chain verifies");

    let mut mutated = records.clone();
    if let AuditEvent::Decision { allowed, .. } = &mut mutated[0].event {
        *allowed = false;
    }
    assert_eq!(
        verify_audit_chain(&mutated),
        Err(AuditIntegrityError::RecordHash { index: 0 })
    );

    let mut reordered = records;
    reordered.swap(0, 1);
    assert_eq!(
        verify_audit_chain(&reordered),
        Err(AuditIntegrityError::Sequence { index: 0 })
    );
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

#[tokio::test]
async fn durable_audit_reopens_verifies_and_continues_the_chain() {
    let directory = tempfile::tempdir().expect("create audit fixture directory");
    let path = directory.path().join("audit.jsonl");
    let audit = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect("create durable audit");
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("first durable append succeeds");
    audit
        .append(decision("invoke-two", false))
        .await
        .expect("second durable append succeeds");
    let checkpoint = audit.checkpoint().await;
    assert_eq!(checkpoint.0, 2);
    assert!(checkpoint.1.is_some());
    assert!(audit.contains_checkpoint(0, None).await);
    assert!(audit.contains_checkpoint(2, checkpoint.1.as_deref()).await);
    let first = serde_json::from_str::<AuditRecord>(
        fs::read_to_string(&path)
            .expect("read synchronized audit")
            .lines()
            .next()
            .expect("first record exists"),
    )
    .expect("first record decodes");
    assert!(audit.contains_checkpoint(1, Some(&first.record_hash)).await);
    assert!(!audit.contains_checkpoint(1, checkpoint.1.as_deref()).await);
    assert!(!audit.contains_checkpoint(3, None).await);
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("a second writer must not share the audit file");
    assert!(matches!(error, FileAuditError::Lock { .. }));
    drop(audit);

    let audit = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect("existing chain verifies");
    assert_eq!(audit.checkpoint().await, checkpoint);
    assert_eq!(
        audit
            .replay_ids()
            .await
            .iter()
            .map(InvocationId::as_str)
            .collect::<Vec<_>>(),
        ["invoke-one", "invoke-two"]
    );
    audit
        .append(decision("invoke-three", true))
        .await
        .expect("append continues verified chain");
    drop(audit);

    let records = fs::read_to_string(&path)
        .expect("read durable fixture")
        .lines()
        .map(|line| serde_json::from_str::<AuditRecord>(line).expect("valid durable record"))
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3);
    verify_audit_chain(&records).expect("reopened chain remains valid");

    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path)
            .expect("audit metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
}

#[tokio::test]
async fn durable_audit_rejects_mutation_and_partial_records() {
    let directory = tempfile::tempdir().expect("create audit fixture directory");
    let path = directory.path().join("audit.jsonl");
    let audit = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect("create durable audit");
    audit
        .append(decision("invoke-one", true))
        .await
        .expect("durable append succeeds");
    drop(audit);

    let original = fs::read_to_string(&path).expect("read durable fixture");
    let mutated = original.replace("\"allowed\":true", "\"allowed\":false");
    assert_ne!(mutated, original);
    fs::write(&path, mutated).expect("tamper with durable fixture");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("semantic mutation must fail verification");
    assert!(matches!(
        error,
        FileAuditError::Integrity {
            source: AuditIntegrityError::RecordHash { index: 0 },
            ..
        }
    ));

    fs::write(&path, format!("{original}{{\"partial\":")).expect("write partial durable fixture");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("partial final record must fail closed");
    assert!(matches!(
        error,
        FileAuditError::UnterminatedRecord { line: 2 }
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn durable_audit_rejects_non_private_permissions() {
    let directory = tempfile::tempdir().expect("create audit fixture directory");
    let path = directory.path().join("audit.jsonl");
    fs::write(&path, []).expect("create audit fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
        .expect("set insecure fixture permissions");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("group/world-readable audit must fail");
    assert!(matches!(error, FileAuditError::InsecureFile));

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .expect("restore private fixture permissions");
    let hard_link = directory.path().join("audit-hard-link.jsonl");
    fs::hard_link(&path, &hard_link).expect("create hard-link fixture");
    let error = FileAuditLog::open(&path, 4, 16 * 1024)
        .await
        .expect_err("multiply linked audit must fail");
    assert!(matches!(error, FileAuditError::InsecureFile));

    fs::remove_file(&hard_link).expect("remove hard-link fixture");
    let symlink_path = directory.path().join("audit-symlink.jsonl");
    symlink(&path, &symlink_path).expect("create symlink fixture");
    let error = FileAuditLog::open(&symlink_path, 4, 16 * 1024)
        .await
        .expect_err("audit symlink must not be followed");
    assert!(matches!(error, FileAuditError::Io { .. }));
}

#[test]
fn policy_http_scope_values_are_bounded() {
    let constraints = ExecutionConstraints {
        http: Some(dekopon_capability::HttpConstraints {
            allowed_hosts: vec![" ".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 1,
            max_request_bytes: 1,
            max_response_bytes: 1,
            allow_plaintext_loopback: false,
        }),
        ..ExecutionConstraints::default()
    };
    let set = ConstraintSet {
        provider: "echo".parse().expect("provider"),
        effect: dekopon_capability::EffectKind::ReadOnly,
        risk: dekopon_core::RiskLevel::Low,
        idempotency: dekopon_capability::Idempotency::Idempotent,
        credential: None,
        credential_by_agent: Default::default(),
        constraints,
    };
    assert!(super::validate_set_constraints(&set).is_err());
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
    // No agent, no override: the shape a direct `dekopon-run` peer arrives in.
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
