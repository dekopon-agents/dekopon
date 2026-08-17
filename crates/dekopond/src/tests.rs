use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{ConversationTurn, HistoryLimits, PromptLimits};
use dekopon_broker_protocol::{
    AvailableCapability, BrokerRequest, FrameLimits, RequestEnvelope, ResponseEnvelope, read_frame,
    write_frame,
};
use dekopon_config::LocalCatalog;
use dekopon_core::ExternalSubject;
use dekopon_model::model::{
    AssistantTurn, ChatModel, CompletionOptions, ModelError, ModelMessage, ModelTool,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::{net::UnixListener, sync::mpsc};

use crate::{
    cache_key,
    config::{
        self, ConversationPolicy, ConversationWindow, ModelConfig, ResolvedBroker, RouteMatch,
        SocketDiscovery,
    },
    conversation::{ConversationKey, ConversationStore, EvictionReason},
    routes::{RouteError, RoutingTable},
    session::{
        BUSY_REPLY, FAILURE_REPLY, ModelFactory, SessionError, SessionGate, SessionRunner,
        UNAUTHORIZED_REPLY, run_session,
    },
    transport::{
        ChatReplier, ChatTransport, ConversationKind, InboundMessage, MAX_INBOUND_TEXT_BYTES,
        MAX_OUTBOUND_TEXT_BYTES, ReplyTarget, TransportError, TransportIdentity, bound_inbound,
        bound_outbound,
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

#[test]
fn a_shared_channel_message_counts_as_addressed_only_when_it_names_the_bot() {
    let slack = TransportIdentity {
        user_id: Some("U0BOTBOT".to_owned()),
        handle: None,
    };
    assert!(slack.is_addressed("hey <@U0BOTBOT> please look at this"));
    assert!(!slack.is_addressed("hey everyone, U0BOTBOT is the bot"));

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
            cache_keys: Mutex::new(Vec::new()),
            requests: AtomicUsize::new(0),
            forbidden: true,
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
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
        _tools: &[ModelTool],
        options: &CompletionOptions,
    ) -> Result<AssistantTurn, ModelError> {
        assert!(!self.0.forbidden, "this session must never reach a model");
        self.0
            .prompts
            .lock()
            .expect("recorded prompts")
            .push(messages.to_vec());
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
        r#match: RouteMatch::DirectMessage,
        agent: "reviewer".parse().expect("valid agent fixture"),
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
        conversation: ConversationKind::DirectMessage,
        reply: ReplyTarget::Local { connection: 1 },
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
    answer: String,
}

impl BlockedModel {
    fn new(answer: &str) -> Arc<Self> {
        let (entered, entered_signal) = std::sync::mpsc::channel();
        let (release, release_signal) = std::sync::mpsc::channel();
        Arc::new(Self {
            entered: Mutex::new(Some(entered)),
            entered_signal: tokio::sync::Mutex::new(entered_signal),
            release: Mutex::new(Some(release)),
            release_signal: Mutex::new(Some(release_signal)),
            answer: answer.to_owned(),
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
        Ok(answer(&self.0.answer))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorized_message_reaches_its_agent_and_answers_in_chat() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(vec![capability(
            "echo.echo",
        )])],
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
async fn an_unauthorized_subject_is_refused_before_any_model_call() {
    // The cheapest possible refusal, and the one that cannot be argued with: the broker already
    // said this subject reaches nothing through this agent, so there is no question to ask a model.
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(Vec::new())],
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
        vec![ResponseEnvelope::capabilities(vec![capability(
            "echo.echo",
        )])],
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
        vec![ResponseEnvelope::capabilities(vec![capability(
            "echo.echo",
        )])],
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
            ResponseEnvelope::capabilities(vec![capability("echo.echo"), capability("gh.pr_view")]),
            ResponseEnvelope::capabilities(vec![capability("echo.echo")]),
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
            ResponseEnvelope::capabilities(vec![capability("echo.echo")]),
            ResponseEnvelope::capabilities(Vec::new()),
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

    fn next(&mut self) -> BoxFuture<'_, Result<InboundMessage, TransportError>> {
        Box::pin(async move { self.inbound.recv().await.ok_or(TransportError::Closed) })
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
    let reader = tokio::spawn(crate::read_transport(Box::new(transport), routed));

    sender
        .send(message("first"))
        .expect("fixture accepts a message");
    assert_eq!(
        received.recv().await.expect("the reader forwards it").text,
        "first"
    );

    drop(sender);
    reader.await.expect("the reader ends with its transport");
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

// ---------------------------------------------------------------------------
// Slack Socket Mode
// ---------------------------------------------------------------------------

/// The next routable message from any transport, failing the test rather than hanging on it.
async fn next_message(transport: &mut dyn ChatTransport) -> InboundMessage {
    tokio::time::timeout(Duration::from_secs(5), transport.next())
        .await
        .expect("a message arrives before the test gives up")
        .expect("a routable message")
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

/// Reads one complete HTTP request, returning its path (with query) and body.
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
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
    Some((path, body))
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
    crate::transport::slack::SlackTransport::new(
        "scientist-slack".to_owned(),
        endpoint.to_owned(),
        "xapp-test-app-token".to_owned(),
        "xoxb-test-bot-token".to_owned(),
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
        vec![ResponseEnvelope::capabilities(vec![capability(
            "echo.echo",
        )])],
    )
    .await;
    let model = BlockedModel::new("All good.");
    let replier = transport.replier();
    let message = transport
        .next()
        .await
        .expect("one routable message arrives");

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

    let first = transport.next().await.expect("the first delivery routes");
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

    let message = tokio::time::timeout(Duration::from_secs(10), transport.next())
        .await
        .expect("the transport reconnects on its own")
        .expect("a message arrives on the second socket");
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

    let message = tokio::time::timeout(Duration::from_secs(5), transport.next())
        .await
        .expect("the third envelope routes")
        .expect("a routable message");
    assert_eq!(message.text, "a real question");
    assert_eq!(message.subject.canonical(), "slack.t0123abc.u9xyz");
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
    )
    .expect("telegram transport builds");
    let identity = transport
        .connect()
        .await
        .expect("telegram transport connects");
    assert_eq!(identity.handle.as_deref(), Some("dekopon_bot"));

    let message = tokio::time::timeout(Duration::from_secs(5), transport.next())
        .await
        .expect("one update routes")
        .expect("a routable message");
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
