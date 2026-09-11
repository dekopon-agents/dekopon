//! What the host's own spans say about stores and component instantiations.
//!
//! Imports are resolved into one `InstancePre` per provider at load, so every description, command
//! run, and invocation builds one fresh store and instantiates the component in it exactly once. A
//! second instantiation inside one operation — a call path that started rebuilding instances per
//! call — is invisible without a number an operator can read, so `stores` and `instantiations` ride
//! `provider.describe`, `provider.run_command`, and `provider.invoke`. This file is what keeps them
//! honest.
//!
//! It lives in its own test binary because `tracing` resolves per-callsite interest against the
//! global dispatcher: a sibling test reaching these callsites with no subscriber installed can
//! disable them for the whole process. The tests hold one asynchronous mutex for the same reason —
//! `cargo test` runs them as threads in one process against one global capture, and two loads
//! interleaved in it would be indistinguishable from one load that instantiated twice.

use std::{path::PathBuf, sync::OnceLock};

use dekopon_broker_host::{
    BrokerHostError, BrokerHostLimits, BrokerProviderRegistry, CommandRunOutcome,
};
use dekopon_capability::{
    AuthorizedInvocation, ExecutionConstraints, ProposedInvocation, broker::AuthorizationGate,
};
use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};
use dekopon_test_support::{CaptureLayer, provider_fixture};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

/// Serializes the tests against the one global capture a process can install.
static SEQUENTIAL: Mutex<()> = Mutex::const_new(());

/// The process-wide capture, installed on first use.
fn capture() -> CaptureLayer {
    static CAPTURE: OnceLock<CaptureLayer> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = CaptureLayer::workspace();
            tracing_subscriber::registry().with(capture.clone()).init();
            capture
        })
        .clone()
}

/// Every field set captured for one span name, at creation and at each later recording.
fn recordings(capture: &CaptureLayer, name: &str) -> Vec<String> {
    capture
        .spans()
        .into_iter()
        .filter(|(span, _)| *span == name)
        .map(|(_, fields)| fields)
        .collect()
}

/// Every numeric value recorded for `field` onto spans named `name`, in arrival order.
///
/// `Span::record` takes one field at a time, so each recording renders as exactly ` field=value`.
fn recorded(capture: &CaptureLayer, name: &str, field: &str) -> Vec<u64> {
    let marker = format!(" {field}=");
    recordings(capture, name)
        .iter()
        .filter_map(|fields| fields.split_once(&marker))
        .map(|(_, rest)| {
            rest.split(' ')
                .next()
                .unwrap_or(rest)
                .parse()
                .expect("a numeric span field")
        })
        .collect()
}

/// Asserts that `name` ran `count` times, each in one fresh store with one instantiation.
fn assert_one_store_each(capture: &CaptureLayer, name: &str, count: usize) {
    let rendered = capture.spans_text();
    assert_eq!(
        recorded(capture, name, "stores"),
        vec![1; count],
        "{name} must build one fresh store per operation:\n{rendered}"
    );
    assert_eq!(
        recorded(capture, name, "instantiations"),
        vec![1; count],
        "{name} must instantiate the component exactly once per operation:\n{rendered}"
    );
}

