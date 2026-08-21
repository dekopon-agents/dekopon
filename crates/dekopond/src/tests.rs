use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{
    AGENT_CONFIG_TOOL_NAME, AssetSource as _, ConversationTurn, HistoryLimits, PromptLimits,
};
use dekopon_broker_protocol::{
    AvailableCapability, BrokerRequest, FrameLimits, RequestEnvelope, ResponseEnvelope, read_frame,
    write_frame,
};
use dekopon_config::LocalCatalog;
use dekopon_core::ExternalSubject;
use dekopon_model::model::{
    AssistantTurn, ChatModel, CompletionOptions, ModelError, ModelFunctionCall, ModelMessage,
    ModelTool, ModelToolCall,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::{net::UnixListener, sync::mpsc};

use crate::{
    agent_inventory,
    asset::{self, AssetSourceRef, AssetStore, PendingAsset, SessionAssets},
    cache_key,
    config::{
        self, ActivityMode, ConversationPolicy, ConversationWindow, ModelConfig,
        NativeActivityConfig, ResolvedBroker, RouteMatch, SlackActivityConfig,
        SlackActivityFallback, SlackExperience, SocketDiscovery,
    },
    conversation::{ConversationKey, ConversationStore, EvictionReason},
    routes::{RouteError, RoutingTable},
    session::{
        BUSY_REPLY, FAILURE_REPLY, ModelFactory, SessionError, SessionGate, SessionRunner,
        UNAUTHORIZED_REPLY, run_session,
    },
    transport::{
        ActivityTarget, ChatActivity, ChatReplier, ChatTransport, ConversationKind, InboundMessage,
        MAX_INBOUND_TEXT_BYTES, MAX_OUTBOUND_TEXT_BYTES, ReplyTarget, TransportError,
        TransportEvent, TransportIdentity, bound_inbound, bound_outbound,
    },
};

const SUBJECT: &str = "tel.16034700182";

fn subject() -> ExternalSubject {
    SUBJECT.parse().expect("canonical subject fixture")
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// A minimal well-formed configuration document every strict-decode case mutates.
fn document(directory: &Path) -> Value {
    json!({
        "apiVersion": config::CONFIG_API_VERSION,
        "catalogPath": directory.join("dekopon.yaml"),
        "broker": { "socketPath": directory.join("broker.sock"), "serverUid": 501 },
        "transports": [
            { "name": "dev", "kind": "local", "socketPath": directory.join("dev.sock") }
        ],
        "models": [
            {
                "name": "local-qwen",
                "kind": "openaiCompatible",
                "endpoint": "http://127.0.0.1:11434/v1",
                "model": "qwen3",
                "timeoutMs": 120_000,
                "classes": ["reasoning"]
            }
        ],
        "routes": [
            {
                "transport": "dev",
                "match": { "kind": "directMessage" },
                "agent": "reviewer"
            }
        ]
    })
}

async fn load(
    directory: &Path,
    document: &Value,
) -> Result<crate::ResolvedConfig, config::ConfigError> {
    let path = directory.join("dekopond.json");
    fs::write(
        &path,
        serde_json::to_vec(document).expect("config serializes"),
    )
    .expect("write config fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure config fixture");
    config::load(&path, crate::current_uid()).await
}

fn temporary() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temporary directory");
    directory
}

#[tokio::test]
async fn a_complete_configuration_resolves_with_documented_defaults() {
    let directory = temporary();
    let resolved = load(directory.path(), &document(directory.path()))
        .await
        .expect("a complete configuration resolves");

    assert_eq!(resolved.transports.len(), 1);
    assert_eq!(resolved.routes.len(), 1);
    assert_eq!(resolved.sessions.max_concurrent, 4);
    assert!(resolved.sessions.reply_on_busy);
    assert_eq!(resolved.routes[0].limits.max_steps, 8);
    assert_eq!(resolved.routes[0].limits.max_capability_calls, 16);
    assert_eq!(resolved.shutdown_grace, Duration::from_secs(120));
    assert_eq!(resolved.broker.server_uid, 501);
    assert!(resolved.telemetry.is_none());
    // A route remembers nothing unless an operator says so, which is exactly the behavior every
    // route had before conversations existed.
    assert_eq!(resolved.sessions.max_conversations, 1024);
    assert_eq!(resolved.routes[0].conversation, ConversationPolicy::OneShot);
}

#[tokio::test]
async fn slack_activity_and_experience_are_explicit_and_strict() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["transports"][0] = json!({
        "name": "workspace-slack",
        "kind": "slackSocketMode",
        "appTokenEnv": "DEKOPOND_SLACK_APP_TOKEN",
        "botTokenEnv": "DEKOPOND_SLACK_BOT_TOKEN",
        "experience": "agent",
        "activity": {"mode": "native", "classicFallback": "reaction"}
    });
    document["routes"][0]["transport"] = json!("workspace-slack");

    let resolved = load(directory.path(), &document)
        .await
        .expect("the Agent profile resolves");
    assert!(matches!(
        resolved.transports.first(),
        Some(config::TransportConfig::SlackSocketMode {
            experience: SlackExperience::Agent,
            activity: SlackActivityConfig {
                mode: ActivityMode::Native,
                classic_fallback: SlackActivityFallback::Reaction,
            },
            ..
        })
    ));

    document["transports"][0]["activity"]["unexpected"] = json!(true);
    assert!(
        load(directory.path(), &document).await.is_err(),
        "unknown cosmetic settings still fail strict decoding"
    );
}

#[tokio::test]
async fn native_activity_is_off_unless_a_transport_opts_in() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["transports"][0] = json!({
        "name": "community-discord",
        "kind": "discordGateway",
        "botTokenEnv": "DEKOPOND_DISCORD_BOT_TOKEN"
    });
    document["routes"][0]["transport"] = json!("community-discord");
    let resolved = load(directory.path(), &document)
        .await
        .expect("the default remains reply-only");
    assert!(matches!(
        resolved.transports.first(),
        Some(config::TransportConfig::DiscordGateway {
            activity: NativeActivityConfig {
                mode: ActivityMode::Off,
            },
            ..
        })
    ));
}

#[tokio::test]
async fn a_persistent_route_resolves_its_documented_window_defaults() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"][0]["conversation"] = json!({"mode": "persistent"});
    let resolved = load(directory.path(), &document)
        .await
        .expect("a persistent route with no bounds resolves");

    assert_eq!(
        resolved.routes[0].conversation,
        ConversationPolicy::Persistent(ConversationWindow {
            idle_timeout: Duration::from_secs(900),
            limits: HistoryLimits {
                max_turns: 12,
                max_bytes: 64 * 1024,
            },
        })
    );
}

/// One table, one property: a configuration that says something the daemon does not understand, or
/// that no deployment could satisfy, fails at startup rather than at the first chat message.
#[tokio::test]
async fn invalid_configurations_fail_closed_at_startup() {
    let directory = temporary();
    let mutate = |mutation: fn(&mut Value)| {
        let mut document = document(directory.path());
        mutation(&mut document);
        document
    };

    let cases: Vec<(&str, Value)> = vec![
        (
            "unknown top-level field",
            mutate(|document| {
                document["unexpected"] = json!(true);
            }),
        ),
        (
            "unknown field inside a transport",
            mutate(|document| {
                document["transports"][0]["socketpath"] = json!("/tmp/typo.sock");
            }),
        ),
        (
            "unknown transport kind",
            mutate(|document| {
                document["transports"][0]["kind"] = json!("carrierPigeon");
            }),
        ),
        (
            "unknown field inside a model",
            mutate(|document| {
                document["models"][0]["temperature"] = json!(0.7);
            }),
        ),
        (
            "unknown route match kind",
            mutate(|document| {
                document["routes"][0]["match"] = json!({"kind": "semaphore"});
            }),
        ),
        (
            // serde's internally tagged *unit* variants accept and discard every key beside the
            // tag, so this once decoded cleanly and threw the channel away — leaving an operator
            // reading their own file convinced the route was scoped to one channel while it in
            // fact claimed every direct message on the transport.
            "a channel on a directMessage route",
            mutate(|document| {
                document["routes"][0]["match"] =
                    json!({"kind": "directMessage", "channel": "c0123abc"});
            }),
        ),
        (
            "duplicate transport name",
            mutate(|document| {
                let duplicate = document["transports"][0].clone();
                document["transports"]
                    .as_array_mut()
                    .expect("transports array")
                    .push(duplicate);
            }),
        ),
        (
            "duplicate model name",
            mutate(|document| {
                let duplicate = document["models"][0].clone();
                document["models"]
                    .as_array_mut()
                    .expect("models array")
                    .push(duplicate);
            }),
        ),
        (
            "route names an unknown transport",
            mutate(|document| {
                document["routes"][0]["transport"] = json!("nowhere");
            }),
        ),
        (
            "route names an unknown model",
            mutate(|document| {
                document["routes"][0]["model"] = json!("gpt-nonexistent");
            }),
        ),
        (
            "zero step budget",
            mutate(|document| {
                document["routes"][0]["limits"] = json!({"maxSteps": 0});
            }),
        ),
        (
            "zero concurrency",
            mutate(|document| {
                document["sessions"] = json!({"maxConcurrent": 0});
            }),
        ),
        (
            "unknown conversation mode",
            mutate(|document| {
                document["routes"][0]["conversation"] = json!({"mode": "amnesiac"});
            }),
        ),
        (
            "zero idle timeout on a persistent route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "persistent", "idleTimeoutMs": 0});
            }),
        ),
        (
            "zero turn window on a persistent route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "persistent", "maxTurns": 0});
            }),
        ),
        (
            "zero byte window on a persistent route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "persistent", "maxBytes": 0});
            }),
        ),
        (
            // A window bound that can never take effect is far more likely a mode typo than an
            // intention, and reading it as one silently would produce a bot that forgets everything
            // while its configuration says otherwise.
            "a window bound on a oneShot route",
            mutate(|document| {
                document["routes"][0]["conversation"] = json!({"mode": "oneShot", "maxTurns": 12});
            }),
        ),
        (
            "an idle timeout on a oneShot route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "oneShot", "idleTimeoutMs": 900_000});
            }),
        ),
        (
            "zero conversation ceiling",
            mutate(|document| {
                document["sessions"] = json!({"maxConversations": 0});
            }),
        ),
        (
            "no transports at all",
            mutate(|document| {
                document["transports"] = json!([]);
            }),
        ),
        (
            // A secret in the field that names a variable is the mistake this rejects loudest: it
            // would otherwise be read as a variable name, come back unset, and look like a
            // deployment problem while sitting in a config file in plain text.
            "credential value where a variable name belongs",
            mutate(|document| {
                document["transports"][0] = json!({
                    "name": "dev",
                    "kind": "telegramLongPoll",
                    "botTokenEnv": "12345:AAH-actual-secret-value"
                });
            }),
        ),
        (
            "model API key variable that is not a variable name",
            mutate(|document| {
                document["models"][0]["apiKeyEnv"] = json!("sk-live-not-a-variable");
            }),
        ),
        (
            "a Slack reaction fallback while activity is off",
            mutate(|document| {
                document["transports"][0] = json!({
                    "name": "dev",
                    "kind": "slackSocketMode",
                    "appTokenEnv": "DEKOPOND_SLACK_APP_TOKEN",
                    "botTokenEnv": "DEKOPOND_SLACK_BOT_TOKEN",
                    "activity": {"mode": "off", "classicFallback": "reaction"}
                });
            }),
        ),
        (
            "classic native Slack activity with no visible fallback",
            mutate(|document| {
                document["transports"][0] = json!({
                    "name": "dev",
                    "kind": "slackSocketMode",
                    "appTokenEnv": "DEKOPOND_SLACK_APP_TOKEN",
                    "botTokenEnv": "DEKOPOND_SLACK_BOT_TOKEN",
                    "experience": "classic",
                    "activity": {"mode": "native", "classicFallback": "none"}
                });
            }),
        ),
        (
            "a Slack endpoint that is neither production nor loopback",
            mutate(|document| {
                document["transports"][0] = json!({
                    "name": "dev",
                    "kind": "slackSocketMode",
                    "appTokenEnv": "DEKOPOND_SLACK_APP_TOKEN",
                    "botTokenEnv": "DEKOPOND_SLACK_BOT_TOKEN",
                    "endpoint": "https://slack.evil.test"
                });
            }),
        ),
        (
            "a Discord endpoint that is neither production nor loopback",
            mutate(|document| {
                document["transports"][0] = json!({
                    "name": "dev",
                    "kind": "discordGateway",
                    "botTokenEnv": "DEKOPOND_DISCORD_BOT_TOKEN",
                    "endpoint": "https://discord.evil.test"
                });
            }),
        ),
        (
            // Userinfo makes the authority read as loopback while the socket connects elsewhere.
            "a loopback-looking endpoint that resolves elsewhere",
            mutate(|document| {
                document["transports"][0] = json!({
                    "name": "dev",
                    "kind": "slackSocketMode",
                    "appTokenEnv": "DEKOPOND_SLACK_APP_TOKEN",
                    "botTokenEnv": "DEKOPOND_SLACK_BOT_TOKEN",
                    "endpoint": "http://127.0.0.1@slack.evil.test"
                });
            }),
        ),
    ];

    for (name, document) in cases {
        assert!(
            load(directory.path(), &document).await.is_err(),
            "{name} must fail closed"
        );
    }
}

#[tokio::test]
async fn a_discord_transport_resolves_its_pinned_rest_endpoint() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["transports"][0] = json!({
        "name": "community-discord",
        "kind": "discordGateway",
        "botTokenEnv": "DEKOPOND_DISCORD_BOT_TOKEN"
    });
    document["routes"][0]["transport"] = json!("community-discord");

    let resolved = load(directory.path(), &document)
        .await
        .expect("a Discord transport resolves");
    assert!(matches!(
        &resolved.transports[0],
        config::TransportConfig::DiscordGateway { endpoint: Some(endpoint), .. }
            if endpoint == config::DISCORD_ENDPOINT
    ));
}

#[tokio::test]
async fn a_loopback_endpoint_override_is_accepted_for_tests() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["transports"][0] = json!({
        "name": "dev",
        "kind": "slackSocketMode",
        "appTokenEnv": "DEKOPOND_SLACK_APP_TOKEN",
        "botTokenEnv": "DEKOPOND_SLACK_BOT_TOKEN",
        "endpoint": "http://127.0.0.1:8080"
    });

    load(directory.path(), &document)
        .await
        .expect("a literal loopback override is what a mock endpoint needs");
}

