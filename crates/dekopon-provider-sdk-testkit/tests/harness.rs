//! Tests for the harness itself, driven against the checked-in example components.
//!
//! Every test here is `multi_thread`: the storage path dispatches to `spawn_blocking`, and a
//! current-thread runtime deadlocks waiting for a namespace lease.

use std::path::PathBuf;

use dekopon_provider_sdk_testkit::{FakeBroker, FakeBrokerError, StorageAccess, StorageInterface};
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/providers")
        .join(name)
}

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
        .component(fixture("memory-chat-provider.wasm"))
        .provider("memory-chat")
        .storage(StorageInterface::Jsonl, StorageAccess::ReadWrite)
        .build()
        .await
        .expect("memory-chat loads")
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_a_storage_backed_component_against_a_real_storage_host() {
    let broker = FakeBroker::builder()
        .component(fixture("storage-probe-provider.wasm"))
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
    // Storage evidence exists because a transaction actually ran, which is the part a hand-written
    // fake would have had to invent.
    let evidence = output
        .storage
        .expect("a storage-backed invocation carries evidence");
    assert!(evidence.operations > 0, "{evidence:?}");
    assert_eq!(evidence.quota_denials, 0, "{evidence:?}");
}

/// The property the whole harness exists for: separate invocations reach one durable namespace.
///
/// Each invocation gets a fresh id and a freshly minted, separately consumed grant; only the scope
/// material around them is held constant. That constancy is what makes the third call able to read
/// what the first two committed, and it is the part a caller would otherwise have to know to
/// reproduce by hand.
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

    // One namespace, one generation: no invocation minted a fresh one behind the test's back.
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
        .component(fixture("memory-chat-provider.wasm"))
        .provider("memory-chat")
        .storage(StorageInterface::Jsonl, StorageAccess::ReadWrite)
        .subject("slack.t0123abc.udifferent")
        .build()
        .await
        .expect("memory-chat loads");

    // A different subject is a different namespace even within one storage host; this one is also
    // a different temporary root, so the read must simply find nothing rather than fail.
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
async fn an_import_free_component_needs_no_storage() {
    let broker = FakeBroker::builder()
        .component(fixture("echo-provider.wasm"))
        .provider("echo")
        .build()
        .await
        .expect("echo loads");

    let output = broker
        .invoke("echo.echo", json!({"message": "hello"}))
        .await
        .expect("echo runs");
    assert_eq!(output["message"], "hello");
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

    // A host refusal is a different shape, and reports no provider code.
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
        .provider("echo")
        .build()
        .await
        .expect_err("no component");
    assert!(matches!(error, FakeBrokerError::NoComponent), "{error}");

    let error = FakeBroker::builder()
        .component(fixture("echo-provider.wasm"))
        .build()
        .await
        .expect_err("no provider");
    assert!(matches!(error, FakeBrokerError::NoProvider), "{error}");
}

/// Counts namespace generations on disk: `<root>/namespaces/<namespace>/<generation>/`.
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
