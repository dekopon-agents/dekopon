use std::path::PathBuf;

use dekopon_provider_host::{
    CommandRunOutcome, HostLimits, HostOptions, PROVIDER_WIT, ProviderHostError, ProviderRegistry,
};
use dekopon_provider_sdk::host::{
    DEFAULT_MAX_INSTANCES, DEFAULT_MAX_MEMORIES, DEFAULT_MAX_MEMORY_BYTES,
    DEFAULT_MAX_TABLE_ELEMENTS, DEFAULT_MAX_TABLES,
};
use serde_json::json;

fn provider_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/providers/echo-provider.wasm")
}

fn imported_provider_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
}

fn load_echo() -> ProviderRegistry {
    ProviderRegistry::load([provider_path()], HostLimits::default())
        .expect("Rust echo provider loads")
}

#[test]
fn loads_routes_and_invokes_the_rust_provider_component() {
    let registry = load_echo();
    let capability = "echo.echo".parse().expect("valid capability fixture");
    let input = json!({"message": "hello"});

    let first = registry
        .invoke(&capability, &input)
        .expect("first invocation succeeds");
    let second = registry
        .invoke(&capability, &json!({"message": "again"}))
        .expect("second invocation succeeds");

    assert_eq!(first.provider.as_str(), "echo");
    assert_eq!(first.output, input);
    assert_eq!(second.output, json!({"message": "again"}));
    assert_eq!(registry.manifests().len(), 1);
    assert_eq!(registry.capabilities().count(), 5);
}

#[test]
fn invokes_the_checked_in_text_transform_capabilities() {
    let registry = load_echo();
    let cases = [
        ("echo.reverse", "Hello 🦀", "🦀 olleH"),
        ("echo.upcase", "Hello, Straße!", "HELLO, STRASSE!"),
        ("echo.downcase", "Hello, WORLD!", "hello, world!"),
        ("echo.ransom-case", "Hello, World!", "hElLo, WoRlD!"),
    ];

    for (capability, input, expected) in cases {
        let capability = capability.parse().expect("valid capability fixture");
        let output = registry
            .invoke(&capability, &json!({"message": input}))
            .expect("text transformation succeeds");
        assert_eq!(output.output, json!({"message": expected}));
    }
}

#[test]
fn rejects_malformed_text_transform_inputs() {
    let registry = load_echo();
    let capability = "echo.upcase".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!({"message": 42}))
        .expect_err("invalid message input must fail");

    assert!(matches!(
        error,
        ProviderHostError::ProviderFailure { ref code, .. } if code == "invalid-input"
    ));
}

#[test]
fn reports_every_duplicate_in_one_conflict_report() {
    let path = provider_path();
    let error = ProviderRegistry::load([path.clone(), path], HostLimits::default())
        .expect_err("duplicate providers must fail");

    let ProviderHostError::ConflictingProviders { report } = error else {
        panic!("duplicate components must produce one aggregated conflict report");
    };
    assert_eq!(
        report
            .providers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["echo".to_owned()]
    );
    // Every collision, not just the first: fixing a --provider list should take one run.
    assert_eq!(
        report.capabilities.len(),
        5,
        "all five echo capabilities collide: {report}"
    );
    assert!(report.command_words.is_empty());
    assert_eq!(report.len(), 6);
    assert!(report.to_string().contains("6 provider conflict(s)"));
}

#[test]
fn enforces_serialized_input_bounds_before_execution() {
    let limits = HostLimits {
        max_input_bytes: 8,
        ..HostLimits::default()
    };
    let registry =
        ProviderRegistry::load([provider_path()], limits).expect("Rust echo provider loads");
    let capability = "echo.echo".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!({"message": "too large"}))
        .expect_err("oversized input must fail");

    assert!(matches!(error, ProviderHostError::InputTooLarge { .. }));
}

#[test]
fn rejects_non_object_inputs_before_entering_wasm() {
    let registry = load_echo();
    let capability = "echo.echo".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!("not an object"))
        .expect_err("provider inputs must be objects");

    assert!(matches!(error, ProviderHostError::InputNotObject { .. }));
}

#[test]
fn reports_unknown_capabilities_without_entering_wasm() {
    let registry = load_echo();
    let capability = "missing.read".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!({}))
        .expect_err("unknown capability must fail");

    assert!(matches!(error, ProviderHostError::UnknownCapability { .. }));
}

#[test]
fn immediate_host_rejects_components_requiring_privileged_imports() {
    for fixture in [
        "http-probe-provider.wasm",
        "jsonplaceholder-provider.wasm",
        "memory-chat-provider.wasm",
        "storage-probe-provider.wasm",
    ] {
        let error =
            ProviderRegistry::load([imported_provider_path(fixture)], HostLimits::default())
                .expect_err("the immediate linker must not satisfy HTTP imports");

        assert!(matches!(error, ProviderHostError::Instantiate { .. }));
    }
}