#[tokio::test]
async fn an_oversized_configuration_is_refused_before_it_is_parsed() {
    let directory = temporary();
    let path = directory.path().join("dekopond.json");
    // Valid JSON, just far past the ceiling: the point is that the byte cap decides, not the parser.
    let mut document = document(directory.path());
    document["routes"][0]["agent"] = json!("reviewer");
    let padding = "p".repeat(crate::HARD_MAX_CONFIG_BYTES + 16);
    document["models"][0]["model"] = json!(padding);
    fs::write(
        &path,
        serde_json::to_vec(&document).expect("config serializes"),
    )
    .expect("write oversized fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure fixture");

    let error = config::load(&path, crate::current_uid())
        .await
        .expect_err("an oversized configuration must be refused");
    assert!(
        matches!(error, config::ConfigError::TooLarge { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn a_group_writable_configuration_is_refused() {
    // This file names the agents chat messages may reach. Another user being able to rewrite it is
    // the same class of problem as another user being able to rewrite broker policy.
    let directory = temporary();
    let path = directory.path().join("dekopond.json");
    fs::write(
        &path,
        serde_json::to_vec(&document(directory.path())).expect("config serializes"),
    )
    .expect("write fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o662)).expect("loosen fixture");

    let error = config::load(&path, crate::current_uid())
        .await
        .expect_err("a group-writable configuration must be refused");
    assert!(
        matches!(error, config::ConfigError::InsecureFile { .. }),
        "{error}"
    );
}

#[test]
fn the_broker_socket_falls_back_to_the_documented_discovery_default() {
    let mut document = serde_json::from_value::<crate::DekopondConfig>(document(Path::new("/tmp")))
        .expect("fixture decodes");
    document.broker.socket_path = None;

    let resolved = config::resolve(
        document,
        PathBuf::from("/tmp/dekopond.json"),
        &SocketDiscovery::new(None, Some(PathBuf::from("/run/user/1000")), None),
        501,
    )
    .expect("discovery resolves");

    assert_eq!(
        resolved.broker.socket_path,
        PathBuf::from("/run/user/1000/dekopon/broker.sock")
    );
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn catalog_text(enabled: bool, model_class: Option<&str>) -> String {
    let class = model_class.map_or(String::new(), |class| format!("  modelClass: {class}\n"));
    format!(
        "apiVersion: dekopon.dev/v1alpha1\n\
         kind: Agent\n\
         metadata:\n  name: reviewer\n\
         spec:\n  description: Reviews things\n  enabled: {enabled}\n  \
         instructions: Answer briefly and never claim authority.\n{class}"
    )
}

fn catalog(enabled: bool, model_class: Option<&str>) -> LocalCatalog {
    LocalCatalog::from_str(
        Path::new("dekopon.yaml"),
        &catalog_text(enabled, model_class),
    )
    .expect("catalog fixture parses")
}

#[test]
fn informational_inventory_omits_agent_instructions() {
    let inventory = agent_inventory(&catalog(true, Some("reasoning")));

    assert!(!inventory.truncated);
    assert_eq!(inventory.agents.len(), 1);
    assert_eq!(inventory.agents[0].id.as_str(), "reviewer");
    assert_eq!(inventory.agents[0].description, "Reviews things");
    assert_eq!(
        inventory.agents[0].model_class.as_deref(),
        Some("reasoning")
    );
    let encoded = serde_json::to_string(&inventory).expect("inventory serializes");
    assert!(!encoded.contains("Answer briefly"), "{encoded}");
    assert!(!encoded.contains("instructions"), "{encoded}");
}

async fn resolved(directory: &Path, document: &Value) -> crate::ResolvedConfig {
    load(directory, document)
        .await
        .expect("configuration resolves")
}

#[tokio::test]
async fn routes_bind_to_a_catalog_agent_and_a_class_matched_model() {
    let directory = temporary();
    let config = resolved(directory.path(), &document(directory.path())).await;
    let table = RoutingTable::bind(&config, &catalog(true, Some("reasoning")))
        .expect("a reachable route binds");

    assert_eq!(table.len(), 1);
    let route = table
        .route("dev", &ConversationKind::DirectMessage)
        .expect("the direct-message route matches");
    assert_eq!(route.agent.as_str(), "reviewer");
    assert_eq!(route.description, "Reviews things");
    assert_eq!(route.model_class.as_deref(), Some("reasoning"));
    assert_eq!(route.model.name(), "local-qwen");
    // Standing orders travel from the catalog into the session as the system prompt.
    assert_eq!(
        route.instructions.as_deref(),
        Some("Answer briefly and never claim authority.")
    );
}

#[tokio::test]
async fn every_bound_route_gets_its_own_prompt_cache_lane() {
    // A route's lane is its instructions and its tools, and two routes are two of those even when
    // they name the same agent — the second route here differs only in what it matches, and the
    // daemon must still not merge their prefixes into one lane.
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"]
        .as_array_mut()
        .expect("routes array")
        .push(json!({
            "transport": "dev",
            "match": {"kind": "channel", "channel": "ops"},
            "agent": "reviewer"
        }));
    let config = resolved(directory.path(), &document).await;
    let catalog = catalog(true, Some("reasoning"));

    let table = RoutingTable::bind(&config, &catalog).expect("both routes bind");
    let direct = table
        .route("dev", &ConversationKind::DirectMessage)
        .expect("the direct-message route matches");
    let channel = table
        .route("dev", &ConversationKind::Channel("ops".to_owned()))
        .expect("the channel route matches");

    assert!(!direct.cache_key.trim().is_empty());
    assert_ne!(direct.cache_key, channel.cache_key);
    // And a restart is a new lane: nothing about the key survives the process that minted it, so it
    // never becomes a durable identifier for the traffic a route carries.
    let rebound = RoutingTable::bind(&config, &catalog).expect("both routes bind again");
    assert_ne!(
        rebound
            .route("dev", &ConversationKind::DirectMessage)
            .expect("the direct-message route matches")
            .cache_key,
        direct.cache_key
    );
}

#[tokio::test]
async fn a_route_no_catalog_can_satisfy_fails_at_startup() {
    let directory = temporary();
    let config = resolved(directory.path(), &document(directory.path())).await;

    // Unknown agent.
    let empty = LocalCatalog::from_str(
        Path::new("dekopon.yaml"),
        "apiVersion: dekopon.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: someone-else\nspec:\n  description: x\n",
    )
    .expect("catalog fixture parses");
    assert!(matches!(
        RoutingTable::bind(&config, &empty).expect_err("an unknown agent is a startup failure"),
        RouteError::UnknownAgent { .. }
    ));

    // Disabled agent: present in the catalog and deliberately not schedulable.
    assert!(matches!(
        RoutingTable::bind(&config, &catalog(false, Some("reasoning")))
            .expect_err("a disabled agent is a startup failure"),
        RouteError::DisabledAgent { .. }
    ));

    // A class no configured model offers.
    assert!(matches!(
        RoutingTable::bind(&config, &catalog(true, Some("vision")))
            .expect_err("an unmatched model class is a startup failure"),
        RouteError::NoModelForClass { .. }
    ));

    // No class and no override: nothing selects a model.
    assert!(matches!(
        RoutingTable::bind(&config, &catalog(true, None))
            .expect_err("an agent with no class and no override is a startup failure"),
        RouteError::NoModelClass { .. }
    ));
}

#[tokio::test]
async fn an_explicit_route_model_outranks_class_matching() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["models"]
        .as_array_mut()
        .expect("models array")
        .push(json!({
            "name": "big-model",
            "kind": "openaiCompatible",
            "endpoint": "http://127.0.0.1:11434/v1",
            "model": "qwen3-max",
            "timeoutMs": 120_000,
            "classes": []
        }));
    document["routes"][0]["model"] = json!("big-model");
    let config = resolved(directory.path(), &document).await;

    let table = RoutingTable::bind(&config, &catalog(true, Some("reasoning")))
        .expect("an explicit model binds");
    assert_eq!(
        table
            .route("dev", &ConversationKind::DirectMessage)
            .expect("route matches")
            .model
            .name(),
        "big-model"
    );
}

#[tokio::test]
async fn channel_routes_match_only_their_own_channel() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"][0]["match"] = json!({"kind": "channel", "channel": "c0123abc"});
    let config = resolved(directory.path(), &document).await;
    let table =
        RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("route binds");

    assert!(
        table
            .route("dev", &ConversationKind::Channel("c0123abc".to_owned()))
            .is_some()
    );
    assert!(
        table
            .route("dev", &ConversationKind::Channel("c9999zzz".to_owned()))
            .is_none()
    );
    assert!(
        table
            .route("dev", &ConversationKind::DirectMessage)
            .is_none()
    );
    assert!(
        table
            .route("other", &ConversationKind::Channel("c0123abc".to_owned()))
            .is_none()
    );
}

#[tokio::test]
async fn a_channel_route_with_no_channel_matches_every_channel() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"][0]["match"] = json!({"kind": "channel"});
    let config = resolved(directory.path(), &document).await;
    let table =
        RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("route binds");

    // Two channels this configuration never names, and a channel created after the daemon started
    // would be a third. Enumerating them is exactly what leaving `channel` out avoids.
    assert!(
        table
            .route("dev", &ConversationKind::Channel("c0123abc".to_owned()))
            .is_some()
    );
    assert!(
        table
            .route("dev", &ConversationKind::Channel("c9999zzz".to_owned()))
            .is_some()
    );
    // Wide, not indiscriminate. A direct message is not a channel, so no catch-all swallows one,
    // and the transport name still bounds the whole thing.
    assert!(
        table
            .route("dev", &ConversationKind::DirectMessage)
            .is_none()
    );
    assert!(
        table
            .route("other", &ConversationKind::Channel("c0123abc".to_owned()))
            .is_none()
    );
}

#[tokio::test]
async fn a_named_channel_route_declared_before_a_catch_all_keeps_its_own_channel() {
    // The configuration an operator writes for "special handling in #incidents, the default
    // everywhere else". Declaration order is the only rule: first match wins, as it always has.
    let directory = temporary();
    let mut document = document(directory.path());
    let routes = document["routes"].as_array_mut().expect("routes array");
    routes[0]["match"] = json!({"kind": "channel", "channel": "c0123abc"});
    routes.push(json!({
        "transport": "dev",
        "match": {"kind": "channel"},
        "agent": "reviewer"
    }));
    routes.push(json!({
        "transport": "dev",
        "match": {"kind": "directMessage"},
        "agent": "reviewer"
    }));
    let config = resolved(directory.path(), &document).await;
    let table =
        RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("every route binds");

    assert_eq!(
        table
            .route("dev", &ConversationKind::Channel("c0123abc".to_owned()))
            .expect("the named channel is routed")
            .r#match,
        RouteMatch::Channel {
            channel: Some("c0123abc".to_owned())
        }
    );
    assert_eq!(
        table
            .route("dev", &ConversationKind::Channel("c9999zzz".to_owned()))
            .expect("every other channel is routed")
            .r#match,
        RouteMatch::Channel { channel: None }
    );
    // And the catch-all sitting above it takes nothing away from the direct-message route.
    assert_eq!(
        table
            .route("dev", &ConversationKind::DirectMessage)
            .expect("direct messages are routed")
            .r#match,
        RouteMatch::DirectMessage {}
    );
}

#[test]
fn a_shared_channel_message_counts_as_addressed_only_when_it_names_the_bot() {
    let slack = TransportIdentity {
        user_id: Some("U0BOTBOT".to_owned()),
        handle: None,
    };
    assert!(slack.is_addressed("hey <@U0BOTBOT> please look at this"));
    assert!(!slack.is_addressed("hey everyone, U0BOTBOT is the bot"));

    let discord = TransportIdentity {
        user_id: Some("123456789012345678".to_owned()),
        handle: None,
    };
    assert!(discord.is_addressed("hey <@123456789012345678>"));
    assert!(discord.is_addressed("hey <@!123456789012345678>"));
    assert!(!discord.is_addressed("123456789012345678 is the bot"));

    let telegram = TransportIdentity {
        user_id: None,
        handle: Some("dekopon_bot".to_owned()),
    };
    assert!(telegram.is_addressed("@dekopon_bot status?"));
    assert!(!telegram.is_addressed("status?"));
}

// ---------------------------------------------------------------------------
// Text bounds
// ---------------------------------------------------------------------------

#[test]
fn untrusted_inbound_text_is_bounded_before_it_reaches_a_model() {
    let short = "hello";
    assert_eq!(bound_inbound(short), short);

    // Multi-byte on purpose: a naive byte slice here panics rather than truncating.
    let long = "é".repeat(MAX_INBOUND_TEXT_BYTES);
    let bounded = bound_inbound(&long);
    assert!(
        bounded.len() <= MAX_INBOUND_TEXT_BYTES + 64,
        "{}",
        bounded.len()
    );
    assert!(bounded.ends_with("[message truncated by the gateway]"));
}

#[test]
fn a_long_answer_keeps_its_beginning_and_its_conclusion() {
    let answer = format!("BEGIN{}END", "x".repeat(MAX_OUTBOUND_TEXT_BYTES * 2));
    let bounded = bound_outbound(&answer);

    assert!(
        bounded.len() <= MAX_OUTBOUND_TEXT_BYTES,
        "{}",
        bounded.len()
    );
    assert!(bounded.starts_with("BEGIN"), "{bounded}");
    assert!(bounded.ends_with("END"), "{bounded}");
    assert!(bounded.contains("truncated by the gateway"), "{bounded}");
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// A model whose turns are fixed in advance, recording every request it received.
struct ModelScript {
    /// Scripted turns, where `None` is a request this model refuses to answer.
    turns: Mutex<VecDeque<Option<AssistantTurn>>>,
    /// Every message list this model was handed, in order.
    ///
    /// Recorded rather than counted because a conversation is an assertion about *what* a later
    /// session replayed and in which order, which a request count cannot express.
    prompts: Mutex<Vec<Vec<ModelMessage>>>,
    /// The model tools offered on each request, in order.
    tools: Mutex<Vec<Vec<ModelTool>>>,
    /// The prompt cache key each request declared, in the same order.
    ///
    /// Recorded from the options the loop actually passed rather than from a serialized body:
    /// `ureq` pretty-prints what it sends, so a captured request is not comparable to a locally
    /// serialized one, and every value compared here is computed in this binary.
    cache_keys: Mutex<Vec<Option<String>>>,
    requests: AtomicUsize,
    forbidden: bool,
}

impl ModelScript {
    fn new(turns: impl IntoIterator<Item = AssistantTurn>) -> Arc<Self> {
        Self::scripted(turns.into_iter().map(Some))
    }

    /// A script in which some requests fail, so one message can break and the next recover.
    fn scripted(turns: impl IntoIterator<Item = Option<AssistantTurn>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into_iter().collect()),
            prompts: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
            cache_keys: Mutex::new(Vec::new()),
            requests: AtomicUsize::new(0),
            forbidden: false,
        })
    }

    /// A model that must never be reached. Calling it fails the test rather than returning an error
    /// a session could recover from and hide.
    fn forbidden() -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(VecDeque::new()),
            prompts: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
            cache_keys: Mutex::new(Vec::new()),
            requests: AtomicUsize::new(0),
            forbidden: true,
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn tool_names(&self, index: usize) -> Vec<String> {
        self.tools
            .lock()
            .expect("recorded tools")
            .get(index)
            .unwrap_or_else(|| panic!("the model received at least {} requests", index + 1))
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }

    /// The cache key one request declared, failing the test when it declared none.
    ///
    /// A missing key is a failure rather than a `None` to compare, because two requests that both
    /// sent nothing would satisfy every "same key" assertion below while carrying no key at all.
    fn cache_key(&self, index: usize) -> String {
        let keys = self.cache_keys.lock().expect("recorded cache keys");
        keys.get(index)
            .cloned()
            .flatten()
            .unwrap_or_else(|| panic!("request {index} declared a prompt cache key"))
    }

    /// One request's messages as `(role, content)` pairs, in the order the model saw them.
    fn prompt(&self, index: usize) -> Vec<(String, String)> {
        let prompts = self.prompts.lock().expect("recorded prompts");
        let messages = prompts
            .get(index)
            .unwrap_or_else(|| panic!("the model received at least {} requests", index + 1));
        messages
            .iter()
            .map(|message| {
                // `ModelMessage`'s fields are private and its serialized form is the contract the
                // backends read, so this asserts on exactly what would go on the wire.
                let value = serde_json::to_value(message).expect("a message serializes");
                (
                    value["role"].as_str().unwrap_or_default().to_owned(),
                    value["content"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }
}

impl ModelFactory for Arc<ModelScript> {
    fn build(&self, _model: &ModelConfig) -> Result<Box<dyn ChatModel + Send>, SessionError> {
        Ok(Box::new(ScriptedModel(Arc::clone(self))))
    }
}

struct ScriptedModel(Arc<ModelScript>);

impl ChatModel for ScriptedModel {
    /// Every request the gateway makes arrives through `complete_with`; this exists because the
    /// trait requires it, and it records a keyless request so a regression that stopped supplying
    /// options would show up as a missing key rather than as a silently different code path.
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
        self.complete_with(messages, tools, &CompletionOptions::default())
    }

    fn complete_with(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
    ) -> Result<AssistantTurn, ModelError> {
        assert!(!self.0.forbidden, "this session must never reach a model");
        self.0
            .prompts
            .lock()
            .expect("recorded prompts")
            .push(messages.to_vec());
        self.0
            .tools
            .lock()
            .expect("recorded tools")
            .push(tools.to_vec());
        self.0
            .cache_keys
            .lock()
            .expect("recorded cache keys")
            .push(options.prompt_cache_key().map(ToOwned::to_owned));
        self.0.requests.fetch_add(1, Ordering::SeqCst);
        self.0
            .turns
            .lock()
            .expect("scripted turn lock")
            .pop_front()
            .flatten()
            .ok_or(ModelError::NoChoices)
    }
}

fn answer(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: Some(text.to_owned()),
        tool_calls: Vec::new(),
        usage: None,
        replay_items: Vec::new(),
    }
}

fn inspect_agent_config() -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "config-call".to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: AGENT_CONFIG_TOOL_NAME.to_owned(),
                arguments: "{}".to_owned(),
            },
        }],
        usage: None,
        replay_items: Vec::new(),
    }
}

/// Records every answer the gateway sent, so a test can assert on what a person would have read.
#[derive(Default)]
struct RecordingReplier {
    replies: Mutex<Vec<String>>,
}

impl RecordingReplier {
    fn replies(&self) -> Vec<String> {
        self.replies.lock().expect("reply lock").clone()
    }
}

impl ChatReplier for RecordingReplier {
    fn reply(
        &self,
        _target: ReplyTarget,
        text: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.replies.lock().expect("reply lock").push(text);
            Ok(())
        })
    }
}

#[derive(Default)]
struct RecordingSurface {
    events: Mutex<Vec<String>>,
    shown: tokio::sync::Notify,
    hidden: tokio::sync::Notify,
}

impl RecordingSurface {
    fn events(&self) -> Vec<String> {
        self.events.lock().expect("surface event lock").clone()
    }

    async fn wait_until_shown(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.shown.notified())
            .await
            .expect("activity becomes visible");
    }

    async fn wait_until_hidden(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.hidden.notified())
            .await
            .expect("activity cleanup completes");
    }
}

impl ChatActivity for RecordingSurface {
    fn show(&self, _target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("surface event lock")
                .push("show".to_owned());
            self.shown.notify_one();
            Ok(())
        })
    }

    fn hide(&self, _target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("surface event lock")
                .push("hide".to_owned());
            self.hidden.notify_one();
            Ok(())
        })
    }

    fn refresh_interval(&self) -> Option<Duration> {
        None
    }
}

impl ChatReplier for RecordingSurface {
    fn reply(
        &self,
        _target: ReplyTarget,
        text: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("surface event lock")
                .push(format!("reply:{text}"));
            Ok(())
        })
    }
}

#[derive(Default)]
struct DelayedSurface {
    events: Mutex<Vec<&'static str>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    hidden: tokio::sync::Notify,
}

impl ChatActivity for DelayedSurface {
    fn show(&self, _target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("delayed surface events")
                .push("show-start");
            self.entered.notify_one();
            self.release.notified().await;
            self.events
                .lock()
                .expect("delayed surface events")
                .push("show-finish");
            Ok(())
        })
    }

    fn hide(&self, _target: ActivityTarget) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("delayed surface events")
                .push("hide");
            self.hidden.notify_one();
            Ok(())
        })
    }

    fn refresh_interval(&self) -> Option<Duration> {
        None
    }
}

impl ChatReplier for DelayedSurface {
    fn reply(
        &self,
        _target: ReplyTarget,
        _text: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("delayed surface events")
                .push("reply");
            Ok(())
        })
    }
}

/// Built through the wire shape rather than the guest type, so this crate keeps its dependency
/// set free of provider-SDK machinery it never links in production.
fn capability(id: &str) -> AvailableCapability {
    serde_json::from_value(json!({
        "provider": "echo",
        "capability": {
            "id": id,
            "description": "Echoes its input",
            "effect": "read-only",
            "risk": "Low",
            "idempotency": "idempotent",
            "inputSchema": {"type": "object"}
        }
    }))
    .expect("capability fixture decodes")
}

/// Serves a fixed script of broker responses over a private Unix socket.
///
/// A real socket rather than an in-memory duplex, because the client authenticates the server by
/// socket ownership and peer UID before it writes a byte.
async fn stub_broker(
    directory: &Path,
    responses: Vec<ResponseEnvelope>,
) -> (ResolvedBroker, mpsc::UnboundedReceiver<RequestEnvelope>) {
    let socket = directory.join("broker.sock");
    let listener = UnixListener::bind(&socket).expect("bind stub broker");
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).expect("secure stub socket");
    let (observed, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(request) =
                read_frame::<_, RequestEnvelope>(&mut stream, FrameLimits::default()).await
            else {
                return;
            };
            let _ = observed.send(request);
            let _ = write_frame(&mut stream, &response, FrameLimits::default()).await;
        }
    });
    (
        ResolvedBroker {
            socket_path: socket,
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        receiver,
    )
}

fn route(model: ModelConfig) -> crate::routes::BoundRoute {
    crate::routes::BoundRoute {
        transport: "dev".to_owned(),
        r#match: RouteMatch::DirectMessage {},
        agent: "reviewer".parse().expect("valid agent fixture"),
        description: "Reviews things".to_owned(),
        model_class: Some("reasoning".to_owned()),
        instructions: Some("Answer briefly.".to_owned()),
        model: Arc::new(model),
        limits: PromptLimits {
            max_steps: 4,
            max_capability_calls: 8,
        },
        conversation: ConversationPolicy::OneShot,
        // Minted the way `RoutingTable::bind` mints it, so a test that reuses one bound route
        // across messages reuses one lane exactly as the daemon does.
        cache_key: cache_key::for_route(),
    }
}

/// The same route, remembering what it was told.
fn persistent_route(model: ModelConfig, window: ConversationWindow) -> crate::routes::BoundRoute {
    crate::routes::BoundRoute {
        conversation: ConversationPolicy::Persistent(window),
        ..route(model)
    }
}

/// Bounds generous enough that only the property under test can drop anything.
fn window() -> ConversationWindow {
    ConversationWindow {
        idle_timeout: Duration::from_secs(900),
        limits: HistoryLimits {
            max_turns: 12,
            max_bytes: 64 * 1024,
        },
    }
}

fn model_config() -> ModelConfig {
    ModelConfig::OpenaiCompatible {
        name: "local-qwen".to_owned(),
        endpoint: "http://127.0.0.1:1/v1".to_owned(),
        model: "qwen3".to_owned(),
        api_key_env: None,
        timeout_ms: 1_000,
        classes: vec!["reasoning".to_owned()],
        // Text only, which is the default and the right one for a local endpoint.
        modalities: Vec::new(),
    }
}

fn message(text: &str) -> InboundMessage {
    InboundMessage {
        transport: "dev".to_owned(),
        subject: subject(),
        channel: "dev".to_owned(),
        thread: None,
        conversation_id: "dev".to_owned(),
        message_id: "1".to_owned(),
        text: text.to_owned(),
        assets: Vec::new(),
        conversation: ConversationKind::DirectMessage,
        // Direct messages ignore addressing. Channel tests opt into structured addressing where
        // that is the behavior under test.
        addressed: None,
        reply: ReplyTarget::Local { connection: 1 },
        activity: None,
    }
}

fn runner(
    broker: ResolvedBroker,
    models: Arc<ModelScript>,
    max_concurrent: usize,
) -> Arc<SessionRunner> {
    runner_with(
        broker,
        Arc::new(models) as Arc<dyn ModelFactory>,
        max_concurrent,
    )
}

fn runner_with(
    broker: ResolvedBroker,
    models: Arc<dyn ModelFactory>,
    max_concurrent: usize,
) -> Arc<SessionRunner> {
    runner_tracking(broker, models, max_concurrent, 1024)
}

fn runner_tracking(
    broker: ResolvedBroker,
    models: Arc<dyn ModelFactory>,
    max_concurrent: usize,
    max_conversations: usize,
) -> Arc<SessionRunner> {
    Arc::new(SessionRunner {
        broker,
        models,
        gate: SessionGate::new(max_concurrent),
        reply_on_busy: true,
        conversations: ConversationStore::new(max_conversations),
        assets: Arc::new(AssetStore::new(
            max_conversations,
            Duration::from_secs(60 * 60),
        )),
        asset_fetchers: HashMap::new(),
        activities: HashMap::new(),
        active_sessions: Default::default(),
        usage_reports: None,
    })
}

