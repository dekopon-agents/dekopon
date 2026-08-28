//! Tests for the harness itself, driven against the checked-in example components.
//!
//! Every test here is `multi_thread`: the storage path dispatches to `spawn_blocking`, and a
//! current-thread runtime deadlocks waiting for a namespace lease.

use dekopon_provider_sdk_testkit::{
    BrokerHostError, BrokerHostLimits, ContinuityPolicy, FakeBroker, FakeBrokerError,
    StorageAccess, StorageInterface, StorageLimits,
};
use dekopon_test_support::{provider_fixture, snapshot_tree};
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
        .component(provider_fixture("memory-chat-provider.wasm"))
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
        .component(provider_fixture("echo-provider.wasm"))
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
        .component(provider_fixture("echo-provider.wasm"))
        .build()
        .await
        .expect_err("no provider");
    assert!(matches!(error, FakeBrokerError::NoProvider), "{error}");
}

/// `compile_cache` is the crate's headline performance affordance — the README tells a suite that
/// loads the same component repeatedly to pass one — so it has to actually write something the
/// second load can read back, rather than accepting a path and ignoring it.
#[tokio::test(flavor = "multi_thread")]
async fn a_compile_cache_directory_is_written_and_reused() {
    let cache = tempfile::tempdir().expect("compile cache directory");
    assert_eq!(files(cache.path()), 0, "the cache starts empty");

    for attempt in ["first", "second"] {
        let broker = FakeBroker::builder()
            .component(provider_fixture("echo-provider.wasm"))
            .provider("echo")
            .compile_cache(cache.path())
            .build()
            .await
            .expect("echo loads against a compile cache");
        let output = broker
            .invoke("echo.echo", json!({"message": attempt}))
            .await
            .expect("echo runs");
        assert_eq!(output["message"], attempt);
    }

    assert!(
        files(cache.path()) > 0,
        "loading the same component twice left the compile cache empty"
    );
}

/// The crate's strongest claim is that a quota tripped here is a quota production would have
/// tripped, and `storage_limits` is the knob that makes that testable. A one-byte write budget is
/// refused by the real storage host — a `StorageCallRejected` with the stable class `quota`, not a
/// provider-declared error and not a fake. The control proves the refusal came from the narrowing.
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
    // The host refused the call; the provider never got to declare anything.
    assert_eq!(error.provider_failure(), None, "{error}");

    // The same turn against the defaults records, which is what makes the refusal above the
    // narrowing rather than the fixture.
    memory_chat()
        .await
        .invoke("memory.chat.record", record("turn-1", "question", "answer"))
        .await
        .expect("the default storage limits accept the same turn");
}

/// `continuity` is the one builder default this crate deliberately overrides, so the override has
/// to be reachable — and what it actually does here has to be stated rather than assumed.
///
/// `AuthorityBound` mints a fresh non-reusing generation whenever the *effective authority
/// commitment* changes. This harness holds that commitment fixed across invocations, so it never
/// changes and the policy addresses one generation exactly like `Stable`: successive calls still
/// read each other's writes. That is the claim `FakeBrokerBuilder::default` makes in a comment and
/// the README's continuity bullet makes in prose, and this is the test that keeps both honest: the
/// authority surface is a literal in `FakeBroker::invoke` with no builder knob on it, so if the
/// harness ever grows a varying one this assertion fails and all three need revisiting together.
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

/// `host_limits` narrows the Wasmtime ceilings the same way `storage_limits` narrows storage, and
/// the fuel ceiling is real enough that one unit cannot carry a component through its own
/// `describe` — the ceiling bites at load, before any invocation exists to refuse.
#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_fuel_ceiling_stops_the_guest() {
    // The control: the same component, the same builder, the default ceilings.
    FakeBroker::builder()
        .component(provider_fixture("echo-provider.wasm"))
        .provider("echo")
        .host_limits(BrokerHostLimits::default())
        .build()
        .await
        .expect("echo loads under the default host limits");

    let error = FakeBroker::builder()
        .component(provider_fixture("echo-provider.wasm"))
        .provider("echo")
        .host_limits(BrokerHostLimits {
            fuel: 1,
            ..BrokerHostLimits::default()
        })
        .build()
        .await
        .expect_err("one unit of fuel cannot describe the component");

    assert!(
        matches!(
            error,
            FakeBrokerError::Host(BrokerHostError::Describe { .. })
        ),
        "{error:?}"
    );
}

/// Counts every regular file under a directory, recursively.
fn files(root: &std::path::Path) -> usize {
    snapshot_tree(root)
        .into_iter()
        .filter(|entry| !entry.is_dir)
        .count()
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
