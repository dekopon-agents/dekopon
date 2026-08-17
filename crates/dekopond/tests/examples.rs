//! The gateway half of `examples/rubber-stamper/`, held to the daemon's own strict decoder.
//!
//! `dekopond.yaml` and `broker.yaml` are two files a reader edits separately, and the one thing
//! that must agree between them is the socket: a gateway pointed at a path no broker binds fails
//! its startup probe with nothing to explain why. Everything else asserted here is a startup
//! failure the daemon would raise anyway — a route naming an absent agent, an agent whose
//! `modelClass` no configured model offers — caught at test time instead of in someone's terminal.

#![cfg(unix)]

use std::path::PathBuf;

use dekopon_config::LocalCatalog;
use dekopond::DekopondConfig;

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/rubber-stamper")
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
    let route = config.routes.first().expect("one route");
    assert_eq!(route.transport, transport.name());
    assert_eq!(route.limits.max_steps, 8);
    assert_eq!(route.limits.max_capability_calls, 16);
    // The walkthrough demonstrates a remembered conversation, which is the mode a reader has to
    // opt into: writing a window bound next to `mode: oneShot` would not decode at all.
    assert_eq!(
        route.conversation,
        dekopond::ConversationConfig::Persistent {
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