/// A model that reports when it was entered and answers only when a test releases it.
///
/// Existing so a test can observe what the gateway had already done *before* the expensive part of
/// a session began — which is the only way to assert an ordering rather than an outcome.
struct BlockedModel {
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    entered_signal: tokio::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    release: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release_signal: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    turn: AssistantTurn,
}

impl BlockedModel {
    fn new(answer_text: &str) -> Arc<Self> {
        Self::with_turn(answer(answer_text))
    }

    fn with_turn(turn: AssistantTurn) -> Arc<Self> {
        let (entered, entered_signal) = std::sync::mpsc::channel();
        let (release, release_signal) = std::sync::mpsc::channel();
        Arc::new(Self {
            entered: Mutex::new(Some(entered)),
            entered_signal: tokio::sync::Mutex::new(entered_signal),
            release: Mutex::new(Some(release)),
            release_signal: Mutex::new(Some(release_signal)),
            turn,
        })
    }

    async fn wait_until_entered(&self) {
        let guard = self.entered_signal.lock().await;
        tokio::task::block_in_place(|| {
            guard
                .recv_timeout(Duration::from_secs(10))
                .expect("the session reached the model");
        });
    }

    fn release(&self) {
        if let Some(sender) = self.release.lock().expect("release lock").take() {
            let _ = sender.send(());
        }
    }
}

impl ModelFactory for Arc<BlockedModel> {
    fn build(&self, _model: &ModelConfig) -> Result<Box<dyn ChatModel + Send>, SessionError> {
        Ok(Box::new(BlockedHandle(Arc::clone(self))))
    }
}

struct BlockedHandle(Arc<BlockedModel>);

impl ChatModel for BlockedHandle {
    fn complete(
        &self,
        _messages: &[ModelMessage],
        _tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
        if let Some(sender) = self.0.entered.lock().expect("entered lock").take() {
            let _ = sender.send(());
        }
        if let Some(receiver) = self.0.release_signal.lock().expect("release lock").take() {
            let _ = receiver.recv_timeout(Duration::from_secs(30));
        }
        Ok(self.0.turn.clone())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorized_message_reaches_its_agent_and_answers_in_chat() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let models = ModelScript::new([answer("Everything looks fine.")]);
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("how are things?"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec!["Everything looks fine.".to_owned()]);
    assert_eq!(models.requests(), 1);

    // The gateway asked on the sender's behalf, not its own: the broker sees a subject and an
    // agent, and maps the subject to a principal itself.
    let request = observed.recv().await.expect("stub broker saw one request");
    let BrokerRequest::CapabilitiesFor { subject, agent } = request.request else {
        panic!("a session must open an attested leg: {request:?}");
    };
    assert_eq!(subject.canonical(), SUBJECT);
    assert_eq!(agent.as_str(), "reviewer");
}

#[tokio::test(flavor = "multi_thread")]
async fn authorized_work_shows_activity_until_after_the_durable_reply() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let model = BlockedModel::new("All good.");
    let surface = Arc::new(RecordingSurface::default());
    let mut runner = runner_with(
        broker,
        Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
        4,
    );
    Arc::get_mut(&mut runner)
        .expect("fixture has one runner owner")
        .activities
        .insert(
            "dev".to_owned(),
            Arc::clone(&surface) as Arc<dyn ChatActivity>,
        );
    let mut inbound = message("how are things?");
    inbound.activity = Some(ActivityTarget::Discord {
        channel_id: "200000000000000001".to_owned(),
    });

    let session = tokio::spawn(run_session(
        runner,
        route(model_config()),
        inbound,
        Arc::clone(&surface) as Arc<dyn ChatReplier>,
    ));
    surface.wait_until_shown().await;
    model.wait_until_entered().await;
    model.release();
    session.await.expect("the session completes");
    surface.wait_until_hidden().await;

    assert_eq!(surface.events(), ["show", "reply:All good.", "hide"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sealing_does_not_delay_reply_and_cleanup_follows_an_issued_show() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let model = BlockedModel::new("not delayed");
    let surface = Arc::new(DelayedSurface::default());
    let mut runner = runner_with(
        broker,
        Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
        4,
    );
    Arc::get_mut(&mut runner)
        .expect("fixture has one runner owner")
        .activities
        .insert(
            "dev".to_owned(),
            Arc::clone(&surface) as Arc<dyn ChatActivity>,
        );
    let mut inbound = message("do it");
    inbound.activity = Some(ActivityTarget::Discord {
        channel_id: "200000000000000001".to_owned(),
    });
    let session = tokio::spawn(run_session(
        runner,
        route(model_config()),
        inbound,
        Arc::clone(&surface) as Arc<dyn ChatReplier>,
    ));
    tokio::time::timeout(Duration::from_secs(5), surface.entered.notified())
        .await
        .expect("activity call starts");
    model.wait_until_entered().await;
    model.release();

    tokio::time::timeout(Duration::from_secs(1), session)
        .await
        .expect("cosmetic I/O cannot delay the answer")
        .expect("session task completes");
    assert_eq!(
        surface
            .events
            .lock()
            .expect("delayed surface events")
            .as_slice(),
        ["show-start", "reply"]
    );

    surface.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), surface.hidden.notified())
        .await
        .expect("cleanup follows the issued show");
    assert_eq!(
        surface
            .events
            .lock()
            .expect("delayed surface events")
            .as_slice(),
        ["show-start", "reply", "show-finish", "hide"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_work_never_publishes_activity() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(Vec::new(), Vec::new())],
    )
    .await;
    let surface = Arc::new(RecordingSurface::default());
    let mut runner = runner(broker, ModelScript::forbidden(), 4);
    Arc::get_mut(&mut runner)
        .expect("fixture has one runner owner")
        .activities
        .insert(
            "dev".to_owned(),
            Arc::clone(&surface) as Arc<dyn ChatActivity>,
        );
    let mut inbound = message("not authorized");
    inbound.activity = Some(ActivityTarget::Discord {
        channel_id: "200000000000000001".to_owned(),
    });

