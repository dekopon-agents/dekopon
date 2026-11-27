#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use dekopon_config::LocalCatalog;
use dekopon_gatewayd::{
    GatewaydConfig, LivenessConfig, LivenessMode, SlackExperience, WorkingReaction,
};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/conditional-write")
        .join(name)
}

fn read(name: &str) -> String {
    let path = example(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} reads: {error}", path.display()))
}

#[test]
fn the_example_gateway_configuration_agrees_with_its_broker_and_its_catalog() {
    let config = serde_yaml::from_str::<GatewaydConfig>(&read("gatewayd.yaml"))
        .expect("the example gateway configuration decodes under the daemon's strict decoder");

    assert_eq!(config.catalog_path, PathBuf::from("dekopon.yaml"));

    let broker = serde_yaml::from_str::<dekopon_brokerd::BrokerdConfig>(&read("broker.yaml"))
        .expect("the example broker configuration decodes");
    assert_eq!(
        config.broker.socket_path.as_ref(),
        Some(&broker.socket_path),
        "the gateway must dial the socket the broker binds"
    );
    assert_eq!(
        config.broker.server_uid,
        broker.identities.first().map(|identity| identity.uid),
        "the client verifies the server UID before sending anything"
    );

    let transport = config.transports.first().expect("one transport");
    assert_eq!(transport.kind(), "slackSocketMode");
    assert!(matches!(
        transport,
        dekopon_gatewayd::TransportConfig::SlackSocketMode {
            experience: SlackExperience::Agent,
            liveness: LivenessConfig {
                mode: Some(LivenessMode::Native),
                reaction: Some(dekopon_gatewayd::ReactionConfig {
                    working: Some(WorkingReaction::Auto),
                    ..
                }),
                ..
            },
            ..
        }
    ));
    let route = config.routes.first().expect("one route");
    assert_eq!(route.transport, transport.name());
    assert_eq!(route.limits.max_steps, 100);
    assert_eq!(route.limits.max_capability_calls, 256);
    assert!(matches!(
        &route.memory,
        dekopon_gatewayd::MemoryConfig::Persistent {
            scope: dekopon_gatewayd::MemoryScope::PrivateConversation,
            idle_timeout_ms: 900_000,
            max_turns: 12,
            max_bytes: Some(65_536),
            recall: None,
            forget_after_ms: None,
            hot_turns: None,
            summaries: None,
            capture: None,
            selection: None,
        }
    ));
    assert_eq!(config.sessions.max_conversations, 1024);

    let catalog = LocalCatalog::load(example("dekopon.yaml")).expect("the example catalog loads");
    let agent = catalog
        .agent(&route.agent)
        .expect("the routed agent exists in the catalog");
    assert!(agent.spec.enabled, "a disabled agent routes to nothing");
    let class = agent
        .spec
        .model_class
        .as_deref()
        .expect("the agent declares a model class");
    assert!(
        config
            .models
            .iter()
            .any(|model| model.classes().iter().any(|offered| offered == class)),
        "no configured model offers the {class} class the agent asks for"
    );
}

