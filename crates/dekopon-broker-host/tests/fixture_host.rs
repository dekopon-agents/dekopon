#![allow(clippy::unwrap_used)]

mod fixture;

use dekopon_test_support::provider_fixture;
use fixture::{
    BrokerHostError, BrokerHostLimits, CommandRunOutcome, FixtureHost, FixtureHostError,
    StorageAccess, StorageInterface, StorageLimits,
};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn a_fetched_0_3_0_memory_chat_component_is_refused_at_load() {
    let error = FixtureHost::builder()
        .component(provider_fixture("memory-chat-provider.wasm"))
        .provider("memory-chat")
        .storage(StorageInterface::Jsonl, StorageAccess::ReadWrite)
        .build()
        .await
        .expect_err("0.3.0 invoke returns a string, not an exit status");
    assert!(
        matches!(
            error,
            FixtureHostError::Host(BrokerHostError::Instantiate { .. })
        ),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_a_storage_backed_component_against_a_real_storage_host() {
    let broker = FixtureHost::builder()
        .component(provider_fixture("storage-probe-provider.wasm"))
        .provider("storage-probe")
        .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
        .build()
        .await
        .expect("storage-probe loads");

    let (output, stdout) = broker
        .invoke_full("storage-probe.run", json!({}))
        .await
        .expect("the durable-files conformance sequence completes");

    assert_eq!(stdout["clocksCalled"], true);
    assert_eq!(stdout["entropyBytes"], 32);
    assert_eq!(stdout["identityNonzero"], true);
    let evidence = output
        .storage
        .expect("a storage-backed invocation carries evidence");
    assert!(evidence.operations > 0, "{evidence:?}");
    assert_eq!(evidence.quota_denials, 0, "{evidence:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_word_renders_help_and_proposes_piped_input_without_reading_it() {
    let broker = FixtureHost::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .build()
        .await
        .expect("cli-probe loads");
    let outcome = broker
        .run_command("probe", &["--help".to_owned()], false)
        .await
        .unwrap();
    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = outcome
    else {
        panic!("expected help, got {outcome:?}");
    };
    assert_eq!(status, 0);
    assert!(stdout.starts_with("Usage: probe <COMMAND>\n"), "{stdout:?}");
    assert!(
        stdout.contains("\n  reverse  Reverse the text\n"),
        "{stdout:?}"
    );
    assert!(stderr.is_empty(), "{stderr:?}");

    let outcome = broker
        .run_command("probe", &["reverse".to_owned(), "-".to_owned()], true)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            secret_use: None,
            capability: "cli-probe.reverse".parse().unwrap(),
            input: json!({"text":"", "piped":true}),
        }
    );
    assert_eq!(
        broker
            .invoke("cli-probe.reverse", json!({"text":"abc"}))
            .await
            .unwrap(),
        json!({"text":"cba"})
    );
    let error = broker.run_command("recall", &[], false).await.unwrap_err();
    assert!(
        matches!(error, FixtureHostError::Host(BrokerHostError::UnknownCommandWord { ref word }) if word == "recall"),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stdio_only_component_needs_no_storage() {
    let broker = FixtureHost::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .build()
        .await
        .unwrap();
    assert_eq!(
        broker
            .invoke("cli-probe.upper", json!({"text":"hello"}))
            .await
            .unwrap()["text"],
        "HELLO"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_exit_is_distinct_from_an_undeclared_capability_refusal() {
    let broker = FixtureHost::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .build()
        .await
        .unwrap();
    let error = broker
        .invoke("cli-probe.upper", json!({"text":1}))
        .await
        .unwrap_err();
    assert_eq!(
        error.provider_failure().map(|(status, _)| status),
        Some(2),
        "{error:?}"
    );
    let refused = broker
        .invoke("cli-probe.nonexistent", json!({}))
        .await
        .unwrap_err();
    assert_eq!(refused.provider_failure(), None, "{refused:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_component_names_the_path() {
    let error = FixtureHost::builder()
        .component("definitely-not-here.wasm")
        .provider("nobody")
        .build()
        .await
        .unwrap_err();
    assert!(
        matches!(error, FixtureHostError::ComponentMissing { .. }),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_builder_missing_its_component_or_provider_says_which() {
    let error = FixtureHost::builder()
        .provider("cli-probe")
        .build()
        .await
        .unwrap_err();
    assert!(matches!(error, FixtureHostError::NoComponent), "{error}");
    let error = FixtureHost::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .build()
        .await
        .unwrap_err();
    assert!(matches!(error, FixtureHostError::NoProvider), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_storage_quota_refuses_the_write_the_defaults_accept() {
    let narrowed = FixtureHost::builder()
        .component(provider_fixture("storage-probe-provider.wasm"))
        .provider("storage-probe")
        .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
        .storage_limits(StorageLimits {
            max_write_bytes_per_call: 1,
            max_write_bytes_per_invocation: 1,
            ..StorageLimits::default()
        })
        .build()
        .await
        .unwrap();
    let error = narrowed
        .invoke("storage-probe.run", json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(error, FixtureHostError::Invocation(_)),
        "{error:?}"
    );
    let default = FixtureHost::builder()
        .component(provider_fixture("storage-probe-provider.wasm"))
        .provider("storage-probe")
        .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
        .build()
        .await
        .unwrap();
    assert_eq!(
        default
            .invoke("storage-probe.run", json!({}))
            .await
            .unwrap()["identityNonzero"],
        true
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_fuel_ceiling_stops_the_guest() {
    FixtureHost::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .build()
        .await
        .unwrap();
    let error = FixtureHost::builder()
        .component(provider_fixture("cli-probe-provider.wasm"))
        .provider("cli-probe")
        .host_limits(BrokerHostLimits {
            fuel: 1,
            ..BrokerHostLimits::default()
        })
        .build()
        .await
        .unwrap_err();
    let FixtureHostError::Host(
        BrokerHostError::Instantiate { source, .. } | BrokerHostError::Describe { source, .. },
    ) = &error
    else {
        panic!("expected load-time fuel exhaustion, got {error:?}");
    };
    assert_eq!(
        source.root_cause().to_string(),
        "wasm trap: all fuel consumed by WebAssembly"
    );
}