    run_session(
        runner,
        route(model_config()),
        inbound,
        Arc::clone(&surface) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(
        surface.events(),
        [format!("reply:{UNAUTHORIZED_REPLY}")],
        "activity begins only after the broker's fresh grant"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_stop_wins_the_race_and_suppresses_answer_and_history() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let model = BlockedModel::new("stale answer");
    let surface = Arc::new(RecordingSurface::default());
    let mut runner = runner_with(
        broker,
        Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
        4,
    );
    Arc::get_mut(&mut runner)
        .expect("fixture has one runner owner")
        .activities
        .insert(
            "dev".to_owned(),
            Arc::clone(&surface) as Arc<dyn ChatActivity>,
        );
    let mut inbound = message("stop this");
    inbound.activity = Some(ActivityTarget::Slack {
        channel_id: "d0123abc".to_owned(),
        thread_ts: "1700000000.000001".to_owned(),
        message_ts: "1700000000.000001".to_owned(),
        initiator_user_id: "u9xyz".to_owned(),
    });
    let route = persistent_route(model_config(), window());
    let session_runner = Arc::clone(&runner);
    let session = tokio::spawn(run_session(
        session_runner,
        route,
        inbound,
        Arc::clone(&surface) as Arc<dyn ChatReplier>,
    ));
    surface.wait_until_shown().await;
    model.wait_until_entered().await;

    let mut controls = tokio::task::JoinSet::new();
    crate::stop_session(
        &runner,
        &mut controls,
        crate::transport::SessionStop {
            transport: "dev".to_owned(),
            conversation_id: "dev".to_owned(),
            subject: "tel.999".parse().expect("other canonical subject"),
        },
    );
    assert_eq!(
        controls.len(),
        0,
        "another chat user cannot stop the initiator's work"
    );
    crate::stop_session(
        &runner,
        &mut controls,
        crate::transport::SessionStop {
            transport: "dev".to_owned(),
            conversation_id: "dev".to_owned(),
            subject: subject(),
        },
    );
    while controls.join_next().await.is_some() {}
    model.release();
    session.await.expect("the cancelled session exits");
    surface.wait_until_hidden().await;

    let events = surface.events();
    assert!(events.contains(&"show".to_owned()), "{events:?}");
    assert!(events.contains(&"hide".to_owned()), "{events:?}");
    assert!(events.contains(&format!("reply:{}", crate::session::STOPPED_REPLY)));
    assert!(!events.iter().any(|event| event.contains("stale answer")));
    assert_eq!(
        runner.conversations.tracked(),
        0,
        "a cancelled turn is never committed to persistent history"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aborting_the_async_session_cancels_later_blocking_tool_work() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(vec![capability("echo.echo")], Vec::new()),
            ResponseEnvelope::error(
                "unexpected-invocation",
                "tool work should have been cancelled",
            ),
        ],
    )
    .await;
    let model = BlockedModel::with_turn(AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "late-tool".to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: "bash".to_owned(),
                arguments: json!({"script": "echo.echo '{}'"}).to_string(),
            },
        }],
        usage: None,
        replay_items: Vec::new(),
    });
    let replier = Arc::new(RecordingReplier::default());
    let session = tokio::spawn(run_session(
        runner_with(
            broker,
            Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
            4,
        ),
        route(model_config()),
        message("cancel during shutdown"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    ));
    model.wait_until_entered().await;
    let first = observed
        .recv()
        .await
        .expect("authorization request was sent");
    assert!(matches!(
        first.request,
        BrokerRequest::CapabilitiesFor { .. }
    ));

    session.abort();
    assert!(
        session
            .await
            .expect_err("session task is aborted")
            .is_cancelled(),
        "the async owner is gone"
    );
    model.release();

    assert!(
        tokio::time::timeout(Duration::from_millis(300), observed.recv())
            .await
            .is_err(),
        "the cancellation guard prevents the model's late tool call reaching the broker"
    );
    assert!(replier.replies().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorized_agent_can_inspect_its_credential_free_effective_configuration() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let models = ModelScript::new([
        inspect_agent_config(),
        answer("I have prepared the configuration table."),
    ]);
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("what is this agent's configuration?"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(
        replier.replies(),
        vec!["I have prepared the configuration table.".to_owned()]
    );
    assert_eq!(models.requests(), 2);
    assert_eq!(
        models.tool_names(0),
        vec!["bash".to_owned(), AGENT_CONFIG_TOOL_NAME.to_owned()]
    );

    let result = models
        .prompt(1)
        .into_iter()
        .find_map(|(role, content)| (role == "tool").then_some(content))
        .expect("second request carries the meta result");
    let encoded = result;
    let result: Value = serde_json::from_str(&encoded).expect("meta result is JSON");
    assert_eq!(result["agent"]["id"], "reviewer");
    assert_eq!(result["agent"]["description"], "Reviews things");
    assert_eq!(result["agent"]["modelClass"], "reasoning");
    assert_eq!(result["prompt"]["instructions"], "Answer briefly.");
    assert_eq!(result["session"]["maxSteps"], 4);
    assert_eq!(result["session"]["maxCapabilityCalls"], 8);
    assert_eq!(result["session"]["conversation"]["mode"], "oneShot");
    assert_eq!(result["effectiveAuthorization"]["engine"], "Cedar");
    assert_eq!(
        result["effectiveAuthorization"]["capabilities"][0]["id"],
        "echo.echo"
    );
    assert_eq!(
        result["effectiveAuthorization"]["capabilities"][0]["effect"],
        "read-only"
    );
    assert_eq!(result["security"]["credentialsIncluded"], false);
    assert_eq!(result["security"]["rawCedarIncluded"], false);
    assert_eq!(result["security"]["identityIncluded"], false);
    assert!(result.get("principal").is_none());
    assert!(result.get("subject").is_none());
    // These values exist on the live route/session objects handed to the constructor's caller.
    // None is an allowed input to the credential-free view itself.
    assert!(!encoded.contains("http://127.0.0.1:1/v1"));
    assert!(!encoded.contains("qwen3"));
    assert!(!encoded.contains(SUBJECT));
    assert!(!encoded.contains(&directory.path().display().to_string()));

    let request = observed.recv().await.expect("one capability listing");
    assert!(matches!(
        request.request,
        BrokerRequest::CapabilitiesFor { .. }
    ));
    assert!(
        observed.try_recv().is_err(),
        "meta inspection makes no broker call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reports_unreported_model_usage_without_delaying_the_answer() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let models = ModelScript::new([answer("Done.")]);
    let replier = Arc::new(RecordingReplier::default());
    let (usage, mut reports) = mpsc::channel(1);
    let mut session_runner = runner(broker, models, 1);
    Arc::get_mut(&mut session_runner)
        .expect("fixture has one runner owner")
        .usage_reports = Some(usage);

    run_session(
        session_runner,
        route(model_config()),
        message("do it"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), ["Done."]);
    let report = reports.recv().await.expect("session emits usage");
    assert_eq!(report.model_calls, 1);
    assert_eq!(report.input_tokens, 0);
    assert_eq!(report.input_unreported_calls, 1);
    assert_eq!(report.output_unreported_calls, 1);
    assert_eq!(report.total_unreported_calls, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unauthorized_subject_is_refused_before_any_model_call() {
    // The cheapest possible refusal, and the one that cannot be argued with: the broker already
    // said this subject reaches nothing through this agent, so there is no question to ask a model.
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(Vec::new(), Vec::new())],
    )
    .await;
    let models = ModelScript::forbidden();
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("do something privileged"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![UNAUTHORIZED_REPLY.to_owned()]);
    assert_eq!(models.requests(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_attestation_reads_as_a_refusal_rather_than_a_breakage() {
    // A broker whose attestor grant does not cover this subject's namespace answers with a
    // transport-level code instead of an empty capability set. Reporting that as "something broke"
    // would send someone to an operator over a working refusal.
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::error(
            dekopon_broker_protocol::ERROR_UNAUTHENTICATED,
            "attestation refused: no attestor authority for this subject",
        )],
    )
    .await;
    let models = ModelScript::forbidden();
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("hello"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![UNAUTHORIZED_REPLY.to_owned()]);
    assert_eq!(models.requests(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_saturated_gateway_says_so_rather_than_queueing_work() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), Vec::new()).await;
    let models = ModelScript::forbidden();
    let runner = runner(broker, Arc::clone(&models), 1);
    // Hold the only permit, exactly as an in-flight session would.
    let _held = runner
        .gate
        .admit(("other".to_owned(), "other".to_owned(), None))
        .expect("the first session is admitted");
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        Arc::clone(&runner),
        route(model_config()),
        message("hello"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![BUSY_REPLY.to_owned()]);
    assert_eq!(models.requests(), 0);
}

#[tokio::test]
async fn one_conversation_runs_one_session_at_a_time() {
    // A person who thinks a bot is stuck sends the same thing again. Without this, the second copy
    // becomes a second billed session racing the first in the same thread.
    let gate = SessionGate::new(8);
    let key = (
        "slack".to_owned(),
        "c0123abc".to_owned(),
        Some("1.0".to_owned()),
    );

    let first = gate
        .admit(key.clone())
        .expect("the first message is admitted");
    assert!(gate.admit(key.clone()).is_none());
    // A different thread in the same channel is a different conversation.
    assert!(
        gate.admit((
            "slack".to_owned(),
            "c0123abc".to_owned(),
            Some("2.0".to_owned())
        ))
        .is_some()
    );

    drop(first);
    assert!(
        gate.admit(key).is_some(),
        "a finished session releases its conversation"
    );
}

#[tokio::test]
async fn concurrency_is_bounded_across_every_conversation() {
    let gate = SessionGate::new(2);
    let first = gate
        .admit(("a".to_owned(), "a".to_owned(), None))
        .expect("first");
    let second = gate
        .admit(("b".to_owned(), "b".to_owned(), None))
        .expect("second");
    assert!(gate.admit(("c".to_owned(), "c".to_owned(), None)).is_none());

    drop(first);
    assert!(gate.admit(("c".to_owned(), "c".to_owned(), None)).is_some());
    drop(second);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_session_answers_one_fixed_line_and_never_raw_error_text() {
    // A `PromptError` can carry model-chosen text, a provider message, or a transport diagnostic.
    // Chat is the last place any of those belong.
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    // An empty script: the first turn fails, which is a broken session rather than a failed script.
    let models = ModelScript::new([]);
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("break something"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![FAILURE_REPLY.to_owned()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_broker_fails_the_session_without_reaching_a_model() {
    let directory = temporary();
    let broker = ResolvedBroker {
        socket_path: directory.path().join("absent.sock"),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let models = ModelScript::forbidden();
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("hello"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![FAILURE_REPLY.to_owned()]);
    assert_eq!(models.requests(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_answer_longer_than_chat_accepts_is_bounded_on_the_way_out() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let long = format!("BEGIN{}END", "y".repeat(MAX_OUTBOUND_TEXT_BYTES * 2));
    let models = ModelScript::new([answer(&long)]);
    let replier = Arc::new(RecordingReplier::default());

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("write a lot"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    let replies = replier.replies();
    assert_eq!(replies.len(), 1);
    assert!(
        replies[0].len() <= MAX_OUTBOUND_TEXT_BYTES,
        "{}",
        replies[0].len()
    );
    assert!(replies[0].starts_with("BEGIN"));
    assert!(replies[0].ends_with("END"));
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------

/// The same message from somebody else in the same conversation.
fn message_from(subject: &str, text: &str) -> InboundMessage {
    InboundMessage {
        subject: subject.parse().expect("canonical subject fixture"),
        ..message(text)
    }
}

/// A prompt written the way a test reads it.
fn transcript(messages: &[(&str, &str)]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|(role, content)| ((*role).to_owned(), (*content).to_owned()))
        .collect()
}

/// Every broker request the stub saw, asserting each one was a capability listing.
///
/// The count is the assertion that matters: `stub_broker` serves one connection per response, so
/// "N messages produced N `capabilitiesFor` envelopes" is what proves authorization is asked again
/// per message rather than remembered with the conversation.
fn capability_listings(observed: &mut mpsc::UnboundedReceiver<RequestEnvelope>) -> usize {
    let mut count = 0;
    while let Ok(request) = observed.try_recv() {
        assert!(
            matches!(request.request, BrokerRequest::CapabilitiesFor { .. }),
            "every session opens an attested leg: {request:?}"
        );
        count += 1;
    }
    count
}

/// One capability listing per message, so a two-message test needs two of them.
fn listings(count: usize, capabilities: &[&str]) -> Vec<ResponseEnvelope> {
    (0..count)
        .map(|_| {
            ResponseEnvelope::capabilities(
                capabilities
                    .iter()
                    .map(|identifier| capability(identifier))
                    .collect(),
                Vec::new(),
            )
        })
        .collect()
}

fn granted(capabilities: &[&str]) -> Vec<String> {
    capabilities
        .iter()
        .map(|capability| (*capability).to_owned())
        .collect()
}

/// Records one exchange for the store tests whose subject is the history rather than the cache
/// lane, minting the key the way the first session of a conversation supplies it.
fn commit(
    store: &ConversationStore,
    key: &ConversationKey,
    granted: &[String],
    window: ConversationWindow,
    turn: ConversationTurn,
    now: Instant,
) {
    store.commit(
        key,
        granted,
        window,
        turn,
        &cache_key::for_conversation(),
        now,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_route_replays_the_previous_exchange_into_the_next_prompt() {
    // The whole feature in one assertion: a follow-up that says "and the second one?" is answerable
    // because the exchange before it is in the prompt, in order, ahead of the new message.
    let directory = temporary();
    let (broker, mut observed) = stub_broker(directory.path(), listings(2, &["echo.echo"])).await;
    let models = ModelScript::new([
        answer("Two things broke."),
        answer("The second one was the database."),
    ]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let route = persistent_route(model_config(), window());

    for text in ["what broke?", "and the second one?"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(text),
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }

    assert_eq!(
        replier.replies(),
        vec![
            "Two things broke.".to_owned(),
            "The second one was the database.".to_owned()
        ]
    );
    assert_eq!(
        models.prompt(0),
        transcript(&[("system", "Answer briefly."), ("user", "what broke?")]),
        "the first message of a conversation starts clean"
    );
    assert_eq!(
        models.prompt(1),
        transcript(&[
            ("system", "Answer briefly."),
            ("user", "what broke?"),
            ("assistant", "Two things broke."),
            ("user", "and the second one?"),
        ]),
        "instructions first, then what the conversation remembers, then the new message"
    );
    // Persistence remembers text and never a decision: both messages asked the broker for
    // themselves.
    assert_eq!(capability_listings(&mut observed), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_route_starts_from_an_empty_prompt_every_message() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(2, &["echo.echo"])).await;
    let models = ModelScript::new([answer("Two things broke."), answer("Which one?")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);

    for text in ["what broke?", "and the second one?"] {
        run_session(
            Arc::clone(&runner),
            route(model_config()),
            message(text),
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }

    assert_eq!(
        models.prompt(1),
        transcript(&[
            ("system", "Answer briefly."),
            ("user", "and the second one?")
        ]),
        "a oneShot route is exactly the behavior every route had before conversations existed"
    );
    assert_eq!(
        runner.conversations.tracked(),
        0,
        "a oneShot route stores nothing at all"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_senders_in_one_conversation_never_see_each_others_history() {
    // In a shared channel this is not a hypothetical. The admission key deliberately has no subject
    // in it; the history key deliberately does, and this is the difference that makes.
    const OTHER_SUBJECT: &str = "tel.16035550100";
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(3, &["echo.echo"])).await;
    let models = ModelScript::new([
        answer("Your deploy failed."),
        answer("Yours is still running."),
        answer("Still the deploy."),
    ]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let route = persistent_route(model_config(), window());

    for message in [
        message_from(SUBJECT, "what happened to mine?"),
        message_from(OTHER_SUBJECT, "and mine?"),
        message_from(SUBJECT, "and now?"),
    ] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message,
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }

    assert_eq!(
        models.prompt(1),
        transcript(&[("system", "Answer briefly."), ("user", "and mine?")]),
        "the second sender's first message must not carry the first sender's exchange"
    );
    assert_eq!(
        models.prompt(2),
        transcript(&[
            ("system", "Answer briefly."),
            ("user", "what happened to mine?"),
            ("assistant", "Your deploy failed."),
            ("user", "and now?"),
        ]),
        "each sender continues their own conversation and nobody else's"
    );
    assert_eq!(runner.conversations.tracked(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_grant_drops_the_history_it_was_built_under() {
    // Output fetched under a wider grant is sitting in the window. Narrowing what the subject may
    // reach without dropping it would keep replaying that output after the capability that produced
    // it was taken away.
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo"), capability("gh.pr_view")],
                Vec::new(),
            ),
            ResponseEnvelope::capabilities(vec![capability("echo.echo")], Vec::new()),
        ],
    )
    .await;
    let models = ModelScript::new([
        answer("Pull request 12 is open."),
        answer("I can't see it."),
    ]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let route = persistent_route(model_config(), window());

    for text in ["what is in pr 12?", "and now?"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(text),
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }

    assert_eq!(
        models.prompt(1),
        transcript(&[("system", "Answer briefly."), ("user", "and now?")]),
        "a changed grant set starts a fresh conversation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_grant_removes_the_conversation_rather_than_only_refusing_the_message() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(vec![capability("echo.echo")], Vec::new()),
            ResponseEnvelope::capabilities(Vec::new(), Vec::new()),
        ],
    )
    .await;
    let models = ModelScript::new([answer("Here is the secret plan.")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let route = persistent_route(model_config(), window());

    run_session(
        Arc::clone(&runner),
        route.clone(),
        message("what is the plan?"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;
    assert_eq!(runner.conversations.tracked(), 1);

    run_session(
        Arc::clone(&runner),
        route,
        message("remind me"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(
        replier.replies().last().map(String::as_str),
        Some(UNAUTHORIZED_REPLY)
    );
    assert_eq!(
        models.requests(),
        1,
        "a revoked subject costs no model call"
    );
    assert_eq!(
        runner.conversations.tracked(),
        0,
        "a revoked subject must not leave their exchange resident"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_session_records_the_question_it_could_not_answer() {
    // The fixed failure line is this daemon's sentence rather than the agent's, and storing it
    // would teach the model to keep producing it. The question still happened, though: dropping it
    // would leave the retry with nothing to refer back to.
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(2, &["echo.echo"])).await;
    let models = ModelScript::scripted([None, Some(answer("It was the database."))]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let route = persistent_route(model_config(), window());

    run_session(
        Arc::clone(&runner),
        route.clone(),
        message("what broke?"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;
    assert_eq!(replier.replies(), vec![FAILURE_REPLY.to_owned()]);

    run_session(
        Arc::clone(&runner),
        route,
        message("try again"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(
        models.prompt(1),
        transcript(&[
            ("system", "Answer briefly."),
            ("user", "what broke?"),
            ("user", "try again"),
        ]),
        "an unanswered turn replays the question and nothing in the answer's place"
    );
    let replies = replier.replies();
    assert!(
        !replies
            .iter()
            .any(|reply| reply.contains(FAILURE_REPLY) && reply != FAILURE_REPLY),
        "{replies:?}"
    );
}

/// A factory whose model cannot be constructed, which is a session that asks nothing.
struct UnbuildableModel;

impl ModelFactory for UnbuildableModel {
    fn build(&self, _model: &ModelConfig) -> Result<Box<dyn ChatModel + Send>, SessionError> {
        Err(SessionError::Model(ModelError::NoChoices))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_never_reached_a_model_remembers_nothing() {
    // The turn a session commits is the one the prompt loop recorded. A session that failed before
    // the loop recorded nothing, and must not commit the newest *seeded* turn in its place.
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(1, &["echo.echo"])).await;
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner_with(
        broker,
        Arc::new(UnbuildableModel) as Arc<dyn ModelFactory>,
        4,
    );

    run_session(
        Arc::clone(&runner),
        persistent_route(model_config(), window()),
        message("what broke?"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![FAILURE_REPLY.to_owned()]);
    assert_eq!(
        runner.conversations.tracked(),
        0,
        "a message nothing was ever asked about leaves no exchange behind"
    );
}

#[test]
fn an_idle_conversation_is_dropped_and_the_next_message_starts_fresh() {
    // The clock is a parameter because `tokio::time::pause` does not reach
    // `std::time::Instant::now()` inside a blocking task, so injecting it is the only way this is
    // deterministic rather than a sleep.
    let store = ConversationStore::new(8);
    let key = ConversationKey::new("dev", "dev", SUBJECT);
    let allowed = granted(&["echo.echo"]);
    let start = Instant::now();
    commit(
        &store,
        &key,
        &allowed,
        window(),
        ConversationTurn::completed("what broke?", "two things"),
        start,
    );

    let warm = store.begin(&key, &allowed, window(), start + Duration::from_secs(899));
    assert_eq!(
        warm.history.len(),
        1,
        "inside the timeout the exchange is replayed"
    );

    let cold = store.begin(&key, &allowed, window(), start + Duration::from_secs(900));
    assert!(
        cold.history.is_empty(),
        "past the timeout the next message starts fresh"
    );
    assert_eq!(
        store.tracked(),
        0,
        "an idle conversation is dropped rather than merely skipped"
    );
}

#[test]
fn the_conversation_ceiling_evicts_the_least_recently_used_rather_than_refusing() {
    // A person talking now matters more than one who stopped an hour ago, so a memory bound must
    // not become an admission bound.
    let store = ConversationStore::new(2);
    let allowed = granted(&["echo.echo"]);
    let start = Instant::now();
    let keys = ["first", "second", "third"]
        .map(|conversation| ConversationKey::new("dev", conversation, SUBJECT));
    let turn = |text: &str| ConversationTurn::completed(text, "noted");

    commit(&store, &keys[0], &allowed, window(), turn("one"), start);
    commit(
        &store,
        &keys[1],
        &allowed,
        window(),
        turn("two"),
        start + Duration::from_secs(1),
    );
    // Touching the oldest conversation makes the middle one the least recently used.
    commit(
        &store,
        &keys[0],
        &allowed,
        window(),
        turn("one again"),
        start + Duration::from_secs(2),
    );
    commit(
        &store,
        &keys[2],
        &allowed,
        window(),
        turn("three"),
        start + Duration::from_secs(3),
    );

    let now = start + Duration::from_secs(4);
    assert_eq!(store.tracked(), 2, "the ceiling holds");
    assert!(
        store
            .begin(&keys[1], &allowed, window(), now)
            .history
            .is_empty(),
        "the least recently used conversation is the one that goes"
    );
    assert_eq!(
        store.begin(&keys[0], &allowed, window(), now).history.len(),
        2,
        "the conversation somebody is still having survives"
    );
    assert_eq!(
        store.begin(&keys[2], &allowed, window(), now).history.len(),
        1
    );
}

#[test]
fn each_window_bound_drops_the_oldest_exchange_on_its_own() {
    // Two bounds because they fail differently: twelve one-line exchanges and twelve
    // paragraph-length ones are the same number of turns and very different prompts.
    let allowed = granted(&["echo.echo"]);
    let now = Instant::now();
    let by_turns = ConversationWindow {
        idle_timeout: Duration::from_secs(900),
        limits: HistoryLimits {
            max_turns: 2,
            max_bytes: 64 * 1024,
        },
    };
    // Each exchange below is a ten-byte question and a nine-byte answer, so two fit under this
    // ceiling and three do not, while the turn count stays well inside `max_turns`.
    let by_bytes = ConversationWindow {
        idle_timeout: Duration::from_secs(900),
        limits: HistoryLimits {
            max_turns: 12,
            max_bytes: 40,
        },
    };

    for (window, name) in [(by_turns, "turn bound"), (by_bytes, "byte bound")] {
        let store = ConversationStore::new(8);
        let key = ConversationKey::new("dev", "dev", SUBJECT);
        for text in ["question a", "question b", "question c"] {
            commit(
                &store,
                &key,
                &allowed,
                window,
                ConversationTurn::completed(text, "an answer"),
                now,
            );
        }
        let history = store.begin(&key, &allowed, window, now).history;
        assert_eq!(history.len(), 2, "{name} keeps two exchanges");
        assert_eq!(
            history.turns()[0].user(),
            "question b",
            "{name} drops the oldest exchange first"
        );
    }
}

#[test]
fn a_history_and_a_revoked_entry_are_two_different_removals() {
    let store = ConversationStore::new(8);
    let key = ConversationKey::new("dev", "dev", SUBJECT);
    let allowed = granted(&["echo.echo"]);
    let now = Instant::now();

    assert!(
        !store.remove(&key, EvictionReason::GrantChanged),
        "removing a conversation nobody started is not an eviction"
    );
    commit(
        &store,
        &key,
        &allowed,
        window(),
        ConversationTurn::completed("what broke?", "two things"),
        now,
    );
    assert!(store.remove(&key, EvictionReason::GrantChanged));
    assert_eq!(store.tracked(), 0);
}

#[test]
fn two_sessions_sharing_one_conversation_both_land_their_exchange() {
    // Admission control does not serialize this: on Slack a message opening a thread and a reply
    // inside it admit under different keys and share one conversation identity, so a sender
    // replying to themselves before the bot answers runs two sessions against one history. Both
    // read the same seed; neither may erase the other's answer.
    let store = ConversationStore::new(8);
    let key = ConversationKey::new("slack", "c0123abc:1700000000.000001", SUBJECT);
    let allowed = granted(&["echo.echo"]);
    let now = Instant::now();

    let first = store.begin(&key, &allowed, window(), now);
    let second = store.begin(&key, &allowed, window(), now);
    assert!(first.history.is_empty() && second.history.is_empty());

    store.commit(
        &key,
        &allowed,
        window(),
        ConversationTurn::completed("what broke?", "two things"),
        &first.cache_key,
        now,
    );
    store.commit(
        &key,
        &allowed,
        window(),
        ConversationTurn::completed("still there?", "yes"),
        &second.cache_key,
        now,
    );

    let resumed = store.begin(&key, &allowed, window(), now);
    assert_eq!(resumed.history.len(), 2);
    assert_eq!(resumed.history.turns()[0].user(), "what broke?");
    assert_eq!(resumed.history.turns()[1].user(), "still there?");
    // Two sessions opening one new conversation mint two lanes, and the one that created the entry
    // is the lane the conversation keeps. The loser paid for one cache lookup on one message; the
    // alternative — the last writer renaming the lane every message — would leave every request
    // naming a lane no earlier request had ever used.
    assert_ne!(first.cache_key, second.cache_key);
    assert_eq!(resumed.cache_key, first.cache_key);
}

#[test]
fn the_store_prints_counts_rather_than_conversations() {
    // `History` and `ConversationTurn` both derive `Debug`, so a derived impl here would put whole
    // conversations into the log stream on one `tracing::debug!(?store)`.
    let store = ConversationStore::new(8);
    commit(
        &store,
        &ConversationKey::new("dev", "dev", SUBJECT),
        &granted(&["echo.echo"]),
        window(),
        ConversationTurn::completed("the secret question", "the secret answer"),
        Instant::now(),
    );

    let rendered = format!("{store:?}");
    assert!(rendered.contains("conversations: 1"), "{rendered}");
    assert!(rendered.contains("turns: 1"), "{rendered}");
    assert!(!rendered.contains("secret"), "{rendered}");
    assert!(!rendered.contains(SUBJECT), "{rendered}");
}

// ---------------------------------------------------------------------------
// Prompt cache keys
// ---------------------------------------------------------------------------

/// The same message, in a different conversation on the same transport.
fn message_in(conversation: &str, text: &str) -> InboundMessage {
    InboundMessage {
        conversation_id: conversation.to_owned(),
        ..message(text)
    }
}

#[test]
fn a_minted_cache_key_is_opaque_and_never_repeats() {
    // Both prefixes are crate constants and `IdSequence::new` rejects a malformed one, in which
    // case minting degrades to an empty key that `with_prompt_cache_key` then drops. That failure
    // is silent by design — a routing hint must not abort a message — so this is the test that
    // keeps the constants valid.
    let first = cache_key::for_conversation();
    let second = cache_key::for_conversation();
    let route = cache_key::for_route();

    for key in [&first, &second, &route] {
        assert!(!key.trim().is_empty(), "an empty key is no key at all");
    }
    assert_ne!(
        first, second,
        "two conversations minted in one process must not collide"
    );
    assert_ne!(route, cache_key::for_route());
}

#[test]
fn a_cache_key_carries_nothing_about_the_sender() {
    // The whole reason the key is minted rather than derived. A canonical subject can be a phone
    // number, so sending it — or a hash of it, which is a stable pseudonym — would hand a model
    // provider the sender's identity in exchange for routing that happens either way.
    const DISTINCTIVE: &str = "tel.15558675309";
    let store = ConversationStore::new(8);
    let key = ConversationKey::new("dev", "c0123abc", DISTINCTIVE);
    let seed = store.begin(&key, &granted(&["echo.echo"]), window(), Instant::now());

    for fragment in [DISTINCTIVE, "15558675309", "tel", "c0123abc"] {
        assert!(
            !seed.cache_key.contains(fragment),
            "{fragment:?} reached the cache key: {}",
            seed.cache_key
        );
    }
    // Nor does the conversation the sender is in, which on a shared channel is barely less
    // identifying than the sender.
    assert!(!cache_key::for_route().contains("c0123abc"));
}

#[test]
fn an_evicted_conversation_comes_back_with_a_new_cache_key() {
    // Rotation is what keeps the key from becoming a durable pseudonym, and it is also simply
    // correct: an evicted conversation rebuilds a prompt sharing no prefix with the one it
    // replaced, so naming the old lane would be a guaranteed miss.
    let store = ConversationStore::new(8);
    let key = ConversationKey::new("dev", "dev", SUBJECT);
    let allowed = granted(&["echo.echo"]);
    let start = Instant::now();

    let first = store.begin(&key, &allowed, window(), start);
    store.commit(
        &key,
        &allowed,
        window(),
        ConversationTurn::completed("what broke?", "two things"),
        &first.cache_key,
        start,
    );

    let warm = store.begin(&key, &allowed, window(), start + Duration::from_secs(60));
    assert_eq!(
        warm.cache_key, first.cache_key,
        "a live conversation stays in the lane its own turns warmed"
    );

    let cold = store.begin(&key, &allowed, window(), start + Duration::from_secs(900));
    assert!(
        cold.history.is_empty(),
        "the idle timeout dropped the entry"
    );
    assert_ne!(
        cold.cache_key, first.cache_key,
        "the same conversation identity must not keep naming a lane whose prefix is gone"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_conversation_keeps_one_cache_key_and_two_conversations_never_share_one() {
    // The point of the key: the second message of a conversation repeats the whole first exchange
    // as its prefix, and declaring the same lane is what lets the provider serve that prefix from
    // its cache instead of reading it again.
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(3, &["echo.echo"])).await;
    let models = ModelScript::new([
        answer("Two things broke."),
        answer("The second one was the database."),
        answer("Nothing is wrong over here."),
    ]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let route = persistent_route(model_config(), window());

    for message in [
        message_in("dev", "what broke?"),
        message_in("dev", "and the second one?"),
        message_in("dev-other", "anything wrong here?"),
    ] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message,
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }

    assert_eq!(
        models.cache_key(0),
        models.cache_key(1),
        "a follow-up must declare the lane its own earlier turn warmed"
    );
    assert_ne!(
        models.cache_key(0),
        models.cache_key(2),
        "two conversations share no prefix, so pointing them at one lane only wastes lookups"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_route_sends_every_sender_to_the_route_s_own_lane() {
    // A `oneShot` route's shared prefix is the agent's instructions and the tool definitions —
    // identical for everyone it answers and containing nothing about any of them — so one lane per
    // route shares what was already common property. Per-message keys would name a lane holding one
    // request and give up the only caching a stateless route can have.
    const OTHER_SUBJECT: &str = "tel.16035550100";
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(3, &["echo.echo"])).await;
    let models = ModelScript::new([answer("one"), answer("two"), answer("three")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    // Bound once and cloned per message, exactly as the routing table hands it to a session.
    let route = route(model_config());

    for message in [
        message_from(SUBJECT, "what broke?"),
        message_from(OTHER_SUBJECT, "and for me?"),
        message_from(SUBJECT, "still?"),
    ] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message,
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }

    assert_eq!(models.cache_key(0), route.cache_key);
    assert_eq!(
        models.cache_key(1),
        route.cache_key,
        "a second sender on one route uses the same lane, because the prefix is the route's"
    );
    assert_eq!(models.cache_key(2), route.cache_key);
    assert_eq!(
        runner.conversations.tracked(),
        0,
        "a lane is not a memory: a oneShot route still stores nothing"
    );
}

/// A model that never heard of routing metadata, implementing only the required trait method.
struct KeylessModel;

impl ModelFactory for KeylessModel {
    fn build(&self, _model: &ModelConfig) -> Result<Box<dyn ChatModel + Send>, SessionError> {
        Ok(Box::new(Self))
    }
}

impl ChatModel for KeylessModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        _tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
        Ok(answer(
            messages
                .last()
                .and_then(ModelMessage::content)
                .unwrap_or_default(),
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_never_heard_of_a_cache_key_still_answers() {
    // `complete_with` is a provided method precisely so this keeps working: an implementation that
    // ignores the options loses a cache lookup, never an answer.
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(1, &["echo.echo"])).await;
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner_with(broker, Arc::new(KeylessModel) as Arc<dyn ModelFactory>, 4);

    run_session(
        Arc::clone(&runner),
        persistent_route(model_config(), window()),
        message("what broke?"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec!["what broke?".to_owned()]);
    assert_eq!(
        runner.conversations.tracked(),
        1,
        "and the conversation it answered is remembered like any other"
    );
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// An in-memory transport whose messages a test supplies directly.
struct FakeTransport {
    name: String,
    inbound: mpsc::UnboundedReceiver<InboundMessage>,
    replier: Arc<RecordingReplier>,
}

impl ChatTransport for FakeTransport {
    fn name(&self) -> &str {
        &self.name
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(async move { Ok(TransportIdentity::default()) })
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            self.inbound
                .recv()
                .await
                .map(Box::new)
                .map(TransportEvent::Message)
                .ok_or(TransportError::Closed)
        })
    }

    fn replier(&self) -> Arc<dyn ChatReplier> {
        Arc::clone(&self.replier) as Arc<dyn ChatReplier>
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transport_reader_forwards_messages_and_stops_when_the_transport_does() {
    let (sender, inbound) = mpsc::unbounded_channel();
    let transport = FakeTransport {
        name: "dev".to_owned(),
        inbound,
        replier: Arc::new(RecordingReplier::default()),
    };
    let (routed, mut received) = mpsc::channel(4);
    let health = Arc::new(crate::TransportHealth::new(1));
    let reader = tokio::spawn(crate::read_transport(
        Box::new(transport),
        routed,
        Arc::clone(&health),
    ));

    sender
        .send(message("first"))
        .expect("fixture accepts a message");
    let TransportEvent::Message(received) = received.recv().await.expect("the reader forwards it")
    else {
        panic!("the fixture sent a message event");
    };
    assert_eq!(received.text, "first");

    drop(sender);
    reader.await.expect("the reader ends with its transport");
    assert_eq!(
        health.dead(),
        vec!["dev".to_owned()],
        "a transport that ended for good is recorded, not only logged once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_that_stops_because_the_daemon_stopped_is_not_a_dead_transport() {
    // The other way a reader ends: shutdown drops the routing loop, so the forward fails. Counting
    // that as a dead transport would announce a degraded gateway on every clean stop.
    let (sender, inbound) = mpsc::unbounded_channel();
    let transport = FakeTransport {
        name: "dev".to_owned(),
        inbound,
        replier: Arc::new(RecordingReplier::default()),
    };
    let (routed, received) = mpsc::channel(1);
    drop(received);
    let health = Arc::new(crate::TransportHealth::new(1));
    let reader = tokio::spawn(crate::read_transport(
        Box::new(transport),
        routed,
        Arc::clone(&health),
    ));

    sender
        .send(message("nobody is listening"))
        .expect("fixture accepts a message");
    reader.await.expect("the reader ends with the daemon");
    assert!(
        health.dead().is_empty(),
        "a reader ending with the daemon is not a transport failure"
    );
}

/// Everything `serve` needs when the test is about why it stopped rather than what it routed.
async fn idle_routing_loop(directory: &Path) -> (Arc<SessionRunner>, Arc<RoutingTable>) {
    let document = document(directory);
    let config = resolved(directory, &document).await;
    let routes = Arc::new(
        RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("route binds"),
    );
    let (broker, _observed) = stub_broker(directory, Vec::new()).await;
    (runner(broker, ModelScript::forbidden(), 4), routes)
}

#[tokio::test(flavor = "multi_thread")]
async fn losing_every_transport_ends_the_daemon_as_a_failure() {
    // Every reader gone and nobody asked for a shutdown: a gateway whose workspaces all fell off
    // their tokens has nothing left to answer with, and reporting success would let a supervisor
    // treat that as a clean run.
    let directory = temporary();
    let (runner, routes) = idle_routing_loop(directory.path()).await;
    let (sender, receiver) = mpsc::channel(4);
    drop(sender);

    let outcome = crate::serve(
        runner,
        routes,
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        receiver,
        std::future::pending(),
        Duration::from_secs(1),
    )
    .await;

    assert_eq!(outcome, crate::ServeOutcome::TransportsLost);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requested_shutdown_ends_the_daemon_successfully() {
    let directory = temporary();
    let (runner, routes) = idle_routing_loop(directory.path()).await;
    let (_sender, receiver) = mpsc::channel(4);

    let outcome = crate::serve(
        runner,
        routes,
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        receiver,
        std::future::ready(()),
        Duration::from_secs(1),
    )
    .await;

    assert_eq!(outcome, crate::ServeOutcome::Shutdown);
}

#[tokio::test(flavor = "multi_thread")]
async fn ambient_channel_traffic_is_ignored_unless_it_names_the_bot() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"][0]["match"] = json!({"kind": "channel", "channel": "c0123abc"});
    let config = resolved(directory.path(), &document).await;
    let routes = Arc::new(
        RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("route binds"),
    );

    let (broker, _observed) = stub_broker(directory.path(), Vec::new()).await;
    let models = ModelScript::forbidden();
    let runner = runner(broker, Arc::clone(&models), 4);
    let replier = Arc::new(RecordingReplier::default());
    let mut identities = BTreeMap::new();
    identities.insert(
        "dev".to_owned(),
        TransportIdentity {
            user_id: Some("U0BOTBOT".to_owned()),
            handle: None,
        },
    );
    let mut repliers: BTreeMap<String, Arc<dyn ChatReplier>> = BTreeMap::new();
    repliers.insert(
        "dev".to_owned(),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    );
    let mut sessions = tokio::task::JoinSet::new();

    let mut ambient = message("just chatting with my colleagues");
    ambient.conversation = ConversationKind::Channel("c0123abc".to_owned());
    crate::dispatch(
        &runner,
        &routes,
        &identities,
        &repliers,
        &mut sessions,
        ambient,
    );
    assert_eq!(
        sessions.len(),
        0,
        "ambient traffic must not start a session"
    );

    // Discord's structured mentions are authoritative. Presentation text cannot turn an explicit
    // `mentions` miss into a wakeup.
    let mut structurally_unaddressed = message("<@U0BOTBOT> presentation text");
    structurally_unaddressed.conversation = ConversationKind::Channel("c0123abc".to_owned());
    structurally_unaddressed.addressed = Some(false);
    crate::dispatch(
        &runner,
        &routes,
        &identities,
        &repliers,
        &mut sessions,
        structurally_unaddressed,
    );
    assert_eq!(sessions.len(), 0, "structured addressing must win");

    // A message on a channel with no route is ignored just as quietly.
    let mut elsewhere = message("<@U0BOTBOT> hello");
    elsewhere.conversation = ConversationKind::Channel("c9999zzz".to_owned());
    crate::dispatch(
        &runner,
        &routes,
        &identities,
        &repliers,
        &mut sessions,
        elsewhere,
    );
    assert_eq!(
        sessions.len(),
        0,
        "an unrouted channel must not start a session"
    );

    let mut addressed = message("<@U0BOTBOT> what is the status?");
    addressed.conversation = ConversationKind::Channel("c0123abc".to_owned());
    crate::dispatch(
        &runner,
        &routes,
        &identities,
        &repliers,
        &mut sessions,
        addressed,
    );
    assert_eq!(sessions.len(), 1, "an addressed message starts one session");
    sessions.abort_all();
    while sessions.join_next().await.is_some() {}
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catch_all_channel_route_still_waits_to_be_summoned() {
    // The property a route matching every channel must not quietly cost: a route decides *which*
    // agent answers, and the mention decides *whether* anything answers at all. Widening the first
    // leaves the second exactly where it was, or the bot would run a session on every message in
    // every channel it sits in.
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"][0]["match"] = json!({"kind": "channel"});
    let config = resolved(directory.path(), &document).await;
    let routes = Arc::new(
        RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("route binds"),
    );

    let (broker, _observed) = stub_broker(directory.path(), Vec::new()).await;
    let runner = runner(broker, ModelScript::forbidden(), 4);
    let replier = Arc::new(RecordingReplier::default());
    let identities = BTreeMap::from([(
        "dev".to_owned(),
        TransportIdentity {
            user_id: Some("U0BOTBOT".to_owned()),
            handle: None,
        },
    )]);
    let repliers = BTreeMap::from([(
        "dev".to_owned(),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )]);
    let mut sessions = tokio::task::JoinSet::new();

    // A channel this configuration never names, which is the whole point of the catch-all.
    let mut ambient = message("just chatting with my colleagues");
    ambient.conversation = ConversationKind::Channel("c9999zzz".to_owned());
    crate::dispatch(
        &runner,
        &routes,
        &identities,
        &repliers,
        &mut sessions,
        ambient,
    );
    assert_eq!(
        sessions.len(),
        0,
        "a matched channel is not a wakeup on its own"
    );

    let mut addressed = message("what is the status?");
    addressed.conversation = ConversationKind::Channel("c9999zzz".to_owned());
    // Discord supplies this from its authenticated `mentions` array rather than presentation text.
    addressed.addressed = Some(true);
    crate::dispatch(
        &runner,
        &routes,
        &identities,
        &repliers,
        &mut sessions,
        addressed,
    );
    assert_eq!(sessions.len(), 1, "and being summoned in one is");
    sessions.abort_all();
    while sessions.join_next().await.is_some() {}
}

// ---------------------------------------------------------------------------
// Slack Socket Mode
// ---------------------------------------------------------------------------

/// The next routable message from any transport, failing the test rather than hanging on it.
fn expect_message(event: TransportEvent) -> InboundMessage {
    let TransportEvent::Message(message) = event else {
        panic!("expected a message event");
    };
    *message
}

async fn next_message(transport: &mut dyn ChatTransport) -> InboundMessage {
    let event = tokio::time::timeout(Duration::from_secs(5), transport.next())
        .await
        .expect("a message arrives before the test gives up")
        .expect("a routable event");
    expect_message(event)
}

/// A loopback HTTP mock serving Slack's token-only methods.
///
/// Hand-rolled rather than a framework: this has to answer exactly what the transport asks for and
/// record it, and a real socket is what proves the request left the process.
struct HttpMock {
    base: String,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl HttpMock {
    /// Paths and request bodies the transport sent, in order.
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("mock call log").clone()
    }
}

/// Serves loopback HTTP until the test drops, answering through `handler`.
fn spawn_http_mock<H>(handler: H) -> HttpMock
where
    H: Fn(&str, &str) -> Value + Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
    let address = listener.local_addr().expect("mock endpoint address");
    listener
        .set_nonblocking(true)
        .expect("mock endpoint is pollable");
    let listener = tokio::net::TcpListener::from_std(listener).expect("mock endpoint adopts");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    tokio::spawn(async move {
        let handler = Arc::new(handler);
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut stream = stream;
                let Some((path, body)) = read_http_request(&mut stream).await else {
                    return;
                };
                recorded
                    .lock()
                    .expect("mock call log")
                    .push((path.clone(), body.clone()));
                let response = handler(&path, &body);
                let encoded = serde_json::to_vec(&response).expect("mock response serializes");
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    encoded.len()
                );
                use tokio::io::AsyncWriteExt as _;
                let _ = stream.write_all(headers.as_bytes()).await;
                let _ = stream.write_all(&encoded).await;
                let _ = stream.flush().await;
            });
        }
    });

    HttpMock {
        base: format!("http://{address}"),
        calls,
    }
}

/// A raw-body loopback mock for attachment downloads and non-200 responses.
///
/// Its call records retain request headers rather than bodies so CDN credential boundaries can be
/// asserted directly.
struct RawHttpMock {
    base: String,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl RawHttpMock {
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("raw mock call log").clone()
    }
}

fn spawn_raw_http_mock<H>(handler: H) -> RawHttpMock
where
    H: Fn(&str) -> (u16, &'static str, Vec<u8>) + Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("raw mock endpoint binds");
    let address = listener.local_addr().expect("raw mock endpoint address");
    listener
        .set_nonblocking(true)
        .expect("raw mock endpoint is pollable");
    let listener = tokio::net::TcpListener::from_std(listener).expect("raw mock endpoint adopts");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&calls);
    tokio::spawn(async move {
        let handler = Arc::new(handler);
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut stream = stream;
                let Some((path, headers, _body)) = read_http_request_parts(&mut stream).await
                else {
                    return;
                };
                recorded
                    .lock()
                    .expect("raw mock call log")
                    .push((path.clone(), headers));
                let (status, content_type, response) = handler(&path);
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let headers = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                use tokio::io::AsyncWriteExt as _;
                let _ = stream.write_all(headers.as_bytes()).await;
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            });
        }
    });
    RawHttpMock {
        base: format!("http://{address}"),
        calls,
    }
}

/// Reads one complete HTTP request, returning its path (with query) and body.
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let (path, _headers, body) = read_http_request_parts(stream).await?;
    Some((path, body))
}

/// The same request with raw headers retained for credential-boundary assertions.
async fn read_http_request_parts(
    stream: &mut tokio::net::TcpStream,
) -> Option<(String, String, String)> {
    use tokio::io::AsyncReadExt as _;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).ok()?;
    let path = headers
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .to_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = String::from_utf8(bytes[header_end..].to_vec()).ok()?;
    Some((path, headers, body))
}

/// Everything one mock Socket Mode connection recorded and can be told to send.
struct SocketMock {
    url: String,
    acks: mpsc::UnboundedReceiver<String>,
}

/// Serves one Socket Mode connection: greets, sends `frames`, and reports every ack it received.
fn spawn_socket_mock(frames: Vec<Value>) -> SocketMock {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("socket mock binds");
    let address = listener.local_addr().expect("socket mock address");
    listener
        .set_nonblocking(true)
        .expect("socket mock is pollable");
    let listener = tokio::net::TcpListener::from_std(listener).expect("socket mock adopts");
    let (acks, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message;
        let hello = json!({"type": "hello", "num_connections": 1}).to_string();
        if socket.send(Message::text(hello)).await.is_err() {
            return;
        }
        for frame in frames {
            if socket.send(Message::text(frame.to_string())).await.is_err() {
                return;
            }
        }
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = message {
                let _ = acks.send(text.to_string());
            }
        }
    });

    SocketMock {
        url: format!("ws://{address}"),
        acks: receiver,
    }
}

const BOT_USER: &str = "u0botbot";
const TEAM: &str = "t0123abc";

fn events_envelope(envelope_id: &str, event: Value) -> Value {
    json!({
        "envelope_id": envelope_id,
        "type": "events_api",
        "accepts_response_payload": false,
        "payload": { "team_id": TEAM, "event": event }
    })
}

fn direct_message(user: &str, ts: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "channel": "d0123abc",
        "channel_type": "im",
        "user": user,
        "ts": ts,
        "text": text
    })
}

/// One shared-channel message, threaded when `thread_ts` is given.
///
/// Slack sends `thread_ts` only on replies *inside* a thread; the message that starts one arrives
/// without it, which is exactly the asymmetry the conversation identity has to absorb.
fn channel_message(user: &str, ts: &str, thread_ts: Option<&str>, text: &str) -> Value {
    let mut event = json!({
        "type": "message",
        "channel": "c0123abc",
        "channel_type": "channel",
        "user": user,
        "ts": ts,
        "text": text
    });
    if let Some(thread_ts) = thread_ts {
        event["thread_ts"] = json!(thread_ts);
    }
    event
}

/// One Slack transport pointed at loopback mocks.
fn slack(endpoint: &str) -> crate::transport::slack::SlackTransport {
    slack_with(
        endpoint,
        SlackExperience::Classic,
        SlackActivityConfig::default(),
    )
}

fn slack_with(
    endpoint: &str,
    experience: SlackExperience,
    activity: SlackActivityConfig,
) -> crate::transport::slack::SlackTransport {
    crate::transport::slack::SlackTransport::new(
        "scientist-slack".to_owned(),
        endpoint.to_owned(),
        "xapp-test-app-token".to_owned(),
        "xoxb-test-bot-token".to_owned(),
        experience,
        activity,
    )
    .expect("slack transport builds")
}

fn slack_handler(sockets: Vec<String>) -> impl Fn(&str, &str) -> Value + Send + Sync + 'static {
    let sockets = Mutex::new(VecDeque::from(sockets));
    move |path, _body| match path {
        "/api/auth.test" => json!({"ok": true, "user_id": BOT_USER, "team_id": TEAM}),
        "/api/apps.connections.open" => {
            let url = sockets
                .lock()
                .expect("socket url queue")
                .pop_front()
                .unwrap_or_default();
            json!({"ok": true, "url": url})
        }
        "/api/chat.postMessage" => json!({"ok": true, "ts": "1700000000.000100"}),
        _ => json!({"ok": false, "error": "unknown_method"}),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_envelope_is_acknowledged_before_the_session_that_answers_it() {
    // Slack redelivers in about three seconds and a session runs for far longer, so acknowledging
    // after the work would guarantee duplicates rather than merely risk them. The model here
    // blocks until the test has already observed the ack, which is the ordering under test.
    let directory = temporary();
    let mut socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        direct_message("u9xyz", "1700000000.000001", "how are things?"),
    )]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let model = BlockedModel::new("All good.");
    let replier = transport.replier();
    let message = expect_message(
        transport
            .next()
            .await
            .expect("one routable message arrives"),
    );

    let session = tokio::spawn(run_session(
        runner_with(
            broker,
            Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
            4,
        ),
        route(model_config()),
        message,
        replier,
    ));

    // The model has been entered and is still blocked, so no answer has been produced yet.
    model.wait_until_entered().await;
    let ack = tokio::time::timeout(Duration::from_secs(5), socket.acks.recv())
        .await
        .expect("the envelope is acknowledged while the session is still running")
        .expect("the mock received an ack");
    assert_eq!(
        serde_json::from_str::<Value>(&ack).expect("ack is JSON")["envelope_id"],
        "envelope-1"
    );

    model.release();
    session.await.expect("the session completes");
    let posted = http
        .calls()
        .into_iter()
        .find(|(path, _)| path == "/api/chat.postMessage")
        .expect("the answer was posted to chat");
    let body = serde_json::from_str::<Value>(&posted.1).expect("post body is JSON");
    assert_eq!(body["text"], "All good.");
    assert_eq!(body["channel"], "d0123abc");
    // A direct message has no thread to join, and answering in one would hide the reply.
    assert!(body.get("thread_ts").is_none(), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redelivered_slack_envelope_is_routed_once() {
    let event = direct_message("u9xyz", "1700000000.000001", "hello");
    let socket = spawn_socket_mock(vec![
        events_envelope("envelope-1", event.clone()),
        events_envelope("envelope-2", event),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let first = expect_message(transport.next().await.expect("the first delivery routes"));
    assert_eq!(first.text, "hello");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), transport.next())
            .await
            .is_err(),
        "a redelivery of the same message must not route a second session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_disconnect_reconnects_on_a_fresh_socket() {
    // Slack rotates sockets on its own schedule. A disconnect is routine, and a transport that
    // treated it as a failure would go quiet until someone restarted the daemon.
    let second = spawn_socket_mock(vec![events_envelope(
        "envelope-2",
        direct_message("u9xyz", "1700000000.000002", "after reconnect"),
    )]);
    let first = spawn_socket_mock(vec![
        json!({"type": "disconnect", "reason": "refresh_requested"}),
    ]);
    let http = spawn_http_mock(slack_handler(vec![first.url.clone(), second.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let message = expect_message(
        tokio::time::timeout(Duration::from_secs(10), transport.next())
            .await
            .expect("the transport reconnects on its own")
            .expect("a message arrives on the second socket"),
    );
    assert_eq!(message.text, "after reconnect");
    assert_eq!(
        http.calls()
            .iter()
            .filter(|(path, _)| path == "/api/apps.connections.open")
            .count(),
        2,
        "a disconnect must open a second socket"
    );
}

/// A socket that negotiates and then says nothing: the handshake succeeds, the greeting never comes.
fn spawn_mute_socket_mock() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("socket mock binds");
    let address = listener.local_addr().expect("socket mock address");
    listener
        .set_nonblocking(true)
        .expect("socket mock is pollable");
    let listener = tokio::net::TcpListener::from_std(listener).expect("socket mock adopts");
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(_socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        // Held open, negotiated, and mute for as long as the test runs.
        std::future::pending::<()>().await;
    });
    format!("ws://{address}")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_slack_socket_is_abandoned_rather_than_waited_on_forever() {
    // A half-open connection — a NAT table forgetting the flow, a partition with no RST — reads
    // exactly like a healthy socket with nothing to say. Slack pings about every 30 seconds and
    // never goes quiet on its own, so silence past the deadline is a dead path: without one, the
    // reader waits on it forever and every route on this workspace goes silent with no log line.
    let second = spawn_socket_mock(vec![events_envelope(
        "envelope-2",
        direct_message("u9xyz", "1700000000.000002", "after the wedge"),
    )]);
    let wedged = spawn_socket_mock(Vec::new());
    let http = spawn_http_mock(slack_handler(vec![wedged.url.clone(), second.url.clone()]));
    let mut transport = slack(&http.base).with_deadline(Duration::from_millis(100));
    transport.connect().await.expect("slack transport connects");

    let message = expect_message(
        tokio::time::timeout(Duration::from_secs(10), transport.next())
            .await
            .expect("the transport gives up on a socket that stopped speaking")
            .expect("a message arrives on the socket that replaced it"),
    );
    assert_eq!(message.text, "after the wedge");
    assert_eq!(
        http.calls()
            .iter()
            .filter(|(path, _)| path == "/api/apps.connections.open")
            .count(),
        2,
        "the wedged socket must be replaced rather than held"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_socket_that_never_greets_fails_inside_open() {
    // The same wedge one round earlier: neither the handshake nor the `hello` wait had a deadline
    // of its own, so a URL that accepts a connection and then stops parks `connect` for good.
    let http = spawn_http_mock(slack_handler(vec![spawn_mute_socket_mock()]));
    let mut transport = slack(&http.base).with_deadline(Duration::from_millis(100));

    let error = tokio::time::timeout(Duration::from_secs(10), transport.connect())
        .await
        .expect("open bounds the greeting it waits for")
        .expect_err("a socket that never greets is not a connected transport");

    assert_eq!(
        error.category(),
        "closed",
        "an expired deadline takes the reconnect path the backoff loop already owns"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_messages_the_bot_itself_posted_are_never_routed() {
    // Both checks matter: another app's post carries `bot_id`, and this app's own post arrives with
    // the bot's user identifier and no `bot_id` at all. Either one routing would be a loop.
    let socket = spawn_socket_mock(vec![
        events_envelope(
            "envelope-1",
            direct_message(BOT_USER, "1700000000.000001", "my own answer"),
        ),
        events_envelope(
            "envelope-2",
            json!({
                "type": "message",
                "channel": "d0123abc",
                "channel_type": "im",
                "bot_id": "B0OTHER",
                "user": "u9xyz",
                "ts": "1700000000.000002",
                "text": "another app's post"
            }),
        ),
        events_envelope(
            "envelope-3",
            direct_message("u9xyz", "1700000000.000003", "a real question"),
        ),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let message = expect_message(
        tokio::time::timeout(Duration::from_secs(5), transport.next())
            .await
            .expect("the third envelope routes")
            .expect("a routable message"),
    );
    assert_eq!(message.text, "a real question");
    assert_eq!(message.subject.canonical(), "slack.t0123abc.u9xyz");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_upload_is_routed_and_described_for_numbering() {
    // The transport reports what arrived and stops there. Numbering belongs to the asset store,
    // because two transports minting their own identifiers would collide inside one conversation,
    // so the reference line a model reads is composed later by the session.
    let socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        json!({
            "type": "message",
            "subtype": "file_share",
            "channel": "d0123abc",
            "channel_type": "im",
            "user": "u9xyz",
            "ts": "1700000000.000001",
            "text": "Can you see my attached screenshot?",
            "files": [{
                "id": "F0123",
                "name": "image.png",
                "mimetype": "image/png",
                "size": 2048,
                "url_private_download": "https://files.slack.com/f/F0123/image.png"
            }]
        }),
    )]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let message = next_message(&mut transport).await;
    assert_eq!(message.text, "Can you see my attached screenshot?");
    assert_eq!(
        message.assets,
        vec![PendingAsset {
            name: "image.png".to_owned(),
            mime: "image/png".to_owned(),
            size: 2048,
            source: Some(AssetSourceRef::Slack {
                file_id: "F0123".to_owned(),
                url: "https://files.slack.com/f/F0123/image.png".to_owned(),
            }),
        }]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_upload_with_no_comment_is_still_a_request() {
    // An upload posted with no comment carries an empty `text`, and the attachment is the whole
    // message. Dropping it would be the same silence the subtype filter used to produce.
    let socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        json!({
            "type": "message",
            "subtype": "file_share",
            "channel": "d0123abc",
            "channel_type": "im",
            "user": "u9xyz",
            "ts": "1700000000.000001",
            "text": "",
            "files": [{
                "id": "F0123",
                "name": "one.png",
                "mimetype": "image/png",
                "size": 10,
                "url_private_download": "https://files.slack.com/f/F0123/one.png"
            }]
        }),
    )]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let message = next_message(&mut transport).await;
    assert!(message.text.is_empty());
    assert_eq!(message.assets.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_file_the_app_cannot_see_is_described_without_a_source() {
    // Slack withholds the id and the URL for a file the token has no access to. It is still
    // described, because "there is something here I cannot open" is a better answer than silence —
    // and it carries no source, so nothing can try to fetch it.
    let socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        json!({
            "type": "message",
            "subtype": "file_share",
            "channel": "d0123abc",
            "channel_type": "im",
            "user": "u9xyz",
            "ts": "1700000000.000001",
            "text": "have a look",
            "files": [{ "file_access": "check_file_info" }]
        }),
    )]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let message = next_message(&mut transport).await;
    assert_eq!(message.text, "have a look");
    assert_eq!(message.assets.len(), 1);
    assert!(message.assets[0].source.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_subtypes_that_are_events_about_a_message_are_still_dropped() {
    // The allowlist has to stay an allowlist. An edit, a deletion, and a channel join are events
    // *about* messages, and routing any of them would answer a question twice or answer nobody.
    let socket = spawn_socket_mock(vec![
        events_envelope(
            "envelope-1",
            json!({
                "type": "message",
                "subtype": "message_changed",
                "channel": "d0123abc",
                "channel_type": "im",
                "user": "u9xyz",
                "ts": "1700000000.000001",
                "text": "an edit"
            }),
        ),
        events_envelope(
            "envelope-2",
            json!({
                "type": "message",
                "subtype": "message_deleted",
                "channel": "d0123abc",
                "channel_type": "im",
                "user": "u9xyz",
                "ts": "1700000000.000002",
                "text": "a deletion"
            }),
        ),
        events_envelope(
            "envelope-3",
            json!({
                "type": "message",
                "subtype": "channel_join",
                "channel": "d0123abc",
                "channel_type": "im",
                "user": "u9xyz",
                "ts": "1700000000.000003",
                "text": "u9xyz has joined the channel"
            }),
        ),
        events_envelope(
            "envelope-4",
            direct_message("u9xyz", "1700000000.000004", "a real question"),
        ),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let message = next_message(&mut transport).await;
    assert_eq!(message.text, "a real question");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_message_with_neither_text_nor_a_file_is_not_a_request() {
    // Text became optional so an uncommented upload could route. Nothing else may ride in on that:
    // an empty message is not a question and must not start a session.
    let socket = spawn_socket_mock(vec![
        events_envelope(
            "envelope-1",
            json!({
                "type": "message",
                "channel": "d0123abc",
                "channel_type": "im",
                "user": "u9xyz",
                "ts": "1700000000.000001",
                "text": "   "
            }),
        ),
        events_envelope(
            "envelope-2",
            direct_message("u9xyz", "1700000000.000002", "a real question"),
        ),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let message = next_message(&mut transport).await;
    assert_eq!(message.text, "a real question");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_thread_and_the_message_that_opened_it_are_one_conversation() {
    // The failure this field exists to prevent. Slack omits `thread_ts` on the message that starts
    // a thread and sends it on every reply inside one, while the bot answers that first message
    // *in* a thread rooted at it. Anything keyed on `thread` therefore files the opening question
    // apart from every answer to it, orphaning the first turn of every threaded conversation.
    let socket = spawn_socket_mock(vec![
        events_envelope(
            "envelope-1",
            channel_message(
                "u9xyz",
                "1700000000.000001",
                None,
                "<@u0botbot> what broke?",
            ),
        ),
        events_envelope(
            "envelope-2",
            channel_message(
                "u9xyz",
                "1700000000.000002",
                Some("1700000000.000001"),
                "<@u0botbot> and since when?",
            ),
        ),
        events_envelope(
            "envelope-3",
            channel_message(
                "u9xyz",
                "1700000000.000003",
                Some("1699999999.000009"),
                "<@u0botbot> different subject entirely",
            ),
        ),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let opening = next_message(&mut transport).await;
    let reply = next_message(&mut transport).await;
    let elsewhere = next_message(&mut transport).await;

    // The asymmetry itself, so the derivation below has something to be right about.
    assert_eq!(opening.thread, None);
    assert_eq!(reply.thread.as_deref(), Some("1700000000.000001"));

    assert_eq!(
        opening.conversation_id, reply.conversation_id,
        "the message that opened a thread and a reply inside it are one conversation"
    );
    assert_eq!(opening.conversation_id, "c0123abc:1700000000.000001");
    assert_ne!(
        opening.conversation_id, elsewhere.conversation_id,
        "two threads in one channel are two conversations"
    );
    // The identity is the thread the *answer* joins, which is why it survives the first turn.
    assert_eq!(
        opening.reply,
        ReplyTarget::Slack {
            channel: "c0123abc".to_owned(),
            thread_ts: Some("1700000000.000001".to_owned()),
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_direct_message_is_one_conversation_across_its_messages() {
    // A DM has no thread to join and the transport deliberately answers outside one, so the whole
    // conversation is the DM channel and stays that way however many messages arrive in it.
    let socket = spawn_socket_mock(vec![
        events_envelope(
            "envelope-1",
            direct_message("u9xyz", "1700000000.000001", "how are things?"),
        ),
        events_envelope(
            "envelope-2",
            direct_message("u9xyz", "1700000000.000002", "and one more thing"),
        ),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let first = next_message(&mut transport).await;
    let second = next_message(&mut transport).await;

    assert_eq!(first.conversation_id, "d0123abc");
    assert_eq!(
        first.conversation_id, second.conversation_id,
        "a direct message is one conversation across its messages"
    );
    assert_eq!(
        first.reply,
        ReplyTarget::Slack {
            channel: "d0123abc".to_owned(),
            thread_ts: None,
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_agent_activity_uses_thread_sessions_and_explicit_lifecycle_states() {
    let socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        direct_message("u9xyz", "1700000000.000001", "handle this"),
    )]);
    let socket_url = socket.url.clone();
    let http = spawn_http_mock(move |path, _body| match path {
        "/api/auth.test" => json!({"ok": true, "user_id": BOT_USER, "team_id": TEAM}),
        "/api/apps.connections.open" => json!({"ok": true, "url": socket_url.clone()}),
        "/api/agents.sessions.setStatus" => json!({"ok": true, "status": "processing"}),
        _ => json!({"ok": false, "error": "unknown_method"}),
    });
    let mut transport = slack_with(
        &http.base,
        SlackExperience::Agent,
        SlackActivityConfig {
            mode: ActivityMode::Native,
            classic_fallback: SlackActivityFallback::Reaction,
        },
    );
    transport.connect().await.expect("Slack Agent connects");
    let message = next_message(&mut transport).await;

    assert_eq!(message.thread.as_deref(), Some("1700000000.000001"));
    assert_eq!(message.conversation_id, "d0123abc:1700000000.000001");
    assert_eq!(
        message.reply,
        ReplyTarget::Slack {
            channel: "d0123abc".to_owned(),
            thread_ts: Some("1700000000.000001".to_owned()),
        }
    );
    let target = message.activity.expect("Agent activity target");
    let activity = transport.activity().expect("native activity is configured");
    activity
        .show(target.clone())
        .await
        .expect("processing status succeeds");
    activity.hide(target).await.expect("active status succeeds");

    let status_calls = http
        .calls()
        .into_iter()
        .filter(|(path, _)| path == "/api/agents.sessions.setStatus")
        .map(|(_, body)| serde_json::from_str::<Value>(&body).expect("status body is JSON"))
        .collect::<Vec<_>>();
    assert_eq!(status_calls.len(), 2);
    assert_eq!(status_calls[0]["status"], "processing");
    assert_eq!(status_calls[0]["channel_id"], "d0123abc");
    assert_eq!(status_calls[0]["thread_ts"], "1700000000.000001");
    assert_eq!(status_calls[0]["initiator_user_id"], "u9xyz");
    assert_eq!(status_calls[1]["status"], "active");
    assert!(status_calls[1].get("initiator_user_id").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_permanently_degrades_agent_status_to_owned_tangerine_reactions() {
    let socket = spawn_socket_mock(vec![
        events_envelope(
            "envelope-1",
            direct_message("u9xyz", "1700000000.000001", "first"),
        ),
        events_envelope(
            "envelope-2",
            direct_message("u9xyz", "1700000000.000002", "second"),
        ),
    ]);
    let socket_url = socket.url.clone();
    let http = spawn_http_mock(move |path, _body| match path {
        "/api/auth.test" => json!({"ok": true, "user_id": BOT_USER, "team_id": TEAM}),
        "/api/apps.connections.open" => json!({"ok": true, "url": socket_url.clone()}),
        "/api/agents.sessions.setStatus" => json!({"ok": false, "error": "feature_disabled"}),
        "/api/reactions.add" | "/api/reactions.remove" => json!({"ok": true}),
        _ => json!({"ok": false, "error": "unknown_method"}),
    });
    let mut transport = slack_with(
        &http.base,
        SlackExperience::Agent,
        SlackActivityConfig {
            mode: ActivityMode::Native,
            classic_fallback: SlackActivityFallback::Reaction,
        },
    );
    transport.connect().await.expect("Slack connects");
    let activity = transport.activity().expect("activity configured");

    for _ in 0..2 {
        let target = next_message(&mut transport)
            .await
            .activity
            .expect("activity target");
        activity
            .show(target.clone())
            .await
            .expect("reaction fallback succeeds");
        activity
            .hide(target)
            .await
            .expect("owned reaction is removed");
    }

    let calls = http.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|(path, _)| path == "/api/agents.sessions.setStatus")
            .count(),
        1,
        "feature_disabled trips one installation-wide breaker"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(path, _)| path == "/api/reactions.add")
            .count(),
        2
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(path, _)| path == "/api/reactions.remove")
            .count(),
        2
    );
    for (_, body) in calls
        .iter()
        .filter(|(path, _)| path.starts_with("/api/reactions."))
    {
        let body = serde_json::from_str::<Value>(body).expect("reaction body is JSON");
        assert_eq!(body["name"], "tangerine");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_does_not_remove_a_reaction_this_generation_did_not_add() {
    let socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        direct_message("u9xyz", "1700000000.000001", "already marked"),
    )]);
    let socket_url = socket.url.clone();
    let http = spawn_http_mock(move |path, _body| match path {
        "/api/auth.test" => json!({"ok": true, "user_id": BOT_USER, "team_id": TEAM}),
        "/api/apps.connections.open" => json!({"ok": true, "url": socket_url.clone()}),
        "/api/reactions.add" => json!({"ok": false, "error": "already_reacted"}),
        "/api/reactions.remove" => json!({"ok": true}),
        _ => json!({"ok": false, "error": "unknown_method"}),
    });
    let mut transport = slack_with(
        &http.base,
        SlackExperience::Classic,
        SlackActivityConfig {
            mode: ActivityMode::Native,
            classic_fallback: SlackActivityFallback::Reaction,
        },
    );
    transport.connect().await.expect("classic Slack connects");
    let target = next_message(&mut transport)
        .await
        .activity
        .expect("reaction target");
    let activity = transport.activity().expect("fallback configured");
    activity
        .show(target.clone())
        .await
        .expect("a pre-existing bot reaction is already visible");
    activity.hide(target).await.expect("cleanup is a no-op");

    assert_eq!(
        http.calls()
            .iter()
            .filter(|(path, _)| path == "/api/reactions.remove")
            .count(),
        0,
        "cleanup ownership comes only from a successful add"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_lost_reaction_response_never_grants_cleanup_ownership() {
    let socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        direct_message("u9xyz", "1700000000.000001", "ambiguous add"),
    )]);
    let socket_url = socket.url.clone();
    let http = spawn_raw_http_mock(move |path| match path {
        "/api/auth.test" => (
            200,
            "application/json",
            serde_json::to_vec(&json!({"ok": true, "user_id": BOT_USER, "team_id": TEAM}))
                .expect("auth response serializes"),
        ),
        "/api/apps.connections.open" => (
            200,
            "application/json",
            serde_json::to_vec(&json!({"ok": true, "url": socket_url.clone()}))
                .expect("socket response serializes"),
        ),
        // The service may have accepted the add even though the response was lost/malformed. The
        // only safe ownership rule is to leave the possible marker rather than remove old state.
        "/api/reactions.add" => (200, "application/json", b"not-json".to_vec()),
        "/api/reactions.remove" => (200, "application/json", br#"{"ok":true}"#.to_vec()),
        _ => (404, "application/json", br#"{"ok":false}"#.to_vec()),
    });
    let mut transport = slack_with(
        &http.base,
        SlackExperience::Classic,
        SlackActivityConfig {
            mode: ActivityMode::Native,
            classic_fallback: SlackActivityFallback::Reaction,
        },
    );
    transport.connect().await.expect("classic Slack connects");
    let target = next_message(&mut transport)
        .await
        .activity
        .expect("reaction target");
    let activity = transport.activity().expect("fallback configured");
    assert!(activity.show(target.clone()).await.is_err());
    activity
        .hide(target)
        .await
        .expect("there is nothing owned to clear");

    assert!(
        !http
            .calls()
            .iter()
            .any(|(path, _)| path == "/api/reactions.remove"),
        "an ambiguous add response cannot authorize removal"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_agent_stop_events_are_acknowledged_and_decoded_as_control_not_prompts() {
    let mut socket = spawn_socket_mock(vec![
        events_envelope(
            "stop-envelope",
            json!({
                "type": "agent_session_stopped",
                "channel": "d0123abc",
                "thread_ts": "1700000000.000001",
                "message_ts": "1700000000.000002",
                "user": "u9xyz"
            }),
        ),
        events_envelope(
            "stop-envelope-alias",
            json!({
                "type": "agent_session_stopped",
                "channel_id": "d0123abc",
                "thread_ts": "1700000000.000003",
                "message_ts": "1700000000.000004",
                "user_id": "u9xyz"
            }),
        ),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack_with(
        &http.base,
        SlackExperience::Agent,
        SlackActivityConfig::default(),
    );
    transport.connect().await.expect("Slack Agent connects");

    let event = tokio::time::timeout(Duration::from_secs(5), transport.next())
        .await
        .expect("control event arrives")
        .expect("control event decodes");
    assert_eq!(
        event,
        TransportEvent::SessionStopped(crate::transport::SessionStop {
            transport: "scientist-slack".to_owned(),
            conversation_id: "d0123abc:1700000000.000001".to_owned(),
            subject: "slack.t0123abc.u9xyz"
                .parse()
                .expect("canonical Slack subject"),
        })
    );
    let alias = tokio::time::timeout(Duration::from_secs(5), transport.next())
        .await
        .expect("aliased control event arrives")
        .expect("aliased control event decodes");
    assert_eq!(
        alias,
        TransportEvent::SessionStopped(crate::transport::SessionStop {
            transport: "scientist-slack".to_owned(),
            conversation_id: "d0123abc:1700000000.000003".to_owned(),
            subject: "slack.t0123abc.u9xyz"
                .parse()
                .expect("canonical Slack subject"),
        })
    );

    let mut acknowledged = Vec::new();
    for _ in 0..2 {
        let ack = tokio::time::timeout(Duration::from_secs(5), socket.acks.recv())
            .await
            .expect("Stop envelope was acknowledged")
            .expect("mock received the ack");
        acknowledged.push(
            serde_json::from_str::<Value>(&ack).expect("ack is JSON")["envelope_id"]
                .as_str()
                .expect("ack id")
                .to_owned(),
        );
    }
    assert_eq!(acknowledged, ["stop-envelope", "stop-envelope-alias"]);
}

// ---------------------------------------------------------------------------
// Slack markdown rendering
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_answer_is_posted_as_a_markdown_block() {
    // A model writes CommonMark. Slack's `text` field is mrkdwn, a proprietary syntax where bold is
    // `*one asterisk*`, so an answer posted through it alone arrives with `**bold**` rendered as
    // four literal asterisks. The `markdown` block hands the translation to Slack, which is the one
    // party that knows what its own client renders.
    let directory = temporary();
    let socket = spawn_socket_mock(vec![events_envelope(
        "envelope-1",
        direct_message("u9xyz", "1700000000.000001", "what is the slang?"),
    )]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack(&http.base);
    transport.connect().await.expect("slack transport connects");

    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
        )],
    )
    .await;
    let answer_text = "**Puñeta** is *vulgar*.\n\n| a | b |\n|---|---|\n| 1 | 2 |";
    let models = ModelScript::new([answer(answer_text)]);
    let replier = transport.replier();
    let message = next_message(&mut transport).await;

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message,
        replier,
    )
    .await;

    let posted = http
        .calls()
        .into_iter()
        .find(|(path, _)| path == "/api/chat.postMessage")
        .expect("the answer was posted to chat");
    let body = serde_json::from_str::<Value>(&posted.1).expect("post body is JSON");
    // Verbatim: anything this process rewrote would be a second translation of what Slack is about
    // to translate, and the table would not survive one.
    assert_eq!(body["blocks"][0]["type"], "markdown");
    assert_eq!(body["blocks"][0]["text"], answer_text);
    // The fallback a push notification shows, which is the one place blocks do not render.
    assert_eq!(body["text"], answer_text);
    assert_eq!(body["channel"], "d0123abc");
}

// ---------------------------------------------------------------------------
// Chat assets
// ---------------------------------------------------------------------------

fn pending(name: &str, mime: &str, size: u64) -> PendingAsset {
    PendingAsset {
        name: name.to_owned(),
        mime: mime.to_owned(),
        size,
        source: Some(AssetSourceRef::Slack {
            file_id: format!("F-{name}"),
            url: format!("https://files.slack.com/f/{name}"),
        }),
    }
}

fn asset_store() -> AssetStore {
    AssetStore::new(4, Duration::from_secs(600))
}

#[test]
fn an_asset_is_numbered_per_conversation_and_still_resolves_later() {
    // The number is the whole interface a model has to an attachment, so it has to mean one file
    // for as long as the reference line naming it is still being replayed.
    let store = asset_store();
    let now = Instant::now();
    let first = store.assets_for("c1", vec![pending("a.png", "image/png", 10)], true, now);
    let second = store.assets_for("c1", vec![pending("b.png", "image/png", 20)], true, now);
    assert_eq!(first.inventory[0].id, 1);
    assert_eq!(second.arrived, vec![2]);

    // A different conversation numbers from one again, and cannot see the first one's files.
    let other = store.assets_for("c2", vec![pending("c.png", "image/png", 30)], true, now);
    assert_eq!(other.inventory[0].id, 1);
    assert_eq!(
        store.get("c2", 2, now).map(|asset| asset.name),
        None,
        "a number must not resolve across conversations"
    );
    assert_eq!(
        store.get("c1", 1, now).map(|asset| asset.name),
        Some("a.png".to_owned())
    );
}

#[test]
fn a_reference_note_numbers_only_what_the_model_can_be_shown() {
    let store = asset_store();
    let now = Instant::now();
    let registered = store.assets_for(
        "c1",
        vec![
            pending("shot.png", "image/png", 2048),
            pending("clip.mov", "video/quicktime", 700 * 1024 * 1024),
            PendingAsset {
                name: "hidden".to_owned(),
                mime: String::new(),
                size: 0,
                source: None,
            },
        ],
        true,
        now,
    );
    let note = asset::reference_note(&registered, true).expect("a note for three files");

    assert!(
        note.contains("Chat Asset #1 — shot.png (image/png, 2 KB)"),
        "{note}"
    );
    // Named, and named as unreadable. Ignoring it is what produced the flat denial in the first
    // place; a number it cannot use would be worse.
    assert!(note.contains("clip.mov"), "{note}");
    assert!(!note.contains("Chat Asset #2"), "{note}");
    assert!(
        note.contains("the gateway cannot see this file at all"),
        "{note}"
    );
    assert!(note.contains("fetch_chat_asset"), "{note}");
    assert!(registered.fetchable);
}

#[test]
fn a_model_that_cannot_be_shown_images_is_offered_no_asset_number() {
    // The route's model decides this, not the media type. A local endpoint handed an image either
    // errors or invents an answer, and the default for `modalities` is deliberately empty.
    let store = asset_store();
    let registered = store.assets_for(
        "c1",
        vec![pending("shot.png", "image/png", 2048)],
        false,
        Instant::now(),
    );
    let note = asset::reference_note(&registered, false).expect("a note");

    assert!(!registered.fetchable);
    assert!(!note.contains("Chat Asset #"), "{note}");
    assert!(note.contains("cannot be shown images"), "{note}");
    assert!(!note.contains("fetch_chat_asset"), "{note}");
}

#[test]
fn an_attachment_stays_fetchable_on_later_messages_that_carry_none() {
    // The bug this pins, observed in a real conversation: someone sends a screenshot, the model
    // looks at it and answers, and then the *next* message withdraws the tool because that message
    // carried no attachment of its own. The reference line is still in replayed history, so the
    // model is left able to name `Chat Asset #1` and unable to open it — and answers from the
    // description it produced a turn ago rather than saying it cannot see. That reads as lying.
    let store = asset_store();
    let now = Instant::now();
    let first = store.assets_for(
        "c1",
        vec![pending("shot.png", "image/png", 2048)],
        true,
        now,
    );
    assert!(first.fetchable);

    // The follow-up: no attachment, same conversation.
    let second = store.assets_for("c1", Vec::new(), true, now);
    assert!(
        second.arrived.is_empty(),
        "a message that carried nothing brought nothing"
    );
    assert!(
        second.fetchable,
        "but the conversation's screenshot is still there to be looked at"
    );
    assert_eq!(
        store.get("c1", 1, now).map(|asset| asset.name),
        Some("shot.png".to_owned())
    );

    // A conversation that never had one still offers nothing.
    let elsewhere = store.assets_for("c2", Vec::new(), true, now);
    assert!(!elsewhere.fetchable);
}

#[test]
fn every_prompt_names_the_whole_inventory_not_just_what_just_arrived() {
    // The bug this pins, observed in a real conversation: a PDF is sent, ordinary chatter follows,
    // and nine messages later the model answers that it has never been sent a PDF. It was telling
    // the truth about the prompt it could see. The reference line naming `Chat Asset #3` lived only
    // in the turn that carried it, and a twelve-turn history window had trimmed that turn away —
    // while the store still held the file for another hour. A number a model cannot see is a file
    // it cannot open.
    let store = asset_store();
    let now = Instant::now();
    store.assets_for(
        "c1",
        vec![PendingAsset {
            name: "recipe.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            size: 1024,
            source: Some(AssetSourceRef::Slack {
                file_id: "F-pdf".to_owned(),
                url: "https://files.slack.com/f/recipe".to_owned(),
            }),
        }],
        true,
        now,
    );

    // Chatter. None of these messages carries anything.
    for _ in 0..9 {
        store.assets_for("c1", Vec::new(), true, now);
    }

    // A later message brings its own file, and the note still has to name both.
    let registered = store.assets_for(
        "c1",
        vec![pending("shot.png", "image/png", 2048)],
        true,
        now,
    );
    let note = asset::reference_note(&registered, true).expect("a note");

    assert!(note.contains("Chat Asset #1 — recipe.pdf"), "{note}");
    assert!(note.contains("Chat Asset #2 — shot.png"), "{note}");
    // Marked, so "is this a good recipe?" reaches for the file that came with the question rather
    // than one from twenty messages ago.
    assert!(
        note.contains("shot.png (image/png, 2 KB) — attached to this message"),
        "{note}"
    );
    assert!(
        !note.contains("recipe.pdf (application/pdf, 1 KB) — attached to this message"),
        "the older file must not claim to be new: {note}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_asset_number_is_refused_in_words_rather_than_by_failing() {
    // A model that asked for the wrong number can say so and carry on. Ending the session would
    // turn a recoverable turn into the fixed failure line.
    let store = Arc::new(asset_store());
    store.assets_for(
        "c1",
        vec![pending("shot.png", "image/png", 10)],
        true,
        Instant::now(),
    );
    let assets = SessionAssets::new(
        Arc::clone(&store),
        "c1".to_owned(),
        None,
        tokio::runtime::Handle::current(),
        true,
        true,
    );

    let refusal = tokio::task::spawn_blocking(move || assets.fetch(99).expect_err("no such asset"))
        .await
        .expect("the blocking task completes");
    assert!(refusal.contains("no Chat Asset #99"), "{refusal}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_stops_opening_attachments_once_its_budget_is_spent() {
    // Four is a working allowance, not a tour of the conversation. The refusal is readable so the
    // model answers with what it has rather than retrying.
    let store = Arc::new(asset_store());
    let arriving = (0..8)
        .map(|index| pending(&format!("shot{index}.png"), "image/png", 10))
        .collect();
    store.assets_for("c1", arriving, true, Instant::now());
    let assets = SessionAssets::new(
        Arc::clone(&store),
        "c1".to_owned(),
        None,
        tokio::runtime::Handle::current(),
        true,
        true,
    );

    let refusal = tokio::task::spawn_blocking(move || {
        // No fetcher is wired, so each of these fails for its own reason; what matters is that the
        // budget is spent by the attempt rather than by the success.
        for id in 1..=4 {
            let _ = assets.fetch(id);
        }
        assets.fetch(5).expect_err("the budget is spent")
    })
    .await
    .expect("the blocking task completes");
    assert!(refusal.contains("already opened"), "{refusal}");
}

#[test]
fn a_redirect_away_from_slack_is_not_followed() {
    // `client()` refuses redirects globally so a bearer token is never forwarded by policy. The
    // one hop this transport follows by hand has to check the host itself, and a prefix comparison
    // would accept the lookalike below.
    assert!(crate::transport::slack::is_slack_file_url(
        "https://files.slack.com/f/F0123/shot.png"
    ));
    assert!(!crate::transport::slack::is_slack_file_url(
        "https://files.slack.com.evil.test/f/F0123/shot.png"
    ));
    assert!(!crate::transport::slack::is_slack_file_url(
        "https://evil.test/?x=files.slack.com"
    ));
    // Credentials in the authority must not smuggle a host past the check either.
    assert!(!crate::transport::slack::is_slack_file_url(
        "https://files.slack.com@evil.test/f/F0123"
    ));
    // Plaintext would put the token on the wire in clear.
    assert!(!crate::transport::slack::is_slack_file_url(
        "http://files.slack.com/f/F0123"
    ));
}

// ---------------------------------------------------------------------------
// Discord Gateway
// ---------------------------------------------------------------------------

const DISCORD_BOT: &str = "111111111111111111";
const DISCORD_USER: &str = "999999999999999999";
const DISCORD_CHANNEL: &str = "222222222222222222";
const DISCORD_MESSAGE: &str = "333333333333333333";

/// One loopback Discord Gateway, including the control payload the bot sent after Hello.
struct DiscordSocketMock {
    url: String,
    sent: mpsc::UnboundedReceiver<Value>,
}

/// Serves one Discord Gateway connection and performs the Hello → Identify/Resume handshake.
fn spawn_discord_socket_mock(
    frames: Vec<Value>,
    resume_gateway_url: Option<String>,
) -> DiscordSocketMock {
    spawn_discord_socket_mock_with_heartbeat(frames, resume_gateway_url, 60_000, true)
}

fn spawn_discord_socket_mock_with_heartbeat(
    frames: Vec<Value>,
    resume_gateway_url: Option<String>,
    heartbeat_interval_ms: u64,
    acknowledge_heartbeats: bool,
) -> DiscordSocketMock {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("Discord socket mock binds");
    let address = listener.local_addr().expect("Discord socket mock address");
    listener
        .set_nonblocking(true)
        .expect("Discord socket mock is pollable");
    let listener = tokio::net::TcpListener::from_std(listener).expect("Discord socket mock adopts");
    let url = format!("ws://{address}");
    let ready_resume_url = resume_gateway_url.unwrap_or_else(|| url.clone());
    let (sent, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message;
        if socket
            .send(Message::text(
                json!({"op": 10, "d": {"heartbeat_interval": heartbeat_interval_ms}}).to_string(),
            ))
            .await
            .is_err()
        {
            return;
        }
        let Some(Ok(Message::Text(handshake))) = socket.next().await else {
            return;
        };
        let Ok(handshake) = serde_json::from_str::<Value>(&handshake) else {
            return;
        };
        let _ = sent.send(handshake.clone());
        let established = if handshake["op"] == 6 {
            json!({"op": 0, "s": 2, "t": "RESUMED", "d": {}})
        } else {
            json!({
                "op": 0,
                "s": 1,
                "t": "READY",
                "d": {
                    "session_id": "discord-session-1",
                    "resume_gateway_url": ready_resume_url,
                    "user": {"id": DISCORD_BOT, "username": "dekopon"}
                }
            })
        };
        if socket
            .send(Message::text(established.to_string()))
            .await
            .is_err()
        {
            return;
        }
        for frame in frames {
            if socket.send(Message::text(frame.to_string())).await.is_err() {
                return;
            }
        }
        while let Some(Ok(message)) = socket.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let _ = sent.send(payload.clone());
            if acknowledge_heartbeats && payload["op"] == 1 {
                let _ = socket
                    .send(Message::text(json!({"op": 11, "d": null}).to_string()))
                    .await;
            }
        }
    });
    DiscordSocketMock {
        url,
        sent: receiver,
    }
}

fn discord_dispatch(sequence: u64, event: &str, data: Value) -> Value {
    json!({"op": 0, "s": sequence, "t": event, "d": data})
}

fn discord_message(
    id: &str,
    channel: &str,
    guild: Option<&str>,
    author: &str,
    bot: bool,
    content: &str,
) -> Value {
    json!({
        "id": id,
        "channel_id": channel,
        "guild_id": guild,
        "author": {"id": author, "bot": bot},
        "content": content,
        "mentions": [],
        "attachments": [],
        "type": 0
    })
}

fn discord_handler(gateway_url: String) -> impl Fn(&str, &str) -> Value + Send + Sync + 'static {
    move |path, _body| match path {
        "/api/v10/gateway/bot" => json!({
            "url": gateway_url,
            "shards": 1,
            "session_start_limit": {
                "total": 1000,
                "remaining": 999,
                "reset_after": 60_000,
                "max_concurrency": 1
            }
        }),
        path if path.starts_with("/api/v10/channels/") => json!({"id": "444444444444444444"}),
        _ => json!({"code": 10002, "message": "Unknown Application"}),
    }
}

fn discord(endpoint: &str) -> crate::transport::discord::DiscordTransport {
    crate::transport::discord::DiscordTransport::new(
        "community-discord".to_owned(),
        endpoint.to_owned(),
        "discord-test-bot-token".to_owned(),
        ActivityMode::Off,
    )
    .expect("Discord transport builds")
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_routes_photos_and_files_and_posts_a_no_ping_reply() {
    let assets = spawn_raw_http_mock(|_path| {
        (
            200,
            "application/octet-stream",
            b"attachment bytes".to_vec(),
        )
    });
    let mut event = discord_message(
        DISCORD_MESSAGE,
        DISCORD_CHANNEL,
        Some("777777777777777777"),
        DISCORD_USER,
        false,
        "please inspect both attachments",
    );
    event["mentions"] = json!([{"id": DISCORD_BOT, "username": "dekopon"}]);
    event["attachments"] = json!([
        {
            "id": "444444444444444444",
            "filename": "screenshot.png",
            "content_type": "image/png",
            "size": 2048,
            "url": format!("{}/attachments/photo", assets.base)
        },
        {
            "id": "555555555555555555",
            "filename": "spec.pdf",
            "content_type": "Application/PDF; charset=binary",
            "size": 4096,
            "url": format!("{}/attachments/document", assets.base)
        }
    ]);
    let mut socket =
        spawn_discord_socket_mock(vec![discord_dispatch(2, "MESSAGE_CREATE", event)], None);
    let http = spawn_http_mock(discord_handler(socket.url.clone()));
    let mut transport = discord(&http.base);
    let identity = transport
        .connect()
        .await
        .expect("Discord transport connects");
    assert_eq!(identity.user_id.as_deref(), Some(DISCORD_BOT));

    let identify = tokio::time::timeout(Duration::from_secs(5), socket.sent.recv())
        .await
        .expect("Identify arrives")
        .expect("Gateway recorded Identify");
    assert_eq!(identify["op"], 2);
    assert_eq!(identify["d"]["intents"], 4_608);
    assert_eq!(
        identify["d"]["intents"].as_u64().unwrap_or_default() & (1 << 15),
        0
    );

    let message = next_message(&mut transport).await;
    assert_eq!(
        message.subject.canonical(),
        format!("discord.{DISCORD_USER}")
    );
    assert_eq!(
        message.addressed,
        Some(true),
        "the structured mention is the wakeup"
    );
    assert_eq!(message.assets.len(), 2);
    assert_eq!(message.assets[0].name, "screenshot.png");
    assert_eq!(message.assets[0].mime, "image/png");
    assert_eq!(message.assets[1].name, "spec.pdf");
    assert_eq!(message.assets[1].mime, "application/pdf");

    // Both an image and a document follow the same bounded lazy fetch path Slack uses. Discord CDN
    // downloads carry no bot Authorization header; only an expired URL refresh returns to REST.
    let fetcher = transport
        .asset_fetcher()
        .expect("Discord messages can carry assets");
    for asset in &message.assets {
        let bytes = fetcher
            .fetch(
                asset.source.as_ref().expect("attachment has a source"),
                8 * 1024,
            )
            .await
            .expect("attachment downloads within the bound");
        assert!(!bytes.is_empty());
    }
    assert_eq!(
        assets.calls().len(),
        2,
        "the image and file were both fetched"
    );
    assert!(
        assets
            .calls()
            .iter()
            .all(|(_, headers)| !headers.to_ascii_lowercase().contains("authorization:")),
        "Discord CDN requests must never carry the bot token"
    );

    transport
        .replier()
        .reply(message.reply, "@everyone **done**".to_owned())
        .await
        .expect("Discord answer posts");
    let posted = http
        .calls()
        .into_iter()
        .find(|(path, _)| path == "/api/v10/channels/222222222222222222/messages")
        .expect("Create Message was called");
    let body = serde_json::from_str::<Value>(&posted.1).expect("reply body is JSON");
    assert_eq!(body["content"], "@everyone **done**");
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    assert_eq!(body["allowed_mentions"]["replied_user"], false);
    assert_eq!(body["message_reference"]["message_id"], DISCORD_MESSAGE);
    assert_eq!(body["message_reference"]["fail_if_not_exists"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_native_activity_triggers_typing_on_the_authenticated_channel() {
    let event = discord_message(
        "300000000000000099",
        "200000000000000099",
        None,
        DISCORD_USER,
        false,
        "please wait",
    );
    let socket =
        spawn_discord_socket_mock(vec![discord_dispatch(2, "MESSAGE_CREATE", event)], None);
    let http = spawn_http_mock(discord_handler(socket.url.clone()));
    let mut transport = crate::transport::discord::DiscordTransport::new(
        "community-discord".to_owned(),
        http.base.clone(),
        "discord-test-bot-token".to_owned(),
        ActivityMode::Native,
    )
    .expect("Discord transport builds");
    transport.connect().await.expect("Discord connects");
    let message = next_message(&mut transport).await;
    assert_eq!(
        message.activity.as_ref(),
        Some(&ActivityTarget::Discord {
            channel_id: "200000000000000099".to_owned(),
        })
    );

    transport
        .activity()
        .expect("native activity configured")
        .show(message.activity.expect("activity target"))
        .await
        .expect("typing request succeeds");
    assert!(http.calls().iter().any(|(path, body)| {
        path == "/api/v10/channels/200000000000000099/typing" && body.is_empty()
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_obeys_one_rest_retry_after_before_posting_the_reply() {
    let socket = spawn_discord_socket_mock(Vec::new(), None);
    let gateway_url = socket.url.clone();
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let http = spawn_raw_http_mock(move |path| match path {
        "/api/v10/gateway/bot" => (
            200,
            "application/json",
            serde_json::to_vec(&json!({
                "url": gateway_url,
                "shards": 1,
                "session_start_limit": {
                    "total": 1000,
                    "remaining": 999,
                    "reset_after": 60_000,
                    "max_concurrency": 1
                }
            }))
            .expect("Gateway response serializes"),
        ),
        "/api/v10/channels/222222222222222222/messages" => {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                (
                    429,
                    "application/json",
                    br#"{"retry_after":0.001,"global":false}"#.to_vec(),
                )
            } else {
                (200, "application/json", b"{}".to_vec())
            }
        }
        _ => (404, "application/json", b"{}".to_vec()),
    });
    let mut transport = discord(&http.base);
    transport.connect().await.expect("Discord connects");

    transport
        .replier()
        .reply(
            ReplyTarget::Discord {
                channel_id: DISCORD_CHANNEL.to_owned(),
                reply_to: None,
            },
            "after a short rate limit".to_owned(),
        )
        .await
        .expect("the bounded retry succeeds");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_refreshes_an_expired_signed_attachment_url_before_fetching_the_file() {
    let cdn = spawn_raw_http_mock(|path| match path {
        "/fresh/document" => (200, "application/pdf", b"fresh pdf bytes".to_vec()),
        _ => (404, "application/json", br#"{"code":404}"#.to_vec()),
    });
    let channel_id = "200000000000000006";
    let message_id = "300000000000000006";
    let attachment_id = "400000000000000006";
    let mut event = discord_message(
        message_id,
        channel_id,
        None,
        DISCORD_USER,
        false,
        "read this later",
    );
    event["attachments"] = json!([{
        "id": attachment_id,
        "filename": "retained.pdf",
        "content_type": "application/pdf",
        "size": 15,
        "url": format!("{}/expired/document", cdn.base)
    }]);
    let socket =
        spawn_discord_socket_mock(vec![discord_dispatch(2, "MESSAGE_CREATE", event)], None);
    let gateway_url = socket.url.clone();
    let fresh_url = format!("{}/fresh/document", cdn.base);
    let http = spawn_http_mock(move |path, _body| match path {
        "/api/v10/gateway/bot" => json!({
            "url": gateway_url,
            "shards": 1,
            "session_start_limit": {
                "total": 1000,
                "remaining": 999,
                "reset_after": 60_000,
                "max_concurrency": 1
            }
        }),
        "/api/v10/channels/200000000000000006/messages/300000000000000006" => json!({
            "id": message_id,
            "attachments": [{"id": attachment_id, "url": fresh_url}]
        }),
        _ => json!({"code": 10008, "message": "Unknown Message"}),
    });
    let mut transport = discord(&http.base);
    transport.connect().await.expect("Discord connects");
    let message = next_message(&mut transport).await;
    let source = message.assets[0]
        .source
        .as_ref()
        .expect("attachment has a source");

    let bytes = transport
        .asset_fetcher()
        .expect("Discord has an asset fetcher")
        .fetch(source, 1024)
        .await
        .expect("an expired URL is refreshed from the source message");
    assert_eq!(bytes, b"fresh pdf bytes");
    assert_eq!(
        cdn.calls()
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        vec!["/expired/document", "/fresh/document"]
    );
    assert!(http.calls().iter().any(|(path, _)| {
        path == "/api/v10/channels/200000000000000006/messages/300000000000000006"
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_drops_bots_webhooks_and_system_messages_before_routing_a_dm() {
    let bot = discord_message(
        "300000000000000001",
        DISCORD_CHANNEL,
        Some("777777777777777777"),
        "888888888888888888",
        true,
        "another bot",
    );
    let mut webhook = discord_message(
        "300000000000000002",
        DISCORD_CHANNEL,
        Some("777777777777777777"),
        DISCORD_USER,
        false,
        "a webhook",
    );
    webhook["webhook_id"] = json!("666666666666666666");
    let mut system = discord_message(
        "300000000000000003",
        DISCORD_CHANNEL,
        Some("777777777777777777"),
        DISCORD_USER,
        false,
        "joined",
    );
    system["type"] = json!(7);
    let direct = discord_message(
        "300000000000000004",
        "200000000000000004",
        None,
        DISCORD_USER,
        false,
        "a private question",
    );
    let socket = spawn_discord_socket_mock(
        vec![
            discord_dispatch(2, "MESSAGE_CREATE", bot),
            discord_dispatch(3, "MESSAGE_CREATE", webhook),
            discord_dispatch(4, "MESSAGE_CREATE", system),
            discord_dispatch(5, "MESSAGE_CREATE", direct),
        ],
        None,
    );
    let http = spawn_http_mock(discord_handler(socket.url.clone()));
    let mut transport = discord(&http.base);
    transport.connect().await.expect("Discord connects");

    let message = next_message(&mut transport).await;
    assert_eq!(message.text, "a private question");
    assert_eq!(message.conversation, ConversationKind::DirectMessage);
    assert_eq!(
        message.addressed,
        Some(true),
        "a direct message is addressed by definition"
    );
    assert_eq!(message.conversation_id, "200000000000000004");
    assert_eq!(
        message.reply,
        ReplyTarget::Discord {
            channel_id: "200000000000000004".to_owned(),
            reply_to: None,
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_reconnects_when_a_heartbeat_is_not_acknowledged() {
    let after_reconnect = discord_message(
        "300000000000000008",
        "200000000000000008",
        None,
        DISCORD_USER,
        false,
        "the heartbeat watchdog recovered",
    );
    let mut second = spawn_discord_socket_mock(
        vec![discord_dispatch(3, "MESSAGE_CREATE", after_reconnect)],
        None,
    );
    let mut first =
        spawn_discord_socket_mock_with_heartbeat(Vec::new(), Some(second.url.clone()), 20, false);
    let http = spawn_http_mock(discord_handler(first.url.clone()));
    let mut transport = discord(&http.base);
    transport.connect().await.expect("Discord connects");

    let message = expect_message(
        tokio::time::timeout(Duration::from_secs(10), transport.next())
            .await
            .expect("the heartbeat watchdog reconnects")
            .expect("a message arrives on the resumed socket"),
    );
    assert_eq!(message.text, "the heartbeat watchdog recovered");

    let mut first_ops = Vec::new();
    while let Ok(payload) = first.sent.try_recv() {
        first_ops.push(payload["op"].as_u64());
    }
    assert!(
        first_ops.contains(&Some(1)),
        "a heartbeat was sent: {first_ops:?}"
    );
    let resume = tokio::time::timeout(Duration::from_secs(5), second.sent.recv())
        .await
        .expect("Resume arrives")
        .expect("the second Gateway recorded Resume");
    assert_eq!(resume["op"], 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_routes_a_redelivered_message_only_once() {
    let event = discord_message(
        "300000000000000007",
        "200000000000000007",
        None,
        DISCORD_USER,
        false,
        "only once",
    );
    let socket = spawn_discord_socket_mock(
        vec![
            discord_dispatch(2, "MESSAGE_CREATE", event.clone()),
            discord_dispatch(3, "MESSAGE_CREATE", event),
        ],
        None,
    );
    let http = spawn_http_mock(discord_handler(socket.url.clone()));
    let mut transport = discord(&http.base);
    transport.connect().await.expect("Discord connects");

    assert_eq!(next_message(&mut transport).await.text, "only once");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), transport.next())
            .await
            .is_err(),
        "a resume redelivery must not create a second session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_reconnects_with_resume_before_delivering_more_messages() {
    let resumed_message = discord_message(
        "300000000000000005",
        "200000000000000005",
        None,
        DISCORD_USER,
        false,
        "after resume",
    );
    let mut second = spawn_discord_socket_mock(
        vec![discord_dispatch(3, "MESSAGE_CREATE", resumed_message)],
        None,
    );
    let first =
        spawn_discord_socket_mock(vec![json!({"op": 7, "d": null})], Some(second.url.clone()));
    let http = spawn_http_mock(discord_handler(first.url.clone()));
    let mut transport = discord(&http.base);
    transport.connect().await.expect("Discord connects");

    let message = expect_message(
        tokio::time::timeout(Duration::from_secs(10), transport.next())
            .await
            .expect("the transport resumes before the test gives up")
            .expect("a message arrives after resume"),
    );
    assert_eq!(message.text, "after resume");

    let resume = tokio::time::timeout(Duration::from_secs(5), second.sent.recv())
        .await
        .expect("Resume arrives")
        .expect("Gateway recorded Resume");
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["session_id"], "discord-session-1");
    assert_eq!(resume["d"]["seq"], 1);
}

// ---------------------------------------------------------------------------
// Telegram long polling
// ---------------------------------------------------------------------------

fn telegram_message(user: i64, is_bot: bool, message_id: i64, text: &str) -> Value {
    telegram_chat_message(42, "private", user, is_bot, message_id, text)
}

/// The same message in a named chat, so a test can tell two conversations apart.
fn telegram_chat_message(
    chat: i64,
    kind: &str,
    user: i64,
    is_bot: bool,
    message_id: i64,
    text: &str,
) -> Value {
    json!({
        "message_id": message_id,
        "from": {"id": user, "is_bot": is_bot, "username": "someone"},
        "chat": {"id": chat, "type": kind},
        "text": text
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_acknowledges_by_advancing_its_offset() {
    // There is no ack call: the next poll's offset is the acknowledgment, and it has to advance
    // past updates the daemon chose not to route or the same bot message returns forever.
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("offset=0") {
            return json!({"ok": true, "result": [
                {"update_id": 100, "message": telegram_message(7, true, 1, "a bot said this")},
                {"update_id": 101, "message": telegram_message(16034700182_i64, false, 2, "a person asked this")}
            ]});
        }
        json!({"ok": true, "result": []})
    });

    let mut transport = crate::transport::telegram::TelegramTransport::new(
        "tg".to_owned(),
        http.base.clone(),
        "12345:test-token".to_owned(),
        ActivityMode::Off,
    )
    .expect("telegram transport builds");
    let identity = transport
        .connect()
        .await
        .expect("telegram transport connects");
    assert_eq!(identity.handle.as_deref(), Some("dekopon_bot"));

    let message = expect_message(
        tokio::time::timeout(Duration::from_secs(5), transport.next())
            .await
            .expect("one update routes")
            .expect("a routable message"),
    );
    assert_eq!(message.text, "a person asked this");
    assert_eq!(message.subject.canonical(), "telegram.16034700182");

    // The next poll must ask past both updates, including the bot message that was filtered.
    assert!(
        tokio::time::timeout(Duration::from_millis(400), transport.next())
            .await
            .is_err(),
        "an empty poll produces no message"
    );
    assert!(
        http.calls()
            .iter()
            .any(|(path, _)| path.contains("offset=102")),
        "{:?}",
        http.calls()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_photo_is_routed_with_its_largest_size() {
    // A photo arrives as the same image at several sizes, smallest first, and its words live in
    // `caption` rather than `text`. Reading only `text` made the whole message invisible; taking
    // the first size would hand a model a thumbnail it cannot read.
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("offset=0") {
            return json!({"ok": true, "result": [{
                "update_id": 300,
                "message": {
                    "message_id": 9,
                    "from": {"id": 16034700182_i64, "is_bot": false},
                    "chat": {"id": 4242, "type": "private"},
                    "caption": "what does this say?",
                    "photo": [
                        {"file_id": "thumb", "file_size": 900},
                        {"file_id": "full", "file_size": 214_000}
                    ]
                }
            }]});
        }
        json!({"ok": true, "result": []})
    });

    let mut transport = crate::transport::telegram::TelegramTransport::new(
        "tg".to_owned(),
        http.base.clone(),
        "12345:test-token".to_owned(),
        ActivityMode::Off,
    )
    .expect("telegram transport builds");
    transport.connect().await.expect("telegram connects");

    let message = next_message(&mut transport).await;
    assert_eq!(message.text, "what does this say?");
    assert_eq!(
        message.assets,
        vec![PendingAsset {
            name: "photo.jpg".to_owned(),
            mime: "image/jpeg".to_owned(),
            size: 214_000,
            source: Some(AssetSourceRef::Telegram {
                file_id: "full".to_owned(),
            }),
        }]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_document_keeps_its_own_name_and_media_type() {
    // Unlike a photo, a document is passed through rather than re-encoded, so Telegram reports
    // both and neither has to be inferred.
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("offset=0") {
            return json!({"ok": true, "result": [{
                "update_id": 301,
                "message": {
                    "message_id": 10,
                    "from": {"id": 16034700182_i64, "is_bot": false},
                    "chat": {"id": 4242, "type": "private"},
                    "document": {
                        "file_id": "doc-1",
                        "file_name": "spec.pdf",
                        "mime_type": "application/pdf",
                        "file_size": 5000
                    }
                }
            }]});
        }
        json!({"ok": true, "result": []})
    });

    let mut transport = crate::transport::telegram::TelegramTransport::new(
        "tg".to_owned(),
        http.base.clone(),
        "12345:test-token".to_owned(),
        ActivityMode::Off,
    )
    .expect("telegram transport builds");
    transport.connect().await.expect("telegram connects");

    // No caption: the attachment is the whole message, and dropping it would be silence.
    let message = next_message(&mut transport).await;
    assert!(message.text.is_empty());
    assert_eq!(message.assets[0].name, "spec.pdf");
    assert_eq!(message.assets[0].mime, "application/pdf");
}

#[test]
fn a_document_does_not_need_the_image_modality() {
    // Gating a PDF on the vision modality would refuse it to a model perfectly able to read one.
    // Only images need it.
    let store = asset_store();
    let registered = store.assets_for(
        "c1",
        vec![
            PendingAsset {
                name: "spec.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                size: 5000,
                source: Some(AssetSourceRef::Telegram {
                    file_id: "doc-1".to_owned(),
                }),
            },
            pending("shot.png", "image/png", 2048),
        ],
        false,
        Instant::now(),
    );
    let note = asset::reference_note(&registered, false).expect("a note");

    assert!(registered.fetchable, "the document is still fetchable");
    assert!(note.contains("Chat Asset #1 — spec.pdf"), "{note}");
    assert!(!note.contains("Chat Asset #2"), "{note}");
    assert!(note.contains("cannot be shown images"), "{note}");
}

#[test]
fn an_unsupported_media_type_is_named_but_never_numbered() {
    // A chat service will deliver anything. The allowlist is the narrow end of the intersection
    // with what a model actually accepts.
    let store = asset_store();
    let registered = store.assets_for(
        "c1",
        vec![pending("clip.mov", "video/quicktime", 700 * 1024 * 1024)],
        true,
        Instant::now(),
    );
    let note = asset::reference_note(&registered, true).expect("a note");

    assert!(!registered.fetchable);
    assert!(note.contains("clip.mov"), "{note}");
    assert!(!note.contains("Chat Asset #"), "{note}");
    assert!(!note.contains("fetch_chat_asset"), "{note}");
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_activity_and_replies_stay_inside_the_inbound_topic() {
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("offset=0") {
            return json!({"ok": true, "result": [{
                "update_id": 350,
                "message": {
                    "message_id": 11,
                    "message_thread_id": 99,
                    "from": {"id": 16034700182_i64, "is_bot": false},
                    "chat": {"id": -1001, "type": "supergroup"},
                    "text": "topic work"
                }
            }]});
        }
        if path.contains("sendChatAction") || path.contains("sendMessage") {
            return json!({"ok": true, "result": true});
        }
        json!({"ok": true, "result": []})
    });
    let mut transport = crate::transport::telegram::TelegramTransport::new(
        "tg".to_owned(),
        http.base.clone(),
        "12345:test-token".to_owned(),
        ActivityMode::Native,
    )
    .expect("Telegram transport builds");
    transport.connect().await.expect("Telegram connects");
    let message = next_message(&mut transport).await;

    assert_eq!(message.thread.as_deref(), Some("99"));
    assert_eq!(message.conversation_id, "-1001:99");
    assert_eq!(
        message.reply.clone(),
        ReplyTarget::Telegram {
            chat_id: -1001,
            reply_to: Some(11),
            message_thread_id: Some(99),
        }
    );
    let target = message.activity.clone().expect("topic activity target");
    transport
        .activity()
        .expect("native activity configured")
        .show(target)
        .await
        .expect("chat action succeeds");
    transport
        .replier()
        .reply(message.reply, "done".to_owned())
        .await
        .expect("topic reply succeeds");

    let calls = http.calls();
    let action = calls
        .iter()
        .find(|(path, _)| path.contains("sendChatAction"))
        .expect("typing action was sent");
    let action = serde_json::from_str::<Value>(&action.1).expect("action body is JSON");
    assert_eq!(action["action"], "typing");
    assert_eq!(action["chat_id"], -1001);
    assert_eq!(action["message_thread_id"], 99);
    let reply = calls
        .iter()
        .find(|(path, _)| path.contains("sendMessage"))
        .expect("reply was sent");
    let reply = serde_json::from_str::<Value>(&reply.1).expect("reply body is JSON");
    assert_eq!(reply["message_thread_id"], 99);
    assert_eq!(reply["reply_to_message_id"], 11);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_chat_is_one_conversation_and_another_chat_is_another() {
    // The Bot API puts no thread identifier on a plain message, so a conversation collapses to its
    // chat: consecutive messages continue one exchange, and a group is not the private chat.
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("offset=0") {
            return json!({"ok": true, "result": [
                {"update_id": 200, "message": telegram_message(16034700182_i64, false, 1, "first")},
                {"update_id": 201, "message": telegram_message(16034700182_i64, false, 2, "second")},
                {"update_id": 202, "message": telegram_chat_message(-1001, "supergroup", 16034700182_i64, false, 3, "over here")}
            ]});
        }
        json!({"ok": true, "result": []})
    });

    let mut transport = crate::transport::telegram::TelegramTransport::new(
        "tg".to_owned(),
        http.base.clone(),
        "12345:test-token".to_owned(),
        ActivityMode::Off,
    )
    .expect("telegram transport builds");
    transport
        .connect()
        .await
        .expect("telegram transport connects");

    let first = next_message(&mut transport).await;
    let second = next_message(&mut transport).await;
    let group = next_message(&mut transport).await;

    assert_eq!(first.conversation_id, "42");
    assert_eq!(
        first.conversation_id, second.conversation_id,
        "two messages in one chat are one conversation"
    );
    assert_eq!(group.conversation_id, "-1001");
    assert_ne!(first.conversation_id, group.conversation_id);
}

// ---------------------------------------------------------------------------
// The development transport
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_local_transport_takes_its_conversation_from_the_caller() {
    // Nothing here is service-native, so the caller names its own conversation and `dev` is the
    // default. Deliberately not the connection number: a developer who reconnects is still in the
    // same conversation, and one client driving several sessions needs to keep them apart.
    let directory = temporary();
    let socket_path = directory.path().join("dev.sock");
    let mut transport =
        crate::transport::local::LocalTransport::new("dev".to_owned(), socket_path.clone());
    transport
        .connect()
        .await
        .expect("the development transport binds");

    use tokio::io::AsyncWriteExt as _;
    let mut client = tokio::net::UnixStream::connect(&socket_path)
        .await
        .expect("a local caller connects");
    for request in [
        json!({"subject": SUBJECT, "text": "first"}),
        json!({"subject": SUBJECT, "text": "second"}),
        json!({"subject": SUBJECT, "channel": "session-7", "text": "over here"}),
    ] {
        client
            .write_all(format!("{request}\n").as_bytes())
            .await
            .expect("the request is written");
    }

    let first = next_message(&mut transport).await;
    let second = next_message(&mut transport).await;
    let named = next_message(&mut transport).await;

    assert_eq!(first.text, "first");
    assert_eq!(first.conversation_id, "dev");
    assert_eq!(
        first.conversation_id, second.conversation_id,
        "two requests on one connection continue one conversation"
    );
    assert_ne!(
        first.message_id, second.message_id,
        "each request is still its own message"
    );
    assert_eq!(named.conversation_id, "session-7");
}