#[test]
fn both_slack_manifests_request_canvas_read_and_follow_events_while_only_agent_enables_agent_ui() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let load = |name: &str| {
        let path = root.join("examples/slack").join(name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{} reads: {error}", path.display()));
        serde_yaml::from_slice::<serde_yaml::Value>(&bytes)
            .unwrap_or_else(|error| panic!("{} decodes: {error}", path.display()))
    };
    let classic = load("manifest-legacy.yaml");
    let agent = load("manifest.yaml");

    assert!(classic["features"].get("agent_view").is_none());
    assert!(agent["features"].get("agent_view").is_some());
    let scopes = |manifest: &serde_yaml::Value| {
        manifest["oauth_config"]["scopes"]["bot"]
            .as_sequence()
            .expect("bot scopes are a sequence")
            .iter()
            .filter_map(serde_yaml::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let classic_scopes = scopes(&classic);
    let agent_scopes = scopes(&agent);
    assert!(
        classic_scopes
            .iter()
            .any(|scope| scope == "reactions:write")
    );
    assert!(
        !classic_scopes
            .iter()
            .any(|scope| scope == "assistant:write")
    );
    assert!(agent_scopes.iter().any(|scope| scope == "assistant:write"));
    assert!(agent_scopes.iter().any(|scope| scope == "channels:history"));
    assert!(agent_scopes.iter().any(|scope| scope == "groups:history"));
    assert!(agent_scopes.iter().any(|scope| scope == "reactions:write"));
    assert!(agent_scopes.iter().any(|scope| scope == "mpim:history"));
    assert!(
        classic_scopes
            .iter()
            .any(|scope| matches!(scope.as_str(), "channels:history" | "groups:history"))
    );
    for manifest in [&classic, &agent] {
        assert_eq!(manifest["settings"]["interactivity"]["is_enabled"], true);
        let scopes = scopes(manifest);
        assert!(
            scopes.iter().any(|scope| scope == "canvases:read"),
            "provider-slack canvas read requires canvases:read"
        );
        for required in [
            "channels:history",
            "groups:history",
            "reactions:read",
            "users:read",
        ] {
            assert!(
                scopes.iter().any(|scope| scope == required),
                "follow requires {required}"
            );
        }
        let events = manifest["settings"]["event_subscriptions"]["bot_events"]
            .as_sequence()
            .expect("bot events are a sequence");
        for required in ["message.channels", "message.groups", "reaction_added"] {
            assert!(
                events.iter().any(|event| event.as_str() == Some(required)),
                "follow requires {required}"
            );
        }
    }
    let agent_events = agent["settings"]["event_subscriptions"]["bot_events"]
        .as_sequence()
        .expect("Agent events are a sequence");
    assert!(
        agent_events
            .iter()
            .any(|event| event.as_str() == Some("message.mpim"))
    );
    assert!(
        agent_events
            .iter()
            .any(|event| event.as_str() == Some("agent_session_stopped"))
    );
    assert!(
        agent_events
            .iter()
            .any(|event| event.as_str() == Some("app_home_opened"))
    );
    assert_eq!(
        classic["display_information"]["background_color"],
        "#ff6a3d"
    );
    assert_eq!(agent["display_information"]["background_color"], "#ff6a3d");
}

#[test]
fn whatsapp_example_uses_the_typed_default_media_collection_window() {
    let config: GatewaydConfig =
        serde_yaml::from_str(include_str!("../../../../examples/whatsapp/gatewayd.yaml"))
            .expect("WhatsApp example strictly decodes");
    assert!(matches!(
        &config.transports[0],
        dekopon_gatewayd::TransportConfig::WhatsappCloudApi {
            debounce_ms: 5000,
            debounce_max_wait_ms: 15000,
            ..
        }
    ));
}

fn write_gateway_fixture(path: &std::path::Path, contents: &str) {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure fixture");
}

fn gateway_diff(
    proposed: &std::path::Path,
    previous: &std::path::Path,
    options: &[&str],
) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_dekopon-gatewayd"))
        .arg("check")
        .arg(proposed)
        .arg("--diff")
        .arg(previous)
        .args(options)
        .output()
        .expect("run gateway diff")
}

#[test]
fn gateway_diff_reports_model_swaps_and_slack_experience_restarts_in_directories() {
    use serde_json::{Value, json};
    use std::{fs, os::unix::fs::PermissionsExt as _};

    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = temporary.path().join("previous");
    let proposed = temporary.path().join("proposed");
    for root in [&previous, &proposed] {
        let directory = root.join("gateway.d");
        fs::create_dir_all(&directory).expect("create config directory");
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .expect("secure config directory");
        write_gateway_fixture(&root.join("dekopon.yaml"), &read("dekopon.yaml"));
    }
    let agent = read("gatewayd.yaml")
        .replace("catalogPath: dekopon.yaml", "catalogPath: ../dekopon.yaml")
        .replace("mode: native", "mode: off");
    let classic = agent.replace("experience: agent", "experience: classic");
    let previous = previous.join("gateway.d");
    let proposed = proposed.join("gateway.d");
    write_gateway_fixture(&previous.join("gateway.yaml"), &classic);

    for (contents, status, expected) in [
        (
            classic.clone(),
            0,
            json!({"restart_required": [], "swap": []}),
        ),
        (
            classic.replace("model: qwen3", "model: qwen3-updated"),
            0,
            json!({"restart_required": [], "swap": ["models"]}),
        ),
        (
            classic
                .replace("seen: off", "seen: on")
                .replace("sessions:\n", "sessions:\n  indicators: { mode: off }\n"),
            0,
            json!({"restart_required": [], "swap": ["indicators"]}),
        ),
        (
            agent,
            3,
            json!({"restart_required": ["transports"], "swap": []}),
        ),
    ] {
        write_gateway_fixture(&proposed.join("gateway.yaml"), &contents);
        let output = gateway_diff(&proposed, &previous, &["--output", "json"]);
        assert_eq!(output.status.code(), Some(status), "{output:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).expect("diff JSON"),
            expected
        );
    }
    let output = gateway_diff(&proposed, &previous, &[]);
    assert_eq!(output.stdout, b"restart_required\ttransports\n");
}

#[test]
fn gateway_diff_loads_single_files_and_refuses_an_unloadable_input() {
    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = temporary.path().join("previous.yaml");
    let proposed = temporary.path().join("proposed.yaml");
    let invalid = temporary.path().join("invalid.yaml");
    let gateway = read("gatewayd.yaml");
    write_gateway_fixture(
        &temporary.path().join("dekopon.yaml"),
        &read("dekopon.yaml"),
    );
    write_gateway_fixture(&previous, &gateway);
    write_gateway_fixture(
        &proposed,
        &gateway.replace("model: qwen3", "model: qwen3-updated"),
    );
    write_gateway_fixture(&invalid, "apiVersion: deliberately-invalid-version\n");

    let output = gateway_diff(&proposed, &previous, &[]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"swap\tmodels\n");
    for (current, baseline) in [(&proposed, &invalid), (&invalid, &previous)] {
        let output = gateway_diff(current, baseline, &[]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            !output
                .stderr
                .windows(b"deliberately-invalid-version".len())
                .any(|window| window == b"deliberately-invalid-version")
        );
    }
    for options in [&["--probe"][..], &["--against", "baseline.yaml"]] {
        assert_eq!(
            gateway_diff(&proposed, &previous, options).status.code(),
            Some(2),
            "{options:?}"
        );
    }
}

#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test owns and shuts down its current-thread runtime"
)]
async fn gateway_diff_keeps_catalog_path_and_cause_without_document_values() {
    use dekopon_config::{CatalogProblem, ConfigError};
    use dekopon_gatewayd::{GatewaydError, check_diff};
    use std::io::ErrorKind;

    let temporary = tempfile::tempdir().expect("fixture root");
    let previous = temporary.path().join("previous.yaml");
    let proposed = temporary.path().join("proposed.yaml");
    let catalog = temporary.path().join("proposed-catalog.yaml");
    let gateway = read("gatewayd.yaml");
    write_gateway_fixture(
        &temporary.path().join("dekopon.yaml"),
        &read("dekopon.yaml"),
    );
    write_gateway_fixture(&previous, &gateway);
    write_gateway_fixture(
        &proposed,
        &gateway.replace(
            "catalogPath: dekopon.yaml",
            "catalogPath: proposed-catalog.yaml",
        ),
    );

    let errors = check_diff(&proposed, &previous, None)
        .await
        .expect_err("missing catalog");
    assert!(matches!(
        errors.as_slice(),
        [GatewaydError::Catalog(ConfigError::Read { path, source })]
            if path == &catalog.to_string_lossy() && source.kind() == ErrorKind::NotFound
    ));
    let missing = gateway_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert_eq!(
        String::from_utf8(missing.stderr).expect("catalog diagnostic"),
        format!(
            "dekopon-gatewayd: {}: could not read catalog ({})\n",
            catalog.display(),
            ErrorKind::NotFound
        ),
    );

    write_gateway_fixture(
        &catalog,
        &read("dekopon.yaml").replace("enabled: true", "enabled: sensitive-catalog-value"),
    );
    let errors = check_diff(&proposed, &previous, None)
        .await
        .expect_err("invalid catalog");
    assert!(matches!(
        errors.as_slice(),
        [GatewaydError::Catalog(ConfigError::Invalid { path, problems })]
            if path == &catalog.to_string_lossy()
                && matches!(problems.as_slice(), [CatalogProblem::Decode { kind: "Agent", .. }])
    ));
    let invalid = gateway_diff(&proposed, &previous, &["--output", "json"]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert_eq!(
        String::from_utf8(invalid.stderr).expect("catalog diagnostic"),
        format!(
            "dekopon-gatewayd: {}: document 1: invalid Agent resource\n",
            catalog.display()
        ),
    );
}