#[test]
fn host_and_guest_sdk_use_the_same_wit_contract() {
    assert_eq!(PROVIDER_WIT, dekopon_provider_sdk::PROVIDER_WIT);
}

/// A warm compilation cache serves a later process from the same directory.
///
/// The registry is per-process, so the only evidence a cache was used at all is that the cold
/// load wrote entries into an empty directory and a second, independent registry built from those
/// entries still routes and invokes.
#[test]
fn a_persistent_compilation_cache_serves_a_second_load() {
    let directory = tempfile::tempdir().expect("cache directory");
    let options = HostOptions {
        compile_cache_dir: Some(directory.path().to_path_buf()),
    };

    let cold =
        ProviderRegistry::load_with_options([provider_path()], HostLimits::default(), &options)
            .expect("cold load populates the cache");
    assert_eq!(cold.manifests().len(), 1);
    assert!(
        std::fs::read_dir(directory.path())
            .expect("cache directory is readable")
            .next()
            .is_some(),
        "a cold compile must write cache entries"
    );

    let warm =
        ProviderRegistry::load_with_options([provider_path()], HostLimits::default(), &options)
            .expect("warm load reads the cache");
    let capability = "echo.echo".parse().expect("valid capability fixture");
    let output = warm
        .invoke(&capability, &json!({"message": "warm"}))
        .expect("a cached component still invokes");

    assert_eq!(output.output, json!({"message": "warm"}));
}

#[test]
fn rejects_zero_execution_limits_before_compiling_components() {
    let limits = HostLimits {
        fuel: 0,
        ..HostLimits::default()
    };
    let error =
        ProviderRegistry::load([provider_path()], limits).expect_err("zero limits must fail");

    assert!(matches!(
        error,
        ProviderHostError::InvalidLimit { name: "fuel" }
    ));
}

#[test]
fn bounds_table_instance_and_memory_counts_alongside_linear_memory() {
    assert_eq!(
        HostLimits::default(),
        HostLimits {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_table_elements: DEFAULT_MAX_TABLE_ELEMENTS,
            max_instances: DEFAULT_MAX_INSTANCES,
            max_tables: DEFAULT_MAX_TABLES,
            max_memories: DEFAULT_MAX_MEMORIES,
            ..HostLimits::default()
        }
    );

    // Each ceiling reaches the store, so a zero is refused by name before any component compiles.
    for (name, limits) in [
        (
            "max_table_elements",
            HostLimits {
                max_table_elements: 0,
                ..HostLimits::default()
            },
        ),
        (
            "max_instances",
            HostLimits {
                max_instances: 0,
                ..HostLimits::default()
            },
        ),
        (
            "max_tables",
            HostLimits {
                max_tables: 0,
                ..HostLimits::default()
            },
        ),
        (
            "max_memories",
            HostLimits {
                max_memories: 0,
                ..HostLimits::default()
            },
        ),
    ] {
        let error = ProviderRegistry::load([provider_path()], limits)
            .expect_err("zero limits must fail before compiling");
        assert!(
            matches!(error, ProviderHostError::InvalidLimit { name: reported } if reported == name),
            "{name} must be rejected by name, got {error}"
        );
    }
}

#[test]
fn the_table_element_ceiling_reaches_the_store() {
    // Not decorative: a store that may hold one table element cannot instantiate the checked-in
    // component at all, which is the same wall an unbounded `table.grow` now hits.
    let limits = HostLimits {
        max_table_elements: 1,
        ..HostLimits::default()
    };

    let error = ProviderRegistry::load([provider_path()], limits)
        .expect_err("a one-element table ceiling must stop instantiation");

    assert!(matches!(error, ProviderHostError::Instantiate { .. }));
}

/// A component built against the frozen `dekopon:provider@0.2.0` `provider-commands` world runs
/// its word through the same `run_command` path a `run-command` guest takes: the legacy
/// `resolved` answer arrives as a proposal and a decline as a failed outcome, never a host error.
#[test]
fn a_legacy_resolve_command_provider_runs_through_the_same_path() {
    let registry = ProviderRegistry::load(
        [imported_provider_path("provider-v0-2-compat-provider.wasm")],
        HostLimits::default(),
    )
    .expect("historical command-word provider loads");

    // The legacy export has no stdin parameter, so the piped value is dropped by contract.
    let outcome = registry
        .run_command("compat", &["echo".to_owned()], Some("piped"))
        .expect("the historical rewrite still runs");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            capability: "provider-v0-2-compat.echo"
                .parse()
                .expect("valid capability fixture"),
            input: json!({}),
        }
    );

    let outcome = registry
        .run_command("compat", &["bogus".to_owned()], None)
        .expect("a decline is an outcome, not a host error");
    assert!(
        matches!(outcome, CommandRunOutcome::Failed { ref error } if error.code == "usage"),
        "{outcome:?}"
    );
}