fn authorized(provider: &str, capability: CapabilityId, input: Value) -> AuthorizedInvocation {
    let proposal = ProposedInvocation::new(
        "invoke-test"
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability,
        Actor::Agent {
            agent: "provider-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
        "0000000000000000000000000000f1c7"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        input,
    );
    AuthorizationGate::new()
        .authorize(
            proposal,
            provider.parse().expect("valid provider fixture"),
            "decision-test".to_owned(),
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid principal fixture"),
            "policy-test".to_owned(),
            ExecutionConstraints {
                timeout_ms: 5_000,
                max_output_bytes: 4_096,
                http: None,
                storage: None,
                secret_use: None,
            },
        )
        .expect("test broker authorizes bounded fixture")
}

fn probe() -> PathBuf {
    provider_fixture("memory-reservation-probe-provider.wasm")
}

/// Loading a command-word provider proves the export statically instead of instantiating twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_load_describes_once_and_a_run_adds_exactly_one_more() {
    let _sequential = SEQUENTIAL.lock().await;
    let capture = capture();
    capture.clear();

    let registry = BrokerProviderRegistry::load([probe()], BrokerHostLimits::default())
        .await
        .expect("command-word provider loads");
    assert_eq!(registry.command_words(), vec!["recall".to_owned()]);
    assert_one_store_each(&capture, "provider.describe", 1);
    assert!(
        recordings(&capture, "provider.run_command").is_empty(),
        "a load runs no command word:\n{}",
        capture.spans_text()
    );

    // The first run is the second instantiation, in its own fresh store; this hand-rolled
    // `run-command` guest ignores the piped value rather than refusing it.
    let outcome = registry
        .run_command("recall", &["recall".to_owned()], Some("piped"))
        .await
        .expect("the probe rewrites its word");
    assert!(
        matches!(outcome, CommandRunOutcome::Proposed { .. }),
        "{outcome:?}"
    );
    assert_one_store_each(&capture, "provider.describe", 1);
    assert_one_store_each(&capture, "provider.run_command", 1);
}

/// A word plus its piped value beyond the input bound is refused before a store exists.
#[tokio::test(flavor = "multi_thread")]
async fn command_input_beyond_the_bound_is_refused_before_a_store_exists() {
    let _sequential = SEQUENTIAL.lock().await;
    let capture = capture();
    capture.clear();

    let registry = BrokerProviderRegistry::load(
        [probe()],
        BrokerHostLimits {
            max_input_bytes: 8,
            ..BrokerHostLimits::default()
        },
    )
    .await
    .expect("command-word provider loads");

    let error = registry
        .run_command("recall", &["recall".to_owned()], Some("more than eight"))
        .await
        .expect_err("argv plus stdin exceed the bound");
    assert!(
        matches!(
            error,
            BrokerHostError::CommandInputTooLarge {
                length: 21,
                maximum: 8,
                ..
            }
        ),
        "{error:?}"
    );

    // Only the load's describe built a store. The refused word has a span — the host was asked —
    // and that span carries no store and no instantiation, which is the refusal being before both.
    assert_one_store_each(&capture, "provider.describe", 1);
    let refused = recordings(&capture, "provider.run_command");
    assert!(!refused.is_empty(), "the refusal is still a span");
    assert!(
        refused
            .iter()
            .all(|fields| !fields.contains(" stores=") && !fields.contains(" instantiations=")),
        "a word refused on input size reaches no store:\n{refused:?}"
    );
}

/// Describe, every command run, and the invocation each instantiate once, in one store each.
#[tokio::test(flavor = "multi_thread")]
async fn every_operation_instantiates_the_component_exactly_once() {
    let _sequential = SEQUENTIAL.lock().await;
    let capture = capture();
    capture.clear();

    let registry = BrokerProviderRegistry::load(
        [provider_fixture("cli-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("command-line provider loads");

    registry
        .run_command("probe", &["--help".to_owned()], None)
        .await
        .expect("help renders");
    let capability = "cli-probe.count"
        .parse::<CapabilityId>()
        .expect("capability");
    let outcome = registry
        .run_command(
            "probe",
            &["count".to_owned(), "-".to_owned()],
            Some("héllo"),
        )
        .await
        .expect("a piped value proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            capability: capability.clone(),
            input: json!({"text": "héllo"}),
        }
    );
    let output = registry
        .invoke(
            authorized("cli-probe", capability, json!({"text": "héllo"})),
            None,
        )
        .await
        .expect("the proposed capability runs");
    assert_eq!(output.output, json!({"characters": 5}));

    assert_one_store_each(&capture, "provider.describe", 1);
    assert_one_store_each(&capture, "provider.run_command", 2);
    assert_one_store_each(&capture, "provider.invoke", 1);

    // What the guest actually burned, read back from the store. A run that recorded nothing here
    // would mean the fuel reading was lost, not that the component executed for free.
    let fuel = recorded(&capture, "provider.invoke", "fuel.consumed");
    assert_eq!(fuel.len(), 1, "one invocation reports fuel once: {fuel:?}");
    assert!(fuel[0] > 0, "a real invocation burns fuel: {fuel:?}");
}
