#![allow(clippy::unwrap_used)]

use dekopon_provider_sdk_testkit::{
    BrokerHostError, BrokerHostLimits, CommandRunOutcome, ContinuityPolicy, FakeBroker,
    FakeBrokerError, StorageAccess, StorageInterface, StorageLimits,
};
use dekopon_test_support::provider_fixture;
use serde_json::{Value, json};

fn record(id: &str, user: &str, assistant: &str) -> Value {
    json!({
        "operation": "record",
        "id": id,
        "commitment": format!("commitment-{id}"),
        "user": user,
        "assistant": assistant,
        "maxTurnBytes": 4096,
        "maxLookbackTurns": 64,
        "maxDedupRecords": 64,
        "maxDedupBytes": 65536,
        "compactionTargetBytes": 8192,
        "compactionThresholdBytes": 16384,
    })
}

async fn memory_chat() -> FakeBroker {
    FakeBroker::builder()
        .component(provider_fixture("memory-chat-provider.wasm"))
        .provider("memory-chat")
        .storage(StorageInterface::Jsonl, StorageAccess::ReadWrite)
        .build()
        .await
        .expect("memory-chat loads")
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_a_storage_backed_component_against_a_real_storage_host() {
    let broker = FakeBroker::builder()
        .component(provider_fixture("storage-probe-provider.wasm"))
        .provider("storage-probe")
        .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
        .build()
        .await
        .expect("storage-probe loads");

    let output = broker
        .invoke_full("storage-probe.run", json!({}))
        .await
        .expect("the durable-files conformance sequence completes");

    assert_eq!(output.output["clocksCalled"], true);
    assert_eq!(output.output["entropyBytes"], 32);
    assert_eq!(output.output["identityNonzero"], true);
    let evidence = output
        .storage
        .expect("a storage-backed invocation carries evidence");
    assert!(evidence.operations > 0, "{evidence:?}");
    assert_eq!(evidence.quota_denials, 0, "{evidence:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn successive_invocations_reach_one_durable_namespace() {
    let broker = memory_chat().await;

    broker
        .invoke(
            "memory.chat.record",
            record("turn-1", "first question", "first answer"),
        )
        .await
        .expect("first turn records");
    broker
        .invoke(
            "memory.chat.record",
            record("turn-2", "second question", "second answer"),
        )
        .await
        .expect("second turn records");

    let recent = broker
        .invoke(
            "memory.chat.recent",
            json!({
                "operation": "recent",
                "last": 2,
                "maxLookbackTurns": 64,
                "maxRecentTurns": 64,
                "maxResultBytes": 65536,
            }),
        )
        .await
        .expect("a later invocation reads what the earlier ones committed");

    let turns = recent["turns"].as_array().expect("turns array");
    assert_eq!(turns.len(), 2, "{recent}");
    assert_eq!(turns[0]["user"], "first question");
    assert_eq!(turns[1]["assistant"], "second answer");

    assert_eq!(generations(broker.storage_root()), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_subjects_do_not_share_a_namespace() {
    let first = memory_chat().await;
    first
        .invoke("memory.chat.record", record("turn-1", "private", "answer"))
        .await
        .expect("records");

    let second = FakeBroker::builder()
        .component(provider_fixture("memory-chat-provider.wasm"))
        .provider("memory-chat")
        .storage(StorageInterface::Jsonl, StorageAccess::ReadWrite)
        .subject("slack.t0123abc.udifferent")
        .build()
        .await
        .expect("memory-chat loads");

    let recent = second
        .invoke(
            "memory.chat.recent",
            json!({
                "operation": "recent",
                "last": 2,
                "maxLookbackTurns": 64,
                "maxRecentTurns": 64,
                "maxResultBytes": 65536,
            }),
        )
        .await
        .expect("an empty namespace reads cleanly");
    assert_eq!(recent["turns"].as_array().expect("turns array").len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_word_renders_its_help_page_and_proposes() {
    let broker = FakeBroker::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .build()
        .await
        .expect("cli-probe loads");

    let outcome = broker
        .run_command("probe", &["--help".to_owned()], None)
        .await
        .expect("help renders");
    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = outcome
    else {
        panic!("expected rendered help, got {outcome:?}");
    };
    assert_eq!(status, 0);
    assert!(stdout.starts_with("Usage: probe <COMMAND>\n"), "{stdout:?}");
    assert!(
        stdout.contains("\n  reverse  Reverse the text\n"),
        "{stdout:?}"
    );
    assert!(stderr.is_empty(), "{stderr:?}");

    let outcome = broker
        .run_command(
            "probe",
            &["reverse".to_owned(), "-".to_owned()],
            Some("abc"),
        )
        .await
        .expect("a piped value proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            secret_use: None,
            capability: "cli-probe.reverse".parse().expect("capability"),
            input: json!({"text": "abc"}),
        }
    );
    let output = broker
        .invoke("cli-probe.reverse", json!({"text": "abc"}))
        .await
        .expect("the proposal runs");
    assert_eq!(output, json!({"text": "cba"}));

    let error = broker
        .run_command("recall", &[], None)
        .await
        .expect_err("a word the component did not declare is refused");
    assert!(
        matches!(error, FakeBrokerError::Host(BrokerHostError::UnknownCommandWord { ref word }) if word == "recall"),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_free_component_needs_no_storage() {
    let broker = FakeBroker::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .build()
        .await
        .expect("cli-probe loads");

    let output = broker
        .invoke("cli-probe.upper", json!({"text": "hello"}))
        .await
        .expect("cli-probe runs");
    assert_eq!(output["text"], "HELLO");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_declared_failure_is_distinguishable_from_a_host_refusal() {
    let broker = memory_chat().await;

    let error = broker
        .invoke(
            "memory.chat.recent",
            json!({
                "operation": "recent",
                "last": 0,
                "maxLookbackTurns": 64,
                "maxRecentTurns": 64,
                "maxResultBytes": 65536,
            }),
        )
        .await
        .expect_err("last: 0 is refused by the provider");

    assert_eq!(
        error.provider_failure().map(|(code, _)| code),
        Some("invalid-input"),
        "{error}"
    );

    let refused = broker
        .invoke("memory.chat.nonexistent", json!({}))
        .await
        .expect_err("an undeclared capability has no route");
    assert_eq!(refused.provider_failure(), None, "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_component_names_the_path() {
    let error = FakeBroker::builder()
        .component("definitely-not-here.wasm")
        .provider("nobody")
        .build()
        .await
        .expect_err("a missing component cannot load");

    assert!(
        matches!(error, FakeBrokerError::ComponentMissing { .. }),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_builder_missing_its_component_or_provider_says_which() {
    let error = FakeBroker::builder()
        .provider("cli-probe")
        .build()
        .await
        .expect_err("no component");
    assert!(matches!(error, FakeBrokerError::NoComponent), "{error}");

    let error = FakeBroker::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .build()
        .await
        .expect_err("no provider");
    assert!(matches!(error, FakeBrokerError::NoProvider), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_storage_quota_refuses_the_write_the_defaults_accept() {
    let narrowed = FakeBroker::builder()
        .component(provider_fixture("memory-chat-provider.wasm"))
        .provider("memory-chat")
        .storage(StorageInterface::Jsonl, StorageAccess::ReadWrite)
        .storage_limits(StorageLimits {
            max_write_bytes_per_call: 1,
            max_write_bytes_per_invocation: 1,
            ..StorageLimits::default()
        })
        .build()
        .await
        .expect("memory-chat loads under a one-byte write budget");

    let error = narrowed
        .invoke("memory.chat.record", record("turn-1", "question", "answer"))
        .await
        .expect_err("a one-byte write budget cannot record a turn");
    let FakeBrokerError::Invocation(failure) = &error else {
        panic!("expected an invocation failure: {error:?}");
    };
    assert!(
        matches!(
            failure.error.as_ref(),
            BrokerHostError::StorageCallRejected { reason, .. } if *reason == "quota"
        ),
        "{error:?}"
    );
    assert_eq!(error.provider_failure(), None, "{error}");

    memory_chat()
        .await
        .invoke("memory.chat.record", record("turn-1", "question", "answer"))
        .await
        .expect("the default storage limits accept the same turn");
}

#[tokio::test(flavor = "multi_thread")]
async fn authority_bound_continuity_is_selectable_and_holds_one_generation_here() {
    let broker = FakeBroker::builder()
        .component(provider_fixture("memory-chat-provider.wasm"))
        .provider("memory-chat")
        .storage(StorageInterface::Jsonl, StorageAccess::ReadWrite)
        .continuity(ContinuityPolicy::AuthorityBound)
        .build()
        .await
        .expect("memory-chat loads under AuthorityBound continuity");

    broker
        .invoke("memory.chat.record", record("turn-1", "first", "answer"))
        .await
        .expect("first turn records");
    broker
        .invoke("memory.chat.record", record("turn-2", "second", "answer"))
        .await
        .expect("second turn records");

    let recent = broker
        .invoke(
            "memory.chat.recent",
            json!({
                "operation": "recent",
                "last": 2,
                "maxLookbackTurns": 64,
                "maxRecentTurns": 64,
                "maxResultBytes": 65536,
            }),
        )
        .await
        .expect("a later invocation reads what the earlier ones committed");

    let turns = recent["turns"].as_array().expect("turns array");
    assert_eq!(turns.len(), 2, "{recent}");
    assert_eq!(generations(broker.storage_root()), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_fuel_ceiling_stops_the_guest() {
    FakeBroker::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .host_limits(BrokerHostLimits::default())
        .build()
        .await
        .expect("cli-probe loads under the default host limits");

    let error = FakeBroker::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .host_limits(BrokerHostLimits {
            fuel: 1,
            ..BrokerHostLimits::default()
        })
        .build()
        .await
        .expect_err("one unit of fuel cannot load the component");

    let source = match &error {
        FakeBrokerError::Host(
            BrokerHostError::Instantiate { source, .. } | BrokerHostError::Describe { source, .. },
        ) => source,
        _ => panic!("expected load-time fuel exhaustion, got {error:?}"),
    };
    assert_eq!(
        source.root_cause().to_string(),
        "wasm trap: all fuel consumed by WebAssembly",
        "expected an out-of-fuel root cause, got {error:?}"
    );
}

fn generations(root: &std::path::Path) -> usize {
    let namespaces = root.join("namespaces");
    let Ok(entries) = std::fs::read_dir(&namespaces) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|namespace| namespace.path().is_dir())
        .map(|namespace| {
            std::fs::read_dir(namespace.path())
                .into_iter()
                .flatten()
                .flatten()
                .filter(|generation| generation.path().is_dir())
                .count()
        })
        .sum()
}