/// The checked-in `cli-probe` component exports `run-command`, so this is where the typed
/// `(list<string>, option<string>) -> string` lowering, the piped value reaching the guest, and
/// every answer shape are proven against a real guest rather than an adapted legacy one.
#[test]
fn a_run_command_provider_renders_help_reads_stdin_and_declines() {
    let registry = ProviderRegistry::load(
        [imported_provider_path("cli-probe-provider.wasm")],
        HostLimits::default(),
    )
    .expect("command-line provider loads");
    let words = registry
        .command_words_by_provider()
        .into_iter()
        .map(|(provider, words)| (provider.to_string(), words.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(
        words,
        vec![("cli-probe".to_owned(), vec!["probe".to_owned()])]
    );

    let outcome = registry
        .run_command("probe", &["--help".to_owned()], None)
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
    assert!(stdout.starts_with("Usage: probe"), "{stdout:?}");
    assert!(stderr.is_empty(), "{stderr:?}");

    let capability = "cli-probe.upper"
        .parse::<dekopon_core::CapabilityId>()
        .expect("valid capability fixture");
    let outcome = registry
        .run_command(
            "probe",
            &["upper".to_owned(), "-".to_owned()],
            Some("hello"),
        )
        .expect("a piped value proposes");
    assert_eq!(
        outcome,
        CommandRunOutcome::Proposed {
            capability: capability.clone(),
            input: json!({"text": "hello"}),
        }
    );
    // The proposal runs through the same registry: the loop from word to output closes.
    let output = registry
        .invoke(&capability, &json!({"text": "hello"}))
        .expect("the proposed capability runs");
    assert_eq!(output.output, json!({"text": "HELLO"}));

    let outcome = registry
        .run_command("probe", &["upper".to_owned(), "-".to_owned()], None)
        .expect("a usage error is rendered, not a host error");
    let CommandRunOutcome::Rendered {
        stdout,
        stderr,
        status,
    } = outcome
    else {
        panic!("expected a rendered usage error, got {outcome:?}");
    };
    assert_eq!(status, 2);
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(stderr.contains("nothing was piped in"), "{stderr:?}");

    let outcome = registry
        .run_command("probe", &["bogus".to_owned()], None)
        .expect("a decline is an outcome, not a host error");
    assert!(
        matches!(
            outcome,
            CommandRunOutcome::Failed { ref error }
                if error.code == "usage" && error.message.contains("bogus")
        ),
        "{outcome:?}"
    );
}

#[test]
fn command_words_by_provider_lists_each_declaring_component() {
    let registry = ProviderRegistry::load(
        [
            provider_path(),
            imported_provider_path("provider-v0-2-compat-provider.wasm"),
        ],
        HostLimits::default(),
    )
    .expect("a wordless and a command-word provider load together");

    let words = registry
        .command_words_by_provider()
        .into_iter()
        .map(|(provider, words)| (provider.to_string(), words.to_vec()))
        .collect::<Vec<_>>();

    assert_eq!(
        words,
        vec![("provider-v0-2-compat".to_owned(), vec!["compat".to_owned()])],
        "echo declares no words and is not listed"
    );
}

#[test]
fn an_unknown_command_word_is_refused_without_entering_wasm() {
    // An input bound of one byte would refuse this argv before instantiation if the run were ever
    // reached, so an unknown-word refusal proves the lookup answered first, with no store built.
    let registry = ProviderRegistry::load(
        [provider_path()],
        HostLimits {
            max_input_bytes: 1,
            ..HostLimits::default()
        },
    )
    .expect("Rust echo provider loads");

    let error = registry
        .run_command("gh", &["gh".to_owned(), "pr".to_owned()], None)
        .expect_err("no loaded provider declares this word");

    assert!(
        matches!(error, ProviderHostError::UnknownCommandWord { ref word } if word == "gh"),
        "{error:?}"
    );
}

#[test]
fn command_input_beyond_the_bound_is_refused_before_entering_wasm() {
    let registry = ProviderRegistry::load(
        [imported_provider_path("provider-v0-2-compat-provider.wasm")],
        HostLimits {
            max_input_bytes: 8,
            ..HostLimits::default()
        },
    )
    .expect("the input bound does not affect describe");

    let error = registry
        .run_command("compat", &["echo".to_owned()], Some("more than eight"))
        .expect_err("argv plus stdin exceed the bound");

    assert!(
        matches!(
            error,
            ProviderHostError::CommandInputTooLarge {
                length: 19,
                maximum: 8,
                ..
            }
        ),
        "{error:?}"
    );
    // The same argv alone fits, which proves the piped value was counted.
    let outcome = registry
        .run_command("compat", &["echo".to_owned()], None)
        .expect("argv within the bound runs");
    assert!(matches!(outcome, CommandRunOutcome::Proposed { .. }));
}
