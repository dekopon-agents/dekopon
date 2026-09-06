//! The gateway half of `examples/conditional-write/`, held to the daemon's strict decoder.
//!
//! `dekopond.yaml` and `broker.yaml` are two files a reader edits separately, and the one thing
//! that must agree between them is the socket: a gateway pointed at a path no broker binds fails
//! its startup probe with nothing to explain why. Everything else asserted here is a startup
//! failure the daemon would raise anyway — a route naming an absent agent, an agent whose
//! `modelClass` no configured model offers — caught at test time instead of in someone's terminal.

#![cfg(unix)]

use std::path::PathBuf;

use dekopon_config::LocalCatalog;
use dekopond::{
    ActivityMode, DekopondConfig, SlackActivityConfig, SlackActivityFallback, SlackExperience,
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
    let config = serde_yaml::from_str::<DekopondConfig>(&read("dekopond.yaml"))
        .expect("the example gateway configuration decodes under the daemon's strict decoder");

    // Relative to the configuration's own directory, like every other path in the example.
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
        dekopond::TransportConfig::SlackSocketMode {
            experience: SlackExperience::Agent,
            activity: SlackActivityConfig {
                mode: ActivityMode::Native,
                classic_fallback: SlackActivityFallback::Reaction,
            },
            ..
        }
    ));
    // The example configures one image generator and one route that opts into it. A gateway holds
    // at most one, so the route's opt-in is a flag rather than a name to keep in step with a list.
    let generator = config
        .image_generator
        .as_ref()
        .expect("the example configures the gateway's image generator");
    assert_eq!(generator.model, "gpt-image-1");
    assert_eq!(generator.api_key_env, "OPENAI_IMAGE_API_KEY");
    assert_eq!(generator.timeout_ms, 120_000);

    let route = config.routes.first().expect("one route");
    assert_eq!(route.transport, transport.name());
    assert!(
        route.image_generator,
        "the walkthrough's route opts into image generation"
    );
    assert_eq!(route.limits.max_steps, 8);
    assert_eq!(route.limits.max_capability_calls, 16);
    // The walkthrough demonstrates a remembered conversation, which is the mode a reader has to
    // opt into: writing a window bound next to `mode: oneShot` would not decode at all.
    assert_eq!(
        route.conversation,
        dekopond::ConversationConfig::Persistent {
            scope: dekopond::ConversationScope::PrivateConversation,
            idle_timeout_ms: 900_000,
            max_turns: 12,
            max_bytes: 65_536,
        }
    );
    assert_eq!(config.sessions.max_conversations, 1024);

    // A route naming an agent the catalog does not contain, or one it disables, is a startup
    // failure. So is an agent with no resolvable model.
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
fn classic_and_agent_slack_manifests_pin_their_intentional_scope_difference() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let load = |name: &str| {
        let path = root.join("examples/slack").join(name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{} reads: {error}", path.display()));
        serde_yaml::from_slice::<serde_yaml::Value>(&bytes)
            .unwrap_or_else(|error| panic!("{} decodes: {error}", path.display()))
    };
    let classic = load("manifest.yaml");
    let agent = load("manifest-agent.yaml");

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
    assert!(
        !classic_scopes
            .iter()
            .any(|scope| matches!(scope.as_str(), "channels:history" | "groups:history"))
    );
    let agent_events = agent["settings"]["event_subscriptions"]["bot_events"]
        .as_sequence()
        .expect("Agent events are a sequence");
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
    for continuation_event in ["message.channels", "message.groups"] {
        assert!(
            agent_events
                .iter()
                .any(|event| event.as_str() == Some(continuation_event)),
            "Agent manifest must receive {continuation_event} for owned-thread continuation"
        );
    }
    assert_eq!(
        classic["display_information"]["background_color"],
        "#ff6a3d"
    );
    assert_eq!(agent["display_information"]["background_color"], "#ff6a3d");
}
