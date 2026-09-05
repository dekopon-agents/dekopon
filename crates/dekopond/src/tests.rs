use dekopon_harness::conversation::{BoundedConversationStore, ConversationKey, EvictionReason};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_broker_protocol::{
    Attestation, AvailableCapability, BrokerRequest, BrokerSocketDiscovery, ChatMemorySurface,
    CommandRunOutcome, FrameLimits, InvocationOutcome, InvocationResult, RequestEnvelope,
    ResponseEnvelope, read_frame, write_frame,
};
use dekopon_config::LocalCatalog;
use dekopon_core::{ExternalSubject, SecretDrn, SecretUseProposal};
use dekopon_harness::{
    history::{HistoryLimits, JobRecord},
    improvement::IMPROVEMENT_TOOL_NAME,
    session::PromptLimits,
    skills::SKILL_TOOL_NAME,
    tools::{
        AGENT_CONFIG_TOOL_NAME, AssetSource as _, DECLINE_REPLY_TOOL_NAME,
        IMAGE_GENERATION_TOOL_NAME,
    },
};
use dekopon_model::{
    image::{GeneratedImage, ImageGenerationError, ImageGenerator},
    model::{
        AssistantTurn, ChatModel, CompletionOptions, ModelError, ModelFunctionCall, ModelMessage,
        ModelTool, ModelToolCall,
    },
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::{net::UnixListener, sync::mpsc};

use crate::{
    agent_inventory,
    asset::{self, AssetSourceRef, AssetStore, PendingAsset, SessionAssets},
    cache_key,
    config::{
        self, ActivityMode, ConfigError, ConfigProblem, ConversationPolicy, ConversationWindow,
        ImageGeneratorConfig, ModelConfig, NativeActivityConfig, ResolvedBroker, RouteMatch,
        SlackActivityConfig, SlackActivityFallback, SlackExperience,
    },
    routes::{RouteError, RouteProblem, RoutingTable},
    session::{
        BUSY_REPLY, CancelAwareInvoker, FAILURE_REPLY, ImageGeneratorStartupError, ModelCache,
        ModelFactory, SessionCancellation, SessionError, SessionGate, SessionRunner, SharedModel,
        UNAUTHORIZED_REPLY, UNREPORTED_WORK_REPLY, configured_image_generator, image_credential,
        memory_record_outcome_category, model_bearer_token, model_credential, run_session,
    },
    transport::{
        ActivityTarget, ChatActivity, ChatReplier, ChatTransport, ConversationKind,
        DeliveryReceipt, InboundMessage, MAX_INBOUND_TEXT_BYTES, MAX_OUTBOUND_TEXT_BYTES,
        OutboundReply, ReplyTarget, ThreadClaim, ThreadContinuation, ThreadOwnership,
        TransportError, TransportEvent, TransportIdentity, bound_inbound, bound_outbound,
        credential_value,
    },
};

const SUBJECT: &str = "tel.16034700182";

fn subject() -> ExternalSubject {
    SUBJECT.parse().expect("canonical subject fixture")
}

fn generated_image() -> GeneratedImage {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(b"kitty pixels");
    GeneratedImage::from_png(png).expect("generated PNG fixture")
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

/// Writes one configuration document where the daemon's own hygiene checks will accept it.
fn write_config(directory: &Path, document: &Value) -> PathBuf {
    let path = directory.join("dekopond.json");
    fs::write(
        &path,
        serde_json::to_vec(document).expect("config serializes"),
    )
    .expect("write config fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure config fixture");
    path
}

async fn load(
    directory: &Path,
    document: &Value,
) -> Result<crate::ResolvedConfig, config::ConfigError> {
    config::load(write_config(directory, document), crate::current_uid()).await
}

/// One refused configuration: what it is called, the document, and which refusal it must be.
///
/// The predicate is the whole point of the tuple. `is_err()` alone passes when a fixture typo trips
/// strict decoding before it ever reaches the check the case is named after, and it keeps passing
/// if the check stops being called at all.
type RefusalCase = (&'static str, Value, fn(&ConfigError) -> bool);

/// Whether one aggregated refusal names this problem among the ones it reports.
///
/// The whole file is scanned before it is refused, so a case asserts that its problem is *in* the
/// report rather than that it is the only thing in it — a fixture with a second mistake would
/// otherwise fail the case it is named after.
fn reports(error: &ConfigError, matcher: fn(&ConfigProblem) -> bool) -> bool {
    matches!(error, ConfigError::Invalid { problems, .. } if problems.iter().any(matcher))
}

/// The one problem a refused route binding reported.
fn only_route_problem(error: &RouteError) -> &RouteProblem {
    assert_eq!(
        error.problems.len(),
        1,
        "one unsatisfiable route, one problem: {:?}",
        error.problems
    );
    &error.problems[0]
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
    assert!(resolved.image_generator.is_none());
    assert!(!resolved.routes[0].image_generator);
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
async fn image_generation_is_configured_once_and_route_opt_in() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["imageGenerator"] = json!({
        "model": "gpt-image-1",
        "apiKeyEnv": "OPENAI_IMAGE_API_KEY",
        "timeoutMs": 120_000
    });
    document["routes"][0]["imageGenerator"] = json!(true);

    let resolved = load(directory.path(), &document)
        .await
        .expect("an explicitly enabled image generator resolves");

    let generator = resolved
        .image_generator
        .as_ref()
        .expect("the gateway configures one generator");
    assert_eq!(generator.model, "gpt-image-1");
    assert_eq!(generator.api_key_env, "OPENAI_IMAGE_API_KEY");
    assert!(resolved.routes[0].image_generator);
    let routes = RoutingTable::bind(&resolved, &catalog(true, Some("reasoning")))
        .expect("the route binds the configured generator");
    assert!(
        routes
            .route("dev", &ConversationKind::DirectMessage)
            .expect("route matches")
            .image_generator
    );
}

#[test]
fn a_missing_image_model_credential_fails_before_chat_starts() {
    let variable = "DEKOPOND_TEST_MISSING_IMAGE_KEY_7C83E9";
    assert!(
        std::env::var_os(variable).is_none(),
        "fixture must stay unset"
    );
    let configured = ImageGeneratorConfig {
        model: "gpt-image-1".to_owned(),
        api_key_env: variable.to_owned(),
        timeout_ms: 120_000,
    };
    let error = match configured_image_generator(Some(&configured), true) {
        Err(error) => error,
        Ok(_) => panic!("a named credential is required at startup"),
    };

    assert!(matches!(
        &error,
        ImageGeneratorStartupError::Credential {
            source: TransportError::MissingCredential { .. },
            ..
        }
    ));
    let diagnostic = error.to_string();
    assert!(diagnostic.contains(variable));

    // Exported but blank is the same refusal, not a generator that starts with a blank key.
    let blank = image_credential(variable, Some(OsString::from("   ")))
        .expect_err("a blank credential is the absence of one presented as presence");
    assert!(matches!(
        &blank,
        ImageGeneratorStartupError::Credential {
            source: TransportError::EmptyCredential { .. },
            ..
        }
    ));

    let unused = configured_image_generator(Some(&configured), false)
        .expect("an unreferenced generator reads no credential");
    assert!(unused.is_none());
}

/// The cancellation boundary must not narrow what a proposal may carry.
///
/// This wrapper forwarded eight of the trait's nine methods and inherited the ninth's
/// deny-by-default, so a `curl --user USER:${drn:...}` in a gateway session was refused inside
/// `dekopond` — the proposal never reached the broker, which is the only thing that can decide
/// `secret.use` at all. Cancellation still applies to it, exactly as it does to a plain call.
#[test]
fn a_secret_use_proposal_reaches_the_broker_leg_through_the_cancellation_boundary() {
    use dekopon_shell::{CapabilityCallResult, CapabilityInvoker};

    /// Records what it was handed, so the assertion is about the wrapper rather than a broker.
    #[derive(Default)]
    struct RecordingLeg {
        secret_uses: Mutex<Vec<Option<SecretUseProposal>>>,
    }

    impl CapabilityInvoker for RecordingLeg {
        fn granted(&self) -> Vec<String> {
            vec!["http-probe.fetch".to_owned()]
        }

        fn invoke(
            &self,
            _capability: &str,
            _input: Value,
            secret_use: Option<SecretUseProposal>,
        ) -> CapabilityCallResult {
            self.secret_uses
                .lock()
                .expect("recorded secret uses")
                .push(secret_use);
            CapabilityCallResult::Succeeded(json!({"status": 200}))
        }
    }

    let proposal = SecretUseProposal::HttpBearer {
        secret: "drn:com.xrl:secret:prod:api/token"
            .parse::<SecretDrn>()
            .expect("canonical DRN"),
    };
    let leg = Arc::new(RecordingLeg::default());
    let cancellation = SessionCancellation::new();
    let invoker = CancelAwareInvoker {
        inner: Arc::clone(&leg),
        cancellation: cancellation.clone(),
    };

    assert_eq!(
        invoker.invoke("http-probe.fetch", json!({}), Some(proposal.clone())),
        CapabilityCallResult::Succeeded(json!({"status": 200}))
    );
    assert_eq!(
        leg.secret_uses.lock().expect("recorded secret uses")[0],
        Some(proposal.clone()),
        "the wrapper dropped the proposal on its way to the leg"
    );

    // A stopped session refuses it like any other call, rather than letting a secret-carrying one
    // through the boundary a plain one cannot cross.
    assert!(cancellation.cancel());
    assert_eq!(
        invoker.invoke("http-probe.fetch", json!({}), Some(proposal)),
        CapabilityCallResult::Denied {
            reason: "session-cancelled".to_owned(),
        }
    );
}

/// Dispatch asks the wrapper, not the leg, and a defaulted method it forgets answers "nothing".
///
/// `command_words` defaults to an empty list and `is_granted` to a scan of `granted`, so a
/// forwarder that omits either turns a session's `gh` command word into "command not found" and a
/// leg's cheaper membership answer into a wrong one. That is the same defect class the secret-use
/// argument was, one method over, and nothing else in this crate pins it.
#[test]
fn the_cancellation_boundary_forwards_the_defaulted_lookups_rather_than_answering_them() {
    use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, CommandRun};

    /// Every answer here is one the trait default cannot give for this `granted` list.
    struct CommandLeg;

    impl CapabilityInvoker for CommandLeg {
        fn granted(&self) -> Vec<String> {
            vec!["echo.echo".to_owned()]
        }

        fn is_granted(&self, capability: &str) -> bool {
            capability == "gh.pr-view"
        }

        fn grants_namespace(&self, namespace: &str) -> bool {
            namespace == "gh"
        }

        fn command_words(&self) -> Vec<String> {
            vec!["gh".to_owned()]
        }

        fn has_command_word(&self, word: &str) -> bool {
            word == "gh"
        }

        fn run_command(
            &self,
            word: &str,
            argv: &[String],
            stdin: Option<&str>,
        ) -> Option<CommandRun> {
            Some(CommandRun::Rendered {
                stdout: format!("{word} {}\n", argv.join(" ")),
                stderr: stdin.unwrap_or_default().to_owned(),
                status: 3,
            })
        }

        fn invoke(
            &self,
            _capability: &str,
            _input: Value,
            _secret_use: Option<SecretUseProposal>,
        ) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    let invoker = CancelAwareInvoker {
        inner: CommandLeg,
        cancellation: SessionCancellation::new(),
    };

    assert_eq!(invoker.command_words(), vec!["gh".to_owned()]);
    assert!(invoker.has_command_word("gh"));
    // Defaults to `None`, which dispatch reports as "command not found".
    assert_eq!(
        invoker.run_command("gh", &["--help".to_owned()], Some("piped")),
        Some(CommandRun::Rendered {
            stdout: "gh --help\n".to_owned(),
            stderr: "piped".to_owned(),
            status: 3,
        })
    );
    // Absent from `granted`, so the default scan would refuse both.
    assert!(invoker.is_granted("gh.pr-view"));
    assert!(invoker.grants_namespace("gh"));
    assert_eq!(invoker.granted(), vec!["echo.echo".to_owned()]);
}

/// `apiKeyEnv` has three meanings and they used to have one outcome.
///
/// Absent means "this endpoint needs no key", which a loopback llama.cpp genuinely does not. Unset
/// and exported-but-blank became "no bearer token" too: the gateway started clean, every answer came
/// back 401, and nothing anywhere named the variable. The cached client made it survive until a
/// restart, so exporting the key afterwards did not help either.
#[test]
fn a_model_api_key_variable_is_absent_or_usable_and_never_silently_empty() {
    let variable = "DEKOPOND_TEST_MODEL_KEY_4F1A62";
    let model = |api_key_env: Option<&str>| ModelConfig::OpenaiCompatible {
        name: "fast".to_owned(),
        endpoint: "http://127.0.0.1:8080/v1/chat/completions".to_owned(),
        model: "qwen3".to_owned(),
        api_key_env: api_key_env.map(ToOwned::to_owned),
        timeout_ms: 60_000,
        classes: vec!["fast".to_owned()],
        modalities: Vec::new(),
        effort: Default::default(),
    };

    // No field at all is a deliberate configuration, not a missing credential.
    assert!(
        model_bearer_token(&model(None))
            .expect("an endpoint that needs no key is not a startup failure")
            .is_none()
    );

    // A set variable is the token, unchanged.
    assert_eq!(
        model_credential("fast", variable, Some(OsString::from("sk-live-1")))
            .expect("a set variable is the token"),
        "sk-live-1"
    );

    // Both failures name the variable and the model, never the value, and keep which problem it
    // was: "export it" and "you exported nothing" are different operator actions.
    for (value, problem) in [
        (None, "is not set"),
        (Some(OsString::from("   ")), "is set to an empty value"),
    ] {
        let error = model_credential("fast", variable, value)
            .expect_err("a model that cannot present its key must not start");
        let rendered = error.to_string();
        assert!(rendered.contains(variable), "{rendered}");
        assert!(rendered.contains("fast"), "{rendered}");
        let cause = std::error::Error::source(&error).expect("the credential problem is the cause");
        assert!(cause.to_string().contains(problem), "{cause}");
    }

    // A named field pointing at an unset variable refuses through the startup entry point too.
    assert!(
        std::env::var_os(variable).is_none(),
        "fixture must stay unset"
    );
    let error = model_bearer_token(&model(Some(variable)))
        .expect_err("a named but unset variable is a startup refusal");
    assert!(error.to_string().contains(variable));
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
                progress_message: false,
            },
            ..
        })
    ));

    document["transports"][0]["activity"]["unexpected"] = json!(true);
    let error = load(directory.path(), &document)
        .await
        .expect_err("unknown cosmetic settings still fail strict decoding");
    assert!(
        matches!(error, ConfigError::Decode { .. }),
        "an unknown field is a decode refusal, not a later check: {error:?}"
    );
}

#[tokio::test]
async fn whatsapp_configuration_is_explicit_strict_and_pinned() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["transports"][0] = json!({
        "name": "support-whatsapp",
        "kind": "whatsappCloudApi",
        "appSecretEnv": "DEKOPOND_WHATSAPP_APP_SECRET",
        "verifyTokenEnv": "DEKOPOND_WHATSAPP_VERIFY_TOKEN",
        "accessTokenEnv": "DEKOPOND_WHATSAPP_ACCESS_TOKEN",
        "bind": "127.0.0.1:9080",
        "callbackPath": "/webhooks/whatsapp",
        "wabaId": "123456",
        "phoneNumberId": "789012",
        "graphApiVersion": "v23.0"
    });
    document["routes"][0]["transport"] = json!("support-whatsapp");

    let resolved = load(directory.path(), &document)
        .await
        .expect("explicit WhatsApp configuration resolves");
    assert!(matches!(
        resolved.transports.first(),
        Some(config::TransportConfig::WhatsappCloudApi {
            callback_path,
            graph_endpoint: Some(endpoint),
            ..
        }) if callback_path == "/webhooks/whatsapp"
            && endpoint == config::WHATSAPP_GRAPH_ENDPOINT
    ));

    let invalid: [RefusalCase; 11] = [
        ("appSecretEnv", json!("pasted secret"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidEnvironmentName { .. })
            })
        }),
        ("bind", json!("127.0.0.1:0"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappBind { .. })
            })
        }),
        ("callbackPath", json!("relative"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappCallback { .. })
            })
        }),
        ("callbackPath", json!("/webhooks/{wildcard}"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappCallback { .. })
            })
        }),
        ("callbackPath", json!("/webhooks//whatsapp"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappCallback { .. })
            })
        }),
        ("callbackPath", json!("/webhooks/whatsapp/"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappCallback { .. })
            })
        }),
        ("wabaId", json!("0123"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappScope { .. })
            })
        }),
        ("graphApiVersion", json!("latest"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappGraphVersion { .. })
            })
        }),
        ("graphApiVersion", json!("v01.0"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappGraphVersion { .. })
            })
        }),
        ("graphApiVersion", json!("v23.1"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::InvalidWhatsappGraphVersion { .. })
            })
        }),
        ("graphEndpoint", json!("https://evil.example"), |error| {
            reports(error, |problem| {
                matches!(problem, ConfigProblem::UnsupportedEndpoint { .. })
            })
        }),
    ];
    for (field, value, expected) in invalid {
        let mut invalid_document = document.clone();
        invalid_document["transports"][0][field] = value;
        let error = load(directory.path(), &invalid_document)
            .await
            .expect_err(&format!("invalid {field} must fail closed"));
        assert!(
            expected(&error),
            "invalid {field} failed closed for the wrong reason: {error:?}"
        );
    }

    // The transport is text-only. A route that pairs it with an image generator would pay a model
    // for a PNG this transport has no way to deliver, so the pair is refused at startup rather
    // than dropped at reply time.
    let mut with_images = document.clone();
    with_images["imageGenerator"] = json!({
        "model": "gpt-image-1",
        "apiKeyEnv": "OPENAI_IMAGE_API_KEY",
        "timeoutMs": 120_000
    });
    with_images["routes"][0]["imageGenerator"] = json!(true);
    let error = load(directory.path(), &with_images)
        .await
        .expect_err("a text-only transport cannot carry a generated image");
    assert!(
        reports(&error, |problem| matches!(
            problem,
            ConfigProblem::UnsupportedRouteImageGenerator { .. }
        )),
        "the refusal must name the transport pairing: {error:?}"
    );
    assert!(
        error.to_string().contains("text-only"),
        "the refusal names why: {error}"
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

    let cases: Vec<RefusalCase> = vec![
        (
            "unknown top-level field",
            mutate(|document| {
                document["unexpected"] = json!(true);
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "unknown field inside a transport",
            mutate(|document| {
                document["transports"][0]["socketpath"] = json!("/tmp/typo.sock");
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "unknown transport kind",
            mutate(|document| {
                document["transports"][0]["kind"] = json!("carrierPigeon");
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "unknown field inside a model",
            mutate(|document| {
                document["models"][0]["temperature"] = json!(0.7);
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "unknown field inside the image generator",
            mutate(|document| {
                document["imageGenerator"] = json!({
                    "model": "gpt-image-1",
                    "apiKeyEnv": "OPENAI_IMAGE_API_KEY",
                    "timeoutMs": 120_000,
                    "endpoint": "https://attacker.example"
                });
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "image generator with a blank model",
            mutate(|document| {
                document["imageGenerator"] = json!({
                    "model": "  ",
                    "apiKeyEnv": "OPENAI_IMAGE_API_KEY",
                    "timeoutMs": 120_000
                });
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::UnnamedImageModel)
                })
            },
        ),
        (
            "invalid image credential environment name",
            mutate(|document| {
                document["imageGenerator"] = json!({
                    "model": "gpt-image-1",
                    "apiKeyEnv": "sk-live-secret-not-a-name",
                    "timeoutMs": 120_000
                });
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidEnvironmentName { .. })
                })
            },
        ),
        (
            "zero image generator timeout",
            mutate(|document| {
                document["imageGenerator"] = json!({
                    "model": "gpt-image-1",
                    "apiKeyEnv": "OPENAI_IMAGE_API_KEY",
                    "timeoutMs": 0
                });
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidImageGeneratorTimeout)
                })
            },
        ),
        (
            "route enables image generation with none configured",
            mutate(|document| {
                document["routes"][0]["imageGenerator"] = json!(true);
            }),
            |error| {
                reports(error, |problem| {
                    matches!(
                        problem,
                        ConfigProblem::UnconfiguredRouteImageGenerator { .. }
                    )
                })
            },
        ),
        (
            "unknown route match kind",
            mutate(|document| {
                document["routes"][0]["match"] = json!({"kind": "semaphore"});
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
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
            |error| matches!(error, ConfigError::Decode { .. }),
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::DuplicateTransport { .. })
                })
            },
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::DuplicateModel { .. })
                })
            },
        ),
        (
            "route names an unknown transport",
            mutate(|document| {
                document["routes"][0]["transport"] = json!("nowhere");
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::UnknownRouteTransport { .. })
                })
            },
        ),
        (
            "route names an unknown model",
            mutate(|document| {
                document["routes"][0]["model"] = json!("gpt-nonexistent");
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::UnknownRouteModel { .. })
                })
            },
        ),
        (
            "zero step budget",
            mutate(|document| {
                document["routes"][0]["limits"] = json!({"maxSteps": 0});
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidRouteLimits { .. })
                })
            },
        ),
        (
            "zero concurrency",
            mutate(|document| {
                document["sessions"] = json!({"maxConcurrent": 0});
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidSessionLimits)
                })
            },
        ),
        (
            "unknown conversation mode",
            mutate(|document| {
                document["routes"][0]["conversation"] = json!({"mode": "amnesiac"});
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "zero idle timeout on a persistent route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "persistent", "idleTimeoutMs": 0});
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidConversationBounds { .. })
                })
            },
        ),
        (
            "zero turn window on a persistent route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "persistent", "maxTurns": 0});
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidConversationBounds { .. })
                })
            },
        ),
        (
            "zero byte window on a persistent route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "persistent", "maxBytes": 0});
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidConversationBounds { .. })
                })
            },
        ),
        (
            // A window bound that can never take effect is far more likely a mode typo than an
            // intention, and reading it as one silently would produce a bot that forgets everything
            // while its configuration says otherwise.
            "a window bound on a oneShot route",
            mutate(|document| {
                document["routes"][0]["conversation"] = json!({"mode": "oneShot", "maxTurns": 12});
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "an idle timeout on a oneShot route",
            mutate(|document| {
                document["routes"][0]["conversation"] =
                    json!({"mode": "oneShot", "idleTimeoutMs": 900_000});
            }),
            |error| matches!(error, ConfigError::Decode { .. }),
        ),
        (
            "zero conversation ceiling",
            mutate(|document| {
                document["sessions"] = json!({"maxConversations": 0});
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidMaxConversations)
                })
            },
        ),
        (
            "no transports at all",
            mutate(|document| {
                document["transports"] = json!([]);
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::NoTransports)
                })
            },
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidEnvironmentName { .. })
                })
            },
        ),
        (
            "model API key variable that is not a variable name",
            mutate(|document| {
                document["models"][0]["apiKeyEnv"] = json!("sk-live-not-a-variable");
            }),
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidEnvironmentName { .. })
                })
            },
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidSlackActivity { .. })
                })
            },
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::InvalidSlackActivity { .. })
                })
            },
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::UnsupportedEndpoint { .. })
                })
            },
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::UnsupportedEndpoint { .. })
                })
            },
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
            |error| {
                reports(error, |problem| {
                    matches!(problem, ConfigProblem::UnsupportedEndpoint { .. })
                })
            },
        ),
    ];

    for (name, document, expected) in cases {
        let error = load(directory.path(), &document)
            .await
            .expect_err(&format!("{name} must fail closed"));
        assert!(
            expected(&error),
            "{name} failed closed for the wrong reason: {error:?}"
        );
    }
}

/// Every mistake in one file, in one refusal.
///
/// The property `docs/design.md` mandates and `dekopon-config` already keeps: an operator who wrote
/// three zeros fixes three and restarts once, instead of rediscovering the next one after every
/// restart. Three simultaneous problems, three lines, one startup failure.
#[tokio::test]
async fn every_configuration_problem_is_reported_before_the_file_is_refused() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["models"][0]["timeoutMs"] = json!(0);
    document["routes"][0]["limits"] = json!({"maxSteps": 0});
    document["sessions"] = json!({"maxConcurrent": 0});

    let error = load(directory.path(), &document)
        .await
        .expect_err("three mistakes are three problems");
    let ConfigError::Invalid { problems, .. } = &error else {
        panic!("an aggregated refusal, not a first-error return: {error:?}");
    };
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::InvalidModelTimeout { .. }
    )));
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::InvalidRouteLimits { .. }
    )));
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::InvalidSessionLimits
    )));

    // And the operator reads all three off one line rather than off three restarts.
    let rendered = error.to_string();
    assert!(
        rendered.contains("3 validation problems found"),
        "{rendered}"
    );
    assert!(
        rendered.contains("must have a timeout greater than zero"),
        "{rendered}"
    );
    assert!(rendered.contains("at least one step"), "{rendered}");
    assert!(rendered.contains("session bounds"), "{rendered}");
}

/// The model-name grammar names the field and the value, and never blames controls.
///
/// Every configured model's name is a configured-model identifier whether or not the deployment
/// uses `controls:`, so a 0.12.0 file with `name: GPT-5` stops starting. That is a breaking change
/// (`docs/upgrading.md` carries it), and a breaking refusal that says only "invalid configured
/// model controls" points an operator at a feature they are not using.
#[tokio::test]
async fn every_invalid_model_name_is_reported_with_its_field_and_its_value() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["models"][0]["name"] = json!("GPT-5");
    document["models"]
        .as_array_mut()
        .expect("models array")
        .push(json!({
            "name": "-leading-punctuation",
            "kind": "openaiCompatible",
            "endpoint": "http://127.0.0.1:11434/v1",
            "model": "qwen3",
            "timeoutMs": 120_000
        }));

    let error = load(directory.path(), &document)
        .await
        .expect_err("two ungrammatical model names cannot start");
    let ConfigError::Invalid { problems, .. } = &error else {
        panic!("an aggregated refusal: {error:?}");
    };
    let named = problems
        .iter()
        .filter(|problem| matches!(problem, ConfigProblem::InvalidModelName { .. }))
        .count();
    assert_eq!(named, 2, "both names, one refusal: {problems:?}");
    let rendered = error.to_string();
    assert!(rendered.contains("models[].name"), "{rendered}");
    assert!(rendered.contains("GPT-5"), "{rendered}");
    assert!(rendered.contains("-leading-punctuation"), "{rendered}");
    assert!(
        !rendered.contains("invalid configured model controls"),
        "the grammar is not a controls failure: {rendered}"
    );
}

/// `sessions.maxConcurrent` is validated against the store ceiling it actually competes for.
///
/// Every live session holds a checkpoint lease reserving the whole per-job ceiling, so a gateway
/// admitting more sessions than `MAX_JOBS` converts the surplus into capacity refusals under load.
#[tokio::test]
async fn max_concurrent_beyond_the_checkpoint_lease_ceiling_is_refused_at_startup() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["sessions"] = json!({ "maxConcurrent": dekopon_harness::checkpoint::MAX_JOBS + 1 });

    let error = load(directory.path(), &document)
        .await
        .expect_err("more sessions than leases cannot start");
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::ExcessiveMaxConcurrent { .. }
    )));
    let rendered = error.to_string();
    assert!(rendered.contains("sessions.maxConcurrent"), "{rendered}");
    assert!(rendered.contains("MAX_JOBS"), "{rendered}");
    assert!(
        rendered.contains(&dekopon_harness::checkpoint::MAX_JOBS.to_string()),
        "{rendered}"
    );

    let mut exact = document.clone();
    exact["sessions"] = json!({ "maxConcurrent": dekopon_harness::checkpoint::MAX_JOBS });
    load(directory.path(), &exact)
        .await
        .expect("the ceiling itself is admissible");
}

/// Every offending `activityLabels` entry, with the rule it broke.
///
/// One aggregate naming no offender told an operator with twenty labels to go and find it. The
/// byte rule is the harness's own renderability check rather than a raw byte count here, so this
/// gate cannot accept a label the transport then truncates.
#[tokio::test]
async fn every_offending_activity_label_names_its_key_and_its_rule() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"][0]["activityLabels"] = json!({
        "Not A Capability": "Reading the record",
        "http-probe.fetch": "x".repeat(dekopon_harness::activity::MAX_ACTIVITY_LABEL_BYTES + 1),
        // Blank once stripped: the renderer silently replaces it with the default label.
        "http-probe.write": "\u{202e}\u{200b}",
        // Stripping alone is not a loss worth refusing; this one renders whole.
        "http-probe.ok": "Writing\u{202e}the record",
    });

    let error = load(directory.path(), &document)
        .await
        .expect_err("three bad labels cannot start");
    let ConfigError::Invalid { problems, .. } = &error else {
        panic!("an aggregated refusal: {error:?}");
    };
    let offenders = problems
        .iter()
        .filter_map(|problem| match problem {
            ConfigProblem::InvalidActivityLabel { capability, .. } => Some(capability.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(offenders.len(), 3, "{problems:?}");
    for offender in ["Not A Capability", "http-probe.fetch", "http-probe.write"] {
        assert!(offenders.contains(&offender), "{offenders:?}");
    }
    assert!(
        !offenders.contains(&"http-probe.ok"),
        "a valid label is not an offender: {offenders:?}"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("not a capability identifier"),
        "{rendered}"
    );
    assert!(
        rendered.contains(&dekopon_harness::activity::MAX_ACTIVITY_LABEL_BYTES.to_string()),
        "{rendered}"
    );
}

/// A list that failed is not blamed on the routes that name it.
///
/// `dekopon-config` skips its reference pass when a resource never reached its set, because a
/// missing transport reported once by its real name beats the same failure reported again for
/// every route pointing at it. The gateway owes an operator the same signal-to-noise.
#[tokio::test]
async fn routes_are_not_blamed_for_a_transport_list_that_failed_itself() {
    let directory = temporary();

    let mut empty = document(directory.path());
    empty["transports"] = json!([]);
    let error = load(directory.path(), &empty)
        .await
        .expect_err("a gateway with no transport cannot start");
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::NoTransports
    )));
    assert!(
        !reports(&error, |problem| matches!(
            problem,
            ConfigProblem::UnknownRouteTransport { .. }
        )),
        "the route did not make the list empty: {error}"
    );

    let mut unnamed = document(directory.path());
    unnamed["transports"][0]["name"] = json!("   ");
    let error = load(directory.path(), &unnamed)
        .await
        .expect_err("a transport with no name cannot be routed to");
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::UnnamedTransport
    )));
    assert!(
        !reports(&error, |problem| matches!(
            problem,
            ConfigProblem::UnknownRouteTransport { .. }
        )),
        "the route named the transport the operator meant to name: {error}"
    );

    // A duplicate is the other case, and it is deliberately not the same one: the first
    // declaration is still in the name set, so the route it resolves against is real and the
    // reference check keeps running.
    let mut duplicate = document(directory.path());
    duplicate["transports"] = json!([
        { "name": "dev", "kind": "local", "socketPath": directory.path().join("dev.sock") },
        { "name": "dev", "kind": "local", "socketPath": directory.path().join("other.sock") }
    ]);
    duplicate["routes"]
        .as_array_mut()
        .expect("routes array")
        .push(json!({
            "transport": "typo",
            "match": {"kind": "channel"},
            "agent": "reviewer"
        }));
    let error = load(directory.path(), &duplicate)
        .await
        .expect_err("a duplicate transport name and an unknown one are two problems");
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::DuplicateTransport { .. }
    )));
    assert!(reports(&error, |problem| matches!(
        problem,
        ConfigProblem::UnknownRouteTransport { .. }
    )));
}

/// Two missing chat credentials cost one restart, and neither service is spoken to.
///
/// Reading a token inside the connect loop meant the daemon authenticated to the first transport,
/// then died on the second — so an operator who forgot two secrets paid two crash loops, each one
/// having already opened a socket with the token it did have. Both are resolved before anything
/// connects, and the mock endpoints prove nothing dialled them.
#[tokio::test]
async fn every_missing_transport_credential_is_named_before_anything_connects() {
    const SLACK_APP_TOKEN: &str = "DEKOPOND_TEST_MISSING_SLACK_APP_4F1B02";
    const SLACK_BOT_TOKEN: &str = "DEKOPOND_TEST_MISSING_SLACK_BOT_4F1B02";
    const TELEGRAM_TOKEN: &str = "DEKOPOND_TEST_MISSING_TELEGRAM_BOT_4F1B02";
    for variable in [SLACK_APP_TOKEN, SLACK_BOT_TOKEN, TELEGRAM_TOKEN] {
        assert!(
            std::env::var_os(variable).is_none(),
            "fixture must stay unset"
        );
    }

    let directory = temporary();
    // Loopback stand-ins for Slack and Telegram. Neither is ever served: an accepted connection
    // here is the regression this test exists for.
    let slack = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback Slack stand-in");
    let telegram = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback Telegram stand-in");
    for listener in [&slack, &telegram] {
        listener
            .set_nonblocking(true)
            .expect("the listener is polled, never waited on");
    }

    fs::write(
        directory.path().join("dekopon.yaml"),
        catalog_text(true, Some("reasoning")),
    )
    .expect("catalog fixture writes");

    let mut document = document(directory.path());
    document["transports"] = json!([
        {
            "name": "support-slack",
            "kind": "slackSocketMode",
            "appTokenEnv": SLACK_APP_TOKEN,
            "botTokenEnv": SLACK_BOT_TOKEN,
            "endpoint": format!("http://{}", slack.local_addr().expect("bound address"))
        },
        {
            "name": "community-telegram",
            "kind": "telegramLongPoll",
            "botTokenEnv": TELEGRAM_TOKEN,
            "endpoint": format!("http://{}", telegram.local_addr().expect("bound address"))
        }
    ]);
    document["routes"][0]["transport"] = json!("support-slack");
    let path = write_config(directory.path(), &document);

    let error = crate::run(&path, std::future::pending())
        .await
        .expect_err("two unset chat credentials are a startup refusal");
    let crate::DekopondError::Startup { problems } = &error else {
        panic!("one refusal naming both transports, not the first one: {error:?}");
    };
    assert_eq!(problems.len(), 2, "{problems:?}");
    let rendered = error.to_string();
    for variable in [SLACK_APP_TOKEN, TELEGRAM_TOKEN] {
        assert!(
            rendered.contains(variable),
            "the refusal names the variable and never its value: {rendered}"
        );
    }
    assert!(
        rendered.contains("support-slack") && rendered.contains("community-telegram"),
        "the refusal names both transports: {rendered}"
    );

    for listener in [&slack, &telegram] {
        assert!(
            matches!(
                listener.accept(),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
            ),
            "a transport whose credential is unusable must never authenticate"
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
        &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/1000")), None),
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

#[tokio::test]
async fn a_failed_report_names_a_category_and_names_a_timeout_apart_from_one() {
    // Reporting is informational and never retried, so this line is the whole record of it. Without
    // a category, "the web UI shows stale inventory" cannot be told apart from "the broker socket is
    // gone", which is exactly the triage the report exists for.
    use dekopon_broker_protocol::ClientError;

    assert_eq!(crate::report_failure(Ok(Ok(()))), None);
    assert_eq!(
        crate::report_failure(Ok(Err(ClientError::UnsafeSocket))),
        Some("unsafe-socket")
    );
    assert_eq!(
        crate::report_failure(Ok(Err(ClientError::ServerIdentity {
            expected: 501,
            actual: 0
        }))),
        Some("server-identity")
    );
    let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
        .await
        .expect_err("a zero deadline elapses");
    assert_eq!(
        crate::report_failure(Err(elapsed)),
        Some("timeout"),
        "a broker that answered too slowly is not a broker that refused"
    );
}

#[test]
fn every_broker_client_failure_has_its_own_category() {
    // A category is only triage if two different failures are two different values.
    use dekopon_broker_protocol::ClientError;
    use std::{collections::BTreeSet, io};

    let categories = [
        ClientError::SocketMetadata {
            source: io::Error::from(io::ErrorKind::NotFound),
        },
        ClientError::UnsafeSocket,
        ClientError::ConnectTimeout,
        ClientError::Connect {
            source: io::Error::from(io::ErrorKind::ConnectionRefused),
        },
        ClientError::PeerCredentials {
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        },
        ClientError::ServerIdentity {
            expected: 501,
            actual: 0,
        },
        ClientError::UnexpectedResponse,
    ]
    .iter()
    .map(crate::client_error_category)
    .collect::<BTreeSet<_>>();

    assert_eq!(categories.len(), 7);
    assert!(!categories.contains("timeout"), "{categories:?}");
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
        ref error if matches!(only_route_problem(error), RouteProblem::UnknownAgent { .. })
    ));

    // Disabled agent: present in the catalog and deliberately not schedulable.
    assert!(matches!(
        RoutingTable::bind(&config, &catalog(false, Some("reasoning")))
            .expect_err("a disabled agent is a startup failure"),
        ref error if matches!(only_route_problem(error), RouteProblem::DisabledAgent { .. })
    ));

    // A class no configured model offers.
    assert!(matches!(
        RoutingTable::bind(&config, &catalog(true, Some("vision")))
            .expect_err("an unmatched model class is a startup failure"),
        ref error if matches!(only_route_problem(error), RouteProblem::NoModelForClass { .. })
    ));

    // No class and no override: nothing selects a model.
    assert!(matches!(
        RoutingTable::bind(&config, &catalog(true, None))
            .expect_err("an agent with no class and no override is a startup failure"),
        ref error if matches!(only_route_problem(error), RouteProblem::NoModelClass { .. })
    ));
}

/// Every unsatisfiable route, in one refusal.
///
/// Binding scans the whole table for the reason `resolve` scans the whole file: a deployment whose
/// catalog disabled one agent and never declared the other is one restart, not two.
#[tokio::test]
async fn every_unsatisfiable_route_is_reported_in_one_refusal() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"]
        .as_array_mut()
        .expect("routes array")
        .push(json!({
            "transport": "dev",
            "match": {"kind": "channel", "channel": "ops"},
            "agent": "nobody"
        }));
    let config = resolved(directory.path(), &document).await;

    let error = RoutingTable::bind(&config, &catalog(false, Some("reasoning")))
        .expect_err("a disabled agent and an absent one are both startup failures");
    assert_eq!(error.problems.len(), 2, "{:?}", error.problems);
    assert!(matches!(
        error.problems[0],
        RouteProblem::DisabledAgent { .. }
    ));
    assert!(matches!(
        error.problems[1],
        RouteProblem::UnknownAgent { .. }
    ));
    let rendered = error.to_string();
    assert!(
        rendered.contains("2 validation problems found"),
        "{rendered}"
    );
    assert!(
        rendered.contains("which the catalog disables"),
        "{rendered}"
    );
    assert!(
        rendered.contains("which is not in the catalog"),
        "{rendered}"
    );
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

#[test]
fn an_exported_but_blank_credential_is_refused_by_name() {
    for blank in ["", " ", "\n\t "] {
        let error = credential_value("DEKOPOND_WHATSAPP_APP_SECRET", blank.to_owned())
            .expect_err("a blank credential is the absence of one");
        assert!(
            matches!(&error, TransportError::EmptyCredential { name }
                if name == "DEKOPOND_WHATSAPP_APP_SECRET"),
            "{error:?}"
        );
        assert_eq!(error.category(), "empty-credential");
    }
    assert_eq!(
        credential_value("DEKOPOND_WHATSAPP_APP_SECRET", " token ".to_owned())
            .expect("a credential with surrounding space is still a credential"),
        " token "
    );
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
    /// How many times a client was constructed for this script, which is the sharing assertion.
    builds: AtomicUsize,
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
            builds: AtomicUsize::new(0),
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
            builds: AtomicUsize::new(0),
            forbidden: true,
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
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

    /// Portable conversation messages excluding the independently tested request-one bootstrap.
    fn conversation_prompt(&self, index: usize) -> Vec<(String, String)> {
        self.prompt(index)
            .into_iter()
            .filter(|(_, text)| !text.starts_with("Dekopon session bootstrap\n"))
            .collect()
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
    fn build(&self, _model: &ModelConfig) -> Result<SharedModel, SessionError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(ScriptedModel(Arc::clone(self))))
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
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        self.complete_with(messages, tools, &CompletionOptions::default(), recorder)
    }

    fn complete_with(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let result: Result<AssistantTurn, ModelError> = {
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
            };
            if let Ok(turn) = &result
                && let Some(usage) = turn.usage
            {
                recorder.observe(
                    attempt,
                    dekopon_model::usage::UsageObservation {
                        usage,
                        invalid: [false; 5],
                    },
                )?;
            }
            result
        }
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

fn generate_image(prompt: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "image-call".to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: IMAGE_GENERATION_TOOL_NAME.to_owned(),
                arguments: json!({"prompt": prompt}).to_string(),
            },
        }],
        usage: None,
        replay_items: Vec::new(),
    }
}

fn script_call(script: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "script-call".to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: "bash".to_owned(),
                arguments: json!({"script": script}).to_string(),
            },
        }],
        usage: None,
        replay_items: Vec::new(),
    }
}

fn decline_reply() -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "decline-call".to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: DECLINE_REPLY_TOOL_NAME.to_owned(),
                arguments: "{}".to_owned(),
            },
        }],
        usage: None,
        replay_items: Vec::new(),
    }
}

/// One model turn asking for a mounted skill's body.
fn read_skill(name: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: format!("skill-{name}"),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: SKILL_TOOL_NAME.to_owned(),
                arguments: json!({"name": name}).to_string(),
            },
        }],
        usage: None,
        replay_items: Vec::new(),
    }
}

/// One model turn tapping the glass.
fn suggest_improvement() -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "suggestion-1".to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: IMPROVEMENT_TOOL_NAME.to_owned(),
                arguments: json!({
                    "category": "instructions",
                    "target": "reviewer",
                    "summary": "Say which files to read first",
                    "evidence": "The first two turns were spent finding the entry point",
                    "proposal": "Name the entry point in the standing instructions",
                    "confidence": "high"
                })
                .to_string(),
            },
        }],
        usage: None,
        replay_items: Vec::new(),
    }
}

/// Writes one small skill under `root` and loads it the way the catalog would.
fn mounted_skill(root: &Path, name: &str) -> dekopon_config::Skill {
    let directory = root.join(name);
    fs::create_dir_all(directory.join("references")).expect("skill directory");
    fs::write(
        directory.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: Counts things carefully.\n---\n\
             # Counting\n\nAlways count twice.\n"
        ),
    )
    .expect("skill file writes");
    fs::write(directory.join("references/table.md"), "one two three\n")
        .expect("skill resource writes");
    dekopon_config::load_skill(&directory).expect("skill loads")
}

/// The latest tool result one request carried back to the model.
fn tool_message(models: &ModelScript, index: usize) -> String {
    models
        .prompt(index)
        .into_iter()
        .filter_map(|(role, content)| (role == "tool").then_some(content))
        .next_back()
        .unwrap_or_else(|| panic!("request {index} carries a tool result"))
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
    image_bytes: Mutex<Vec<usize>>,
}

impl RecordingReplier {
    fn replies(&self) -> Vec<String> {
        self.replies.lock().expect("reply lock").clone()
    }

    fn image_bytes(&self) -> Vec<usize> {
        self.image_bytes.lock().expect("image reply lock").clone()
    }
}

impl ChatReplier for RecordingReplier {
    fn reply(
        &self,
        _target: ReplyTarget,
        reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            self.replies.lock().expect("reply lock").push(reply.text);
            if let Some(image) = reply.image {
                self.image_bytes
                    .lock()
                    .expect("image reply lock")
                    .push(image.bytes().len());
            }
            Ok(DeliveryReceipt::new("test-acceptance"))
        })
    }
}

#[derive(Default)]
struct RecordingThreadOwnership {
    claimed: Mutex<Vec<ThreadClaim>>,
    revoked: Mutex<Vec<ThreadClaim>>,
}

impl ThreadOwnership for RecordingThreadOwnership {
    fn claim(&self, claim: ThreadClaim) {
        self.claimed.lock().expect("claim lock").push(claim);
    }

    fn revoke(&self, claim: &ThreadClaim) {
        self.revoked
            .lock()
            .expect("revoke lock")
            .push(claim.clone());
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
        reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("surface event lock")
                .push(format!("reply:{}", reply.text));
            Ok(DeliveryReceipt::new("recording-surface"))
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
        _reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            self.events
                .lock()
                .expect("delayed surface events")
                .push("reply");
            Ok(DeliveryReceipt::new("delayed-surface"))
        })
    }
}

struct PartialDeliveryReplier;

impl ChatReplier for PartialDeliveryReplier {
    fn reply(
        &self,
        _target: ReplyTarget,
        _reply: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async { Err(TransportError::PartialDelivery) })
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

fn memory_surface_response() -> ResponseEnvelope {
    ResponseEnvelope::chat_capabilities(
        vec![
            capability("memory.chat.recent"),
            capability("memory.chat.search"),
        ],
        vec!["memory".to_owned()],
        Some(ChatMemorySurface {
            max_lookback_turns: 200,
            prompt_note: "Durable memory is available only on demand.".to_owned(),
        }),
        "fixture-epoch".parse().expect("fixture epoch"),
    )
}

fn record_result(outcome: InvocationOutcome, error: Option<&str>) -> InvocationResult {
    serde_json::from_value(json!({
        "invocation": "record-result-fixture",
        "decision": {
            "decisionId": "record-result-decision",
            "authorizedBy": "broker",
            "policyRevision": "record-result-policy"
        },
        "outcome": outcome,
        "error": error,
        "evidence": []
    }))
    .expect("record result fixture decodes")
}

/// Serves a fixed script of broker responses over a private Unix socket.
///
/// A real socket rather than an in-memory duplex, because the client authenticates the server by
/// socket ownership and peer UID before it writes a byte.
#[allow(
    clippy::let_underscore_must_use,
    reason = "the stub's observation channel is unbounded and its reply goes to a socket the test \
              owns; either failing shows up as the test's own missing request or timeout"
)]
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

/// Repeat each nonempty initial surface for the explicitly expected safe-boundary checks.
/// Refusals are terminal and therefore still receive only one exchange.
async fn stub_broker_with_checks(
    checks: usize,
    directory: &Path,
    responses: Vec<ResponseEnvelope>,
) -> (ResolvedBroker, mpsc::UnboundedReceiver<RequestEnvelope>) {
    stub_broker(
        directory,
        responses
            .into_iter()
            .flat_map(|response| {
                let count = if matches!(&response.response,
            dekopon_broker_protocol::BrokerResponse::Capabilities { capabilities, .. }
                if !capabilities.is_empty())
                {
                    checks
                } else {
                    1
                };
                std::iter::repeat_n(response, count)
            })
            .collect(),
    )
    .await
}

async fn stub_broker_with_effect(
    before: usize,
    after: usize,
    directory: &Path,
    responses: Vec<ResponseEnvelope>,
) -> (ResolvedBroker, mpsc::UnboundedReceiver<RequestEnvelope>) {
    let [surface, effect]: [ResponseEnvelope; 2] = responses.try_into().unwrap();
    let mut responses = vec![surface.clone(); before];
    responses.push(effect);
    responses.extend(std::iter::repeat_n(surface, after));
    stub_broker(directory, responses).await
}

fn session_seed(
    runner: &SessionRunner,
    route: &crate::routes::BoundRoute,
    inbound: &InboundMessage,
    surface: ResponseEnvelope,
) -> dekopon_harness::conversation::ConversationSeed {
    let key = ConversationKey::scoped(
        route.agent.as_str(),
        &route.cache_key,
        &inbound.transport,
        &inbound.channel,
        &inbound.conversation_id,
        &inbound.subject.canonical(),
    );
    runner.conversations.begin(
        &key,
        &conversation_surface(surface),
        route.conversation.window().unwrap(),
        Instant::now(),
    )
}

fn conversation_surface(response: ResponseEnvelope) -> Vec<String> {
    match response.response {
        dekopon_broker_protocol::BrokerResponse::Capabilities {
            capabilities,
            command_words,
            surface_epoch,
            ..
        } => {
            struct Metadata(
                Vec<dekopon_broker_protocol::AvailableCapability>,
                Vec<String>,
            );
            impl dekopon_shell::CapabilityInvoker for Metadata {
                fn granted(&self) -> Vec<String> {
                    self.0.iter().map(|c| c.capability.id.to_string()).collect()
                }
                fn command_words(&self) -> Vec<String> {
                    self.1.clone()
                }
                fn describe(&self, id: &str) -> Option<dekopon_shell::CapabilityDescription> {
                    self.0
                        .iter()
                        .find(|c| c.capability.id.as_str() == id)
                        .map(|c| dekopon_shell::CapabilityDescription {
                            capability: id.into(),
                            description: c.capability.description.clone(),
                            input_schema: c.capability.input_schema.clone(),
                        })
                }
                fn invoke(
                    &self,
                    _: &str,
                    _: Value,
                    _: Option<dekopon_core::SecretUseProposal>,
                ) -> dekopon_shell::CapabilityCallResult {
                    panic!("metadata only")
                }
            }
            let metadata = dekopon_harness::bootstrap::CapabilitySnapshot::from_invoker(&Metadata(
                capabilities,
                command_words,
            ))
            .unwrap();
            vec![metadata.fingerprint(), surface_epoch.to_string()]
        }
        _ => panic!("capability fixture"),
    }
}

fn route(model: ModelConfig) -> crate::routes::BoundRoute {
    crate::routes::BoundRoute {
        transport: "dev".to_owned(),
        r#match: RouteMatch::DirectMessage {},
        agent: "reviewer".parse().expect("valid agent fixture"),
        description: "Reviews things".to_owned(),
        model_class: Some("reasoning".to_owned()),
        instructions: Some("Answer briefly.".to_owned()),
        skills: Arc::from(Vec::new()),
        model: Arc::new(model),
        controls: None,
        image_generator: false,
        improvement_suggestions: false,
        activity_labels: Default::default(),
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
        effort: Default::default(),
    }
}

fn message(text: &str) -> InboundMessage {
    InboundMessage {
        transport: "dev".to_owned(),
        transport_kind: dekopon_broker_protocol::ChatTransportKind::Local,
        subject: subject(),
        channel: "dev".to_owned(),
        thread: None,
        conversation_id: "dev".to_owned(),
        message_id: "0123456789abcdef0123456789abcdef-1-1".to_owned(),
        text: text.to_owned(),
        assets: Vec::new(),
        conversation: ConversationKind::DirectMessage,
        // Direct messages ignore addressing. Channel tests opt into structured addressing where
        // that is the behavior under test.
        addressed: None,
        thread_continuation: None,
        reply: ReplyTarget::Local { connection: 1 },
        activity: None,
    }
}

#[test]
fn whatsapp_delivery_identity_is_typed_and_bound_to_its_attested_scope() {
    let mut inbound = message("hello");
    inbound.transport = "support-whatsapp".to_owned();
    inbound.transport_kind = dekopon_broker_protocol::ChatTransportKind::Whatsapp;
    inbound.subject = ExternalSubject::whatsapp("16034700182").expect("subject");
    inbound.channel = "123:456:16034700182".to_owned();
    inbound.conversation_id = inbound.channel.clone();
    inbound.message_id = "wamid.delivery".to_owned();
    inbound.reply = ReplyTarget::WhatsApp {
        recipient: "16034700182".to_owned(),
    };
    let claim = dekopon_broker_protocol::Attestation::for_chat(
        inbound.subject.clone(),
        "reviewer".parse().expect("agent"),
        dekopon_broker_protocol::ChatScopeClaim {
            transport: "support-whatsapp".parse().expect("transport"),
            kind: dekopon_broker_protocol::ChatTransportKind::Whatsapp,
            channel: inbound.channel.clone(),
            conversation: inbound.conversation_id.clone(),
        },
    );
    let delivery = crate::session::delivery_identity(&inbound, &claim)
        .expect("WhatsApp replies can be recorded after transport acceptance");
    assert_eq!(
        delivery,
        dekopon_broker_protocol::DeliveryIdentity::Whatsapp {
            waba: "123".to_owned(),
            phone_number: "456".to_owned(),
            message: "wamid.delivery".to_owned(),
        }
    );
    assert!(delivery.is_canonical_for(&claim.scope.expect("chat scope")));
}

fn slack_thread_continuation(inherited: bool) -> ThreadContinuation {
    ThreadContinuation {
        claim: ThreadClaim::Slack {
            team_id: "t0123abc".to_owned(),
            channel_id: "c0123abc".to_owned(),
            thread_ts: "1700000000.000001".to_owned(),
            user_id: "u9xyz".to_owned(),
        },
        inherited,
    }
}

fn owned_slack_message(text: &str, inherited: bool) -> InboundMessage {
    InboundMessage {
        transport: "scientist-slack".to_owned(),
        transport_kind: dekopon_broker_protocol::ChatTransportKind::Slack,
        subject: "slack.t0123abc.u9xyz"
            .parse()
            .expect("Slack subject fixture"),
        channel: "c0123abc".to_owned(),
        thread: Some("1700000000.000001".to_owned()),
        conversation_id: "c0123abc:1700000000.000001".to_owned(),
        message_id: "1700000000.000002".to_owned(),
        text: text.to_owned(),
        assets: Vec::new(),
        conversation: ConversationKind::Channel("c0123abc".to_owned()),
        addressed: Some(!inherited),
        thread_continuation: Some(slack_thread_continuation(inherited)),
        reply: ReplyTarget::Slack {
            channel: "c0123abc".to_owned(),
            thread_ts: Some("1700000000.000001".to_owned()),
        },
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
        models: Arc::new(ModelCache::new(models)),
        gate: SessionGate::new(max_concurrent),
        reply_on_busy: true,
        conversations: BoundedConversationStore::new(max_conversations),
        assets: Arc::new(AssetStore::new(
            max_conversations,
            Duration::from_secs(60 * 60),
        )),
        asset_fetchers: HashMap::new(),
        image_generator: None,
        activities: HashMap::new(),
        thread_ownership: HashMap::new(),
        active_sessions: Default::default(),
        usage_reports: None,
        activity_supervisors: crate::activity::ActivitySupervisors::default(),
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
            #[allow(
                clippy::let_underscore_must_use,
                reason = "a blocked model that already gave up on being released fails the test at \
                          its own recv_timeout, not here"
            )]
            let _ = sender.send(());
        }
    }
}

impl ModelFactory for Arc<BlockedModel> {
    fn build(&self, _model: &ModelConfig) -> Result<SharedModel, SessionError> {
        Ok(Arc::new(BlockedHandle(Arc::clone(self))))
    }
}

struct BlockedHandle(Arc<BlockedModel>);

impl ChatModel for BlockedHandle {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "both halves are the test's own rendezvous: an unobserved entry signal fails \
                  wait_until_entered, and a release that never arrives is bounded by the timeout"
    )]
    fn complete(
        &self,
        _messages: &[ModelMessage],
        _tools: &[ModelTool],
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let result: Result<AssistantTurn, ModelError> = {
                if let Some(sender) = self.0.entered.lock().expect("entered lock").take() {
                    let _ = sender.send(());
                }
                if let Some(receiver) = self.0.release_signal.lock().expect("release lock").take() {
                    let _ = receiver.recv_timeout(Duration::from_secs(30));
                }
                Ok(self.0.turn.clone())
            };
            if let Ok(turn) = &result
                && let Some(usage) = turn.usage
            {
                recorder.observe(
                    attempt,
                    dekopon_model::usage::UsageObservation {
                        usage,
                        invalid: [false; 5],
                    },
                )?;
            }
            result
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorized_message_reaches_its_agent_and_answers_in_chat() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
    let BrokerRequest::Capabilities {
        attestation: Some(claim),
    } = request.request
    else {
        panic!("a session must open a chat-scoped attested leg: {request:?}");
    };
    assert_eq!(claim.subject.canonical(), SUBJECT);
    assert_eq!(claim.agent.as_str(), "reviewer");
    assert_eq!(claim.scope.expect("chat scope").transport.as_str(), "dev");
}

struct TestImageGenerator;

impl ImageGenerator for TestImageGenerator {
    fn generate(
        &self,
        _prompt: &str,
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<GeneratedImage, ImageGenerationError> {
        recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(b"kitty pixels");
        GeneratedImage::from_png(png)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_route_generator_yields_an_image_reply() {
    let directory = temporary();
    let (broker, _) = stub_broker_with_checks(
        6,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
        )],
    )
    .await;
    let models = ModelScript::new([
        generate_image("a cheerful watercolor kitten"),
        answer("Here is your kitty."),
    ]);
    let replier = Arc::new(RecordingReplier::default());
    let mut runner = runner(broker, Arc::clone(&models), 4);
    Arc::get_mut(&mut runner)
        .expect("runner is uniquely owned")
        .image_generator = Some(Arc::new(TestImageGenerator));
    let mut route = route(model_config());
    route.image_generator = true;

    run_session(
        runner,
        route,
        message("draw me a kitty cat"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), ["Here is your kitty."]);
    assert_eq!(replier.image_bytes(), [20]);
    assert_eq!(models.requests(), 2);
    assert!(
        models
            .tool_names(0)
            .contains(&IMAGE_GENERATION_TOOL_NAME.to_owned())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_freshly_authorized_agent_message_claims_its_exact_sender_thread() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(1, &["echo.echo"])).await;
    let models = ModelScript::new([answer("Claimed.")]);
    let replier = Arc::new(RecordingReplier::default());
    let ownership = Arc::new(RecordingThreadOwnership::default());
    let mut runner = runner(broker, Arc::clone(&models), 4);
    Arc::get_mut(&mut runner)
        .expect("fixture owns its runner")
        .thread_ownership
        .insert(
            "scientist-slack".to_owned(),
            Arc::clone(&ownership) as Arc<dyn ThreadOwnership>,
        );
    let message = owned_slack_message("<@u0botbot> help", false);
    let expected = message
        .thread_continuation
        .as_ref()
        .expect("Agent claim")
        .claim
        .clone();

    run_session(
        runner,
        route(model_config()),
        message,
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(*ownership.claimed.lock().expect("claim lock"), [expected]);
    assert!(ownership.revoked.lock().expect("revoke lock").is_empty());
    assert_eq!(replier.replies(), ["Claimed."]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_sender_loses_owned_thread_continuation() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(1, &[])).await;
    let models = ModelScript::forbidden();
    let replier = Arc::new(RecordingReplier::default());
    let ownership = Arc::new(RecordingThreadOwnership::default());
    let mut runner = runner(broker, Arc::clone(&models), 4);
    Arc::get_mut(&mut runner)
        .expect("fixture owns its runner")
        .thread_ownership
        .insert(
            "scientist-slack".to_owned(),
            Arc::clone(&ownership) as Arc<dyn ThreadOwnership>,
        );
    let message = owned_slack_message("anything else?", true);
    let expected = message
        .thread_continuation
        .as_ref()
        .expect("Agent continuation")
        .claim
        .clone();

    run_session(
        runner,
        route(model_config()),
        message,
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert!(ownership.claimed.lock().expect("claim lock").is_empty());
    assert_eq!(*ownership.revoked.lock().expect("revoke lock"), [expected]);
    assert_eq!(replier.replies(), [UNAUTHORIZED_REPLY]);
    assert_eq!(models.requests(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owned_unaddressed_thread_message_may_end_without_any_slack_post() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![memory_surface_response(), ResponseEnvelope::acknowledged()],
    )
    .await;
    let models = ModelScript::new([decline_reply()]);
    let replier = Arc::new(RecordingReplier::default());
    let activity = Arc::new(RecordingSurface::default());
    let mut runner = runner(broker, Arc::clone(&models), 4);
    Arc::get_mut(&mut runner)
        .expect("fixture owns its runner")
        .activities
        .insert(
            "scientist-slack".to_owned(),
            Arc::clone(&activity) as Arc<dyn ChatActivity>,
        );
    let route = persistent_route(model_config(), window());
    let mut message = owned_slack_message("OK, thanks", true);
    message.activity = Some(ActivityTarget::Slack {
        channel_id: "c0123abc".to_owned(),
        thread_ts: "1700000000.000001".to_owned(),
        message_ts: "1700000000.000002".to_owned(),
        initiator_user_id: "u9xyz".to_owned(),
    });
    let key = ConversationKey::scoped(
        route.agent.as_str(),
        &route.cache_key,
        &message.transport,
        &message.channel,
        &message.conversation_id,
        &message.subject.canonical(),
    );
    let surface = conversation_surface(memory_surface_response());

    run_session(
        Arc::clone(&runner),
        route,
        message,
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert!(
        replier.replies().is_empty(),
        "declining must not call chat.postMessage"
    );
    activity.wait_until_hidden().await;
    assert_eq!(
        activity.events().last().map(String::as_str),
        Some("hide"),
        "declining must return native activity to its inactive state"
    );
    assert_eq!(models.requests(), 1);
    assert!(
        models
            .tool_names(0)
            .iter()
            .any(|name| name == DECLINE_REPLY_TOOL_NAME)
    );
    assert!(
        models
            .prompt(0)
            .iter()
            .any(|(role, text)| role == "system" && text.contains("last word"))
    );
    let remembered = runner
        .conversations
        .begin(&key, &surface, window(), Instant::now());
    assert_eq!(remembered.history.turns().len(), 1);
    assert_eq!(remembered.history.turns()[0].user(), "OK, thanks");
    assert_eq!(remembered.history.turns()[0].answer(), None);
    let record = &remembered.history.turns()[0];
    assert_eq!(
        record.delivery,
        dekopon_harness::history::DeliveryDisposition::Suppressed
    );
    let checkpoint = dekopon_harness::checkpoint::memory_checkpoints()
        .load(&record.job)
        .unwrap();
    assert!(checkpoint.finalized);
    assert_eq!(checkpoint.record.delivery, record.delivery);
    assert_eq!(checkpoint.state.accounting.delivery, "suppressed");
    assert!(matches!(
        observed
            .recv()
            .await
            .expect("authorization request")
            .request,
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));
    assert_surface_checks(&mut observed, 2);
    assert!(
        observed.try_recv().is_err(),
        "no Slack acceptance means no durable-memory record request"
    );
}

/// A provider command word answers through the broker leg as the tool it fronts: the help page a
/// guest renders reaches the model with the status the guest chose, over the run operation, and
/// proposes nothing to invoke.
#[tokio::test(flavor = "multi_thread")]
async fn a_rendered_command_word_reaches_the_model_through_the_broker_leg() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_effect(
        3,
        2,
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo")],
                vec!["probe".to_owned()],
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
            ResponseEnvelope::command_run(CommandRunOutcome::Rendered {
                stdout: "Usage: probe <COMMAND>\n".to_owned(),
                stderr: String::new(),
                status: 0,
            }),
        ],
    )
    .await;
    let models = ModelScript::new([script_call("probe --help"), answer("done")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);

    run_session(
        runner,
        route(model_config()),
        message("show me the help"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), ["done"]);
    assert!(matches!(
        observed
            .recv()
            .await
            .expect("authorization request")
            .request,
        BrokerRequest::Capabilities { .. }
    ));
    assert_surface_checks(&mut observed, 2);
    let run = observed.recv().await.expect("the command run").request;
    assert!(
        matches!(
            &run,
            BrokerRequest::RunCommand { word, argv, stdin: None, .. }
                if word == "probe" && argv == &["--help".to_owned()]
        ),
        "{run:?}"
    );
    assert_surface_checks(&mut observed, 2);
    assert!(
        observed.try_recv().is_err(),
        "rendered text proposes nothing to invoke"
    );
    let tool = tool_message(&models, 1);
    assert!(tool.contains("Usage: probe <COMMAND>"), "{tool}");
    assert!(tool.contains("[exit code: 0]"), "{tool}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_final_turn_decline_after_capability_work_warns_against_blind_retry() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_effect(
        3,
        2,
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo")],
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
            ResponseEnvelope::invocation(record_result(InvocationOutcome::Succeeded, None)),
        ],
    )
    .await;
    let models = ModelScript::new([script_call("echo.echo '{}'"), decline_reply()]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let mut route = persistent_route(model_config(), window());
    route.limits.max_steps = 2;

    run_session(
        runner,
        route,
        owned_slack_message("maybe do this", true),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), [UNREPORTED_WORK_REPLY]);
    assert!(matches!(
        observed
            .recv()
            .await
            .expect("authorization request")
            .request,
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));
    assert_surface_checks(&mut observed, 2);
    assert!(matches!(
        observed
            .recv()
            .await
            .expect("capability invocation")
            .request,
        BrokerRequest::Invoke {
            attestation: Some(Attestation { scope: Some(_), .. }),
            ..
        }
    ));
    assert_surface_checks(&mut observed, 2);
    assert!(
        observed.try_recv().is_err(),
        "the warning is not a delivered model answer and must not be durably recorded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_hidden_record_request_follows_transport_acceptance_and_is_never_retried() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![
            memory_surface_response(),
            ResponseEnvelope::error("outcome-unaudited", "do not retry"),
            // Keep the listener alive for a third exchange. If recording retries, the request is
            // observed and receives this response instead of merely failing to reconnect after
            // the fixture exits.
            ResponseEnvelope::error("outcome-unaudited", "still do not retry"),
        ],
    )
    .await;
    let models = ModelScript::new([answer("The exact accepted answer.")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);

    run_session(
        runner,
        route(model_config()),
        message("the exact sender text"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), ["The exact accepted answer."]);
    assert!(matches!(
        observed.recv().await.expect("surface request").request,
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));
    assert_surface_checks(&mut observed, 2);
    let record = observed.recv().await.expect("one record request");
    let BrokerRequest::RecordDeliveredTurn { attestation, turn } = record.request else {
        panic!("expected hidden record operation: {record:?}");
    };
    assert_eq!(turn.user, "the exact sender text");
    assert_eq!(turn.assistant, "The exact accepted answer.");
    assert_eq!(
        turn.delivery,
        dekopon_broker_protocol::DeliveryIdentity::Local {
            transport: "dev".parse().expect("transport"),
            conversation: "dev".to_owned(),
            boot_nonce: "0123456789abcdef0123456789abcdef".to_owned(),
            connection: 1,
            sequence: 1,
        }
    );
    assert_eq!(Some(turn.id), attestation.invocation);
    assert!(
        observed.try_recv().is_err(),
        "outcome-unknown must never trigger a retry"
    );
}

#[test]
fn record_outcomes_have_a_stable_content_free_failure_vocabulary() {
    for (outcome, error, expected) in [
        (InvocationOutcome::Succeeded, None, None),
        (
            InvocationOutcome::Denied,
            Some("policy detail sentinel"),
            Some("denied"),
        ),
        (
            InvocationOutcome::Failed,
            Some("dedup-capacity"),
            Some("dedup-capacity"),
        ),
        (
            InvocationOutcome::Failed,
            Some("dedup-conflict"),
            Some("dedup-conflict"),
        ),
        (
            InvocationOutcome::Failed,
            Some("memory-corrupt"),
            Some("memory-corrupt"),
        ),
        (
            InvocationOutcome::Failed,
            Some("result-too-large"),
            Some("result-too-large"),
        ),
        (
            InvocationOutcome::Failed,
            Some("storage-quota"),
            Some("storage-quota"),
        ),
        (
            InvocationOutcome::Failed,
            Some("storage-busy"),
            Some("storage-busy"),
        ),
        (
            InvocationOutcome::Failed,
            Some("storage-timeout"),
            Some("storage-timeout"),
        ),
        (
            InvocationOutcome::Failed,
            Some("storage-corrupt"),
            Some("storage-corrupt"),
        ),
        (
            InvocationOutcome::Failed,
            Some("storage-io"),
            Some("storage-io"),
        ),
        (
            InvocationOutcome::Failed,
            Some("untrusted future detail sentinel"),
            Some("failed"),
        ),
    ] {
        assert_eq!(
            memory_record_outcome_category(&record_result(outcome, error)),
            expected
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_failed_dedup_and_storage_record_results_are_terminal_without_retry() {
    for (outcome, error) in [
        (InvocationOutcome::Denied, Some("policy-denied")),
        (InvocationOutcome::Failed, Some("provider-failure")),
        (InvocationOutcome::Failed, Some("dedup-capacity")),
        (InvocationOutcome::Failed, Some("dedup-conflict")),
        (InvocationOutcome::Failed, Some("storage-quota")),
        (InvocationOutcome::Failed, Some("storage-busy")),
        (InvocationOutcome::Failed, Some("storage-timeout")),
        (InvocationOutcome::Failed, Some("storage-corrupt")),
        (InvocationOutcome::Failed, Some("storage-io")),
    ] {
        let directory = temporary();
        let result = record_result(outcome, error);
        let (broker, mut observed) = stub_broker_with_checks(
            3,
            directory.path(),
            vec![
                memory_surface_response(),
                ResponseEnvelope::invocation(result.clone()),
                ResponseEnvelope::invocation(result),
            ],
        )
        .await;
        let models = ModelScript::new([answer("The delivered answer remains delivered.")]);
        let replier = Arc::new(RecordingReplier::default());

        run_session(
            runner(broker, Arc::clone(&models), 4),
            route(model_config()),
            message("record this once"),
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;

        assert_eq!(
            replier.replies(),
            ["The delivered answer remains delivered."]
        );
        assert!(matches!(
            observed.recv().await.expect("surface request").request,
            BrokerRequest::Capabilities {
                attestation: Some(Attestation { scope: Some(_), .. })
            }
        ));
        assert_surface_checks(&mut observed, 2);
        assert!(matches!(
            observed.recv().await.expect("record request").request,
            BrokerRequest::RecordDeliveredTurn { .. }
        ));
        assert!(
            observed.try_recv().is_err(),
            "{outcome:?}/{error:?} unexpectedly retried"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn model_failure_and_partial_delivery_never_record_the_gateways_failure_text() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_checks(
        2,
        directory.path(),
        vec![memory_surface_response(), ResponseEnvelope::acknowledged()],
    )
    .await;
    let models = ModelScript::scripted([None]);
    let replier = Arc::new(RecordingReplier::default());
    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("the model will fail"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;
    assert_eq!(replier.replies(), [FAILURE_REPLY]);
    assert!(matches!(
        observed.recv().await.expect("surface request").request,
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));
    assert_surface_checks(&mut observed, 1);
    assert!(
        observed.try_recv().is_err(),
        "the fixed gateway failure reply must not be recorded"
    );

    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![memory_surface_response(), ResponseEnvelope::acknowledged()],
    )
    .await;
    let models = ModelScript::new([answer("one chunk lands and another fails")]);
    run_session(
        runner(broker, Arc::clone(&models), 4),
        route(model_config()),
        message("partial delivery"),
        Arc::new(PartialDeliveryReplier) as Arc<dyn ChatReplier>,
    )
    .await;
    assert!(matches!(
        observed.recv().await.expect("surface request").request,
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));
    assert_surface_checks(&mut observed, 2);
    assert!(
        observed.try_recv().is_err(),
        "partial transport delivery must not be recorded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn authorized_work_shows_activity_until_after_the_durable_reply() {
    let directory = temporary();
    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
        vec![ResponseEnvelope::capabilities(
            Vec::new(),
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
        )],
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
async fn a_native_stop_suppresses_delivery_but_retains_private_job_history() {
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker_with_checks(3, directory.path(), vec![memory_surface_response()]).await;
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
        route.clone(),
        inbound.clone(),
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
        1,
        "Stop retains the independently recorded job, not a delivery receipt"
    );
    let seed = session_seed(&runner, &route, &inbound, memory_surface_response());
    let record = &seed.history.turns()[0];
    assert_eq!(
        record.delivery,
        dekopon_harness::history::DeliveryDisposition::Cancelled
    );
    let checkpoint = dekopon_harness::checkpoint::memory_checkpoints()
        .load(&record.job)
        .unwrap();
    assert!(checkpoint.finalized);
    assert_eq!(checkpoint.record.delivery, record.delivery);
    assert_eq!(checkpoint.state.accounting.delivery, "cancelled");
    assert_eq!(checkpoint.state.accounting.calls.len(), 1);
    assert!(matches!(
        observed.recv().await.expect("surface request").request,
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));
    assert_surface_checks(&mut observed, 2);
    assert!(
        observed.try_recv().is_err(),
        "a cancelled turn is never durably recorded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aborting_the_async_session_cancels_later_blocking_tool_work() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo")],
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
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
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));

    assert_surface_checks(&mut observed, 1);
    session.abort();
    assert!(
        session
            .await
            .expect_err("session task is aborted")
            .is_cancelled(),
        "the async owner is gone"
    );
    model.release();
    let freshness = tokio::time::timeout(Duration::from_secs(3), observed.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            freshness.request,
            BrokerRequest::Capabilities {
                attestation: Some(_),
                ..
            }
        ),
        "the late model result still crosses a fresh authorization check, never an invocation"
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(300), observed.recv())
            .await
            .is_err(),
        "the cancellation guard prevents the model's late tool call reaching the broker"
    );
    assert!(replier.replies().is_empty());
}

/// The catalog's skills ride the bound route, so a session never touches the filesystem.
#[tokio::test]
async fn a_bound_route_carries_the_skills_its_agent_mounts() {
    let directory = temporary();
    let _skill = mounted_skill(&directory.path().join("skills"), "counting");
    let text = format!(
        "{}  skills:\n    - skills/counting\n",
        catalog_text(true, Some("reasoning"))
    );
    let catalog = LocalCatalog::from_str(directory.path().join("dekopon.yaml"), &text)
        .expect("catalog with a skill parses");
    let resolved = load(directory.path(), &document(directory.path()))
        .await
        .expect("configuration resolves");

    let routes = RoutingTable::bind(&resolved, &catalog).expect("route binds");
    let route = routes
        .route("dev", &ConversationKind::DirectMessage)
        .expect("route matches");

    assert_eq!(route.skills.len(), 1);
    assert_eq!(route.skills[0].name().as_str(), "counting");
    assert!(!route.improvement_suggestions);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_lists_mounted_skills_by_summary_and_reads_one_on_demand() {
    let directory = temporary();
    let skill = mounted_skill(directory.path(), "counting");
    let (broker, _observed) = stub_broker_with_checks(
        9,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
        )],
    )
    .await;
    let models = ModelScript::new([
        read_skill("counting"),
        inspect_agent_config(),
        answer("Counted twice."),
    ]);
    let replier = Arc::new(RecordingReplier::default());
    let route = crate::routes::BoundRoute {
        skills: Arc::from(vec![skill]),
        ..route(model_config())
    };

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route,
        message("count the posts"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec!["Counted twice.".to_owned()]);
    assert_eq!(models.requests(), 3);
    let tools = models.tool_names(0);
    assert!(tools.contains(&SKILL_TOOL_NAME.to_owned()), "{tools:?}");
    assert!(
        !tools.contains(&IMPROVEMENT_TOOL_NAME.to_owned()),
        "suggestions are a route opt-in: {tools:?}"
    );
    let listing = models
        .prompt(0)
        .into_iter()
        .filter(|(role, _)| role == "system")
        .map(|(_, content)| content)
        .find(|content| content.contains("Skills mounted for this agent"))
        .expect("the skills listing is a system message");
    assert!(listing.contains("counting"), "{listing}");
    assert!(listing.contains("Counts things carefully."), "{listing}");
    assert!(
        !listing.contains("Always count twice."),
        "the body is read on demand, not listed: {listing}"
    );
    let body = tool_message(&models, 1);
    assert!(body.contains("Always count twice."), "{body}");

    // The configuration view names the skill and its files, never their text.
    let view: Value =
        serde_json::from_str(&tool_message(&models, 2)).expect("the meta result is JSON");
    assert_eq!(view["skills"][0]["name"], "counting");
    assert_eq!(view["skills"][0]["description"], "Counts things carefully.");
    assert_eq!(
        view["skills"][0]["resources"],
        json!(["references/table.md"])
    );
    assert!(!view.to_string().contains("Always count twice."));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_suggestion_tool_is_offered_only_where_the_route_opts_in() {
    let directory = temporary();
    let (broker, _observed) = stub_broker_with_checks(
        6,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
        )],
    )
    .await;
    let models = ModelScript::new([suggest_improvement(), answer("Noted.")]);
    let replier = Arc::new(RecordingReplier::default());
    let route = crate::routes::BoundRoute {
        improvement_suggestions: true,
        ..route(model_config())
    };

    run_session(
        runner(broker, Arc::clone(&models), 4),
        route,
        message("how could this go better?"),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec!["Noted.".to_owned()]);
    assert_eq!(models.requests(), 2);
    let tools = models.tool_names(0);
    assert!(
        tools.contains(&IMPROVEMENT_TOOL_NAME.to_owned()),
        "{tools:?}"
    );
    assert!(
        !tools.contains(&SKILL_TOOL_NAME.to_owned()),
        "no skill is mounted, so nothing offers to read one: {tools:?}"
    );
    let recorded = tool_message(&models, 1);
    assert!(
        recorded.contains("Recorded suggestion 1 of 3"),
        "{recorded}"
    );
}

#[tokio::test]
async fn improvement_suggestions_are_a_per_route_opt_in() {
    let directory = temporary();
    let mut document = document(directory.path());
    let resolved = load(directory.path(), &document)
        .await
        .expect("the default configuration resolves");
    assert!(!resolved.routes[0].improvement_suggestions);

    document["routes"][0]["improvementSuggestions"] = json!(true);
    let resolved = load(directory.path(), &document)
        .await
        .expect("the opt-in resolves");
    assert!(resolved.routes[0].improvement_suggestions);
    let routes =
        RoutingTable::bind(&resolved, &catalog(true, Some("reasoning"))).expect("the route binds");
    assert!(
        routes
            .route("dev", &ConversationKind::DirectMessage)
            .expect("route matches")
            .improvement_suggestions
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorized_agent_can_inspect_its_credential_free_effective_configuration() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker_with_checks(
        5,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
        BrokerRequest::Capabilities {
            attestation: Some(Attestation { scope: Some(_), .. })
        }
    ));
    assert_surface_checks(&mut observed, 4);
    assert!(
        observed.try_recv().is_err(),
        "meta inspection makes no broker call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_reports_unreported_model_usage_without_delaying_the_answer() {
    let directory = temporary();
    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
        vec![ResponseEnvelope::capabilities(
            Vec::new(),
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
        )],
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
    let ownership = Arc::new(RecordingThreadOwnership::default());
    let mut runner = runner(broker, Arc::clone(&models), 4);
    Arc::get_mut(&mut runner)
        .expect("fixture owns its runner")
        .thread_ownership
        .insert(
            "scientist-slack".to_owned(),
            Arc::clone(&ownership) as Arc<dyn ThreadOwnership>,
        );
    let message = owned_slack_message("hello", true);
    let expected = message
        .thread_continuation
        .as_ref()
        .expect("owned continuation")
        .claim
        .clone();

    run_session(
        runner,
        route(model_config()),
        message,
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![UNAUTHORIZED_REPLY.to_owned()]);
    assert_eq!(*ownership.revoked.lock().expect("revoke lock"), [expected]);
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
    let (broker, _observed) = stub_broker_with_checks(
        2,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
    let mut expected: Vec<_> = messages
        .iter()
        .map(|(role, content)| ((*role).to_owned(), (*content).to_owned()))
        .collect();
    expected.insert(1, ("system".into(), concat!(
        "Host-selected inference identity (not authorization): ",
        r#"{"configured":"local-qwen","backend":"adapter","model":"qwen3","effort":"providerDefault"}"#
    ).into()));
    expected
}

/// Every broker request the stub saw, asserting each one was a capability listing.
///
/// The exact count includes initial admission plus safe boundaries, proving that authorization
/// is freshly checked rather than remembered with the conversation.
fn capability_listings(observed: &mut mpsc::UnboundedReceiver<RequestEnvelope>) -> usize {
    let mut count = 0;
    while let Ok(request) = observed.try_recv() {
        assert!(
            matches!(
                request.request,
                BrokerRequest::Capabilities {
                    attestation: Some(Attestation { scope: Some(_), .. })
                }
            ),
            "every session opens a chat-scoped attested leg: {request:?}"
        );
        count += 1;
    }
    count
}

fn assert_surface_checks(observed: &mut mpsc::UnboundedReceiver<RequestEnvelope>, count: usize) {
    for _ in 0..count {
        assert!(matches!(
            observed
                .try_recv()
                .expect("expected fresh surface check")
                .request,
            BrokerRequest::Capabilities {
                attestation: Some(Attestation { scope: Some(_), .. })
            }
        ));
    }
}

/// Exactly `count` surface exchanges; callers include every safe-yield check they expect.
fn listings(count: usize, capabilities: &[&str]) -> Vec<ResponseEnvelope> {
    (0..count)
        .map(|_| {
            ResponseEnvelope::capabilities(
                capabilities
                    .iter()
                    .map(|identifier| capability(identifier))
                    .collect(),
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
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
    store: &BoundedConversationStore,
    key: &ConversationKey,
    granted: &[String],
    window: ConversationWindow,
    turn: JobRecord,
    now: Instant,
) {
    let seed = store.begin(key, granted, window, now);
    store
        .commit(key, granted, window, turn, &seed.cache_key, now)
        .expect("live test lease");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_route_replays_the_previous_exchange_into_the_next_prompt() {
    // The whole feature in one assertion: a follow-up that says "and the second one?" is answerable
    // because the exchange before it is in the prompt, in order, ahead of the new message.
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker_with_checks(3, directory.path(), listings(2, &["echo.echo"])).await;
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
        models.conversation_prompt(0),
        transcript(&[("system", "Answer briefly."), ("user", "what broke?")]),
        "the first message of a conversation starts clean"
    );
    assert_eq!(
        models.conversation_prompt(1),
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
    assert_eq!(capability_listings(&mut observed), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_route_starts_from_an_empty_prompt_every_message() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(2, &["echo.echo"])).await;
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
        models.conversation_prompt(1),
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
async fn one_client_serves_every_message_routed_to_the_same_model() {
    // A model client owns an HTTP agent and its connection pool, so a client per message paid a
    // fresh TCP and TLS handshake before the first token of every answer. Sharing is only correct
    // because the prompt cache key is request-scoped, which the cache-key tests above pin down.
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(2, &["echo.echo"])).await;
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

    assert_eq!(models.requests(), 2, "both messages reached the model");
    assert_eq!(
        models.builds(),
        1,
        "the second message reused the first message's client"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_configured_models_never_share_one_client() {
    // The key is the configured name the loader already proved unique. Two endpoints sharing a
    // client would send one route's messages to the other's host.
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(2, &["echo.echo"])).await;
    let models = ModelScript::new([answer("from one"), answer("from the other")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    let mut second = model_config();
    if let ModelConfig::OpenaiCompatible { name, .. } = &mut second {
        *name = "another-endpoint".to_owned();
    }

    for model in [model_config(), second] {
        run_session(
            Arc::clone(&runner),
            route(model),
            message("who answers?"),
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }

    assert_eq!(
        models.builds(),
        2,
        "each configured model built its own client"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_senders_in_one_conversation_never_see_each_others_history() {
    // In a shared channel this is not a hypothetical. The admission key deliberately has no subject
    // in it; the history key deliberately does, and this is the difference that makes.
    const OTHER_SUBJECT: &str = "tel.16035550100";
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(3, &["echo.echo"])).await;
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
        models.conversation_prompt(1),
        transcript(&[("system", "Answer briefly."), ("user", "and mine?")]),
        "the second sender's first message must not carry the first sender's exchange"
    );
    assert_eq!(
        models.conversation_prompt(2),
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
    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo"), capability("gh.pr_view")],
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo")],
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
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
        models.conversation_prompt(1),
        transcript(&[("system", "Answer briefly."), ("user", "and now?")]),
        "a changed grant set starts a fresh conversation"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_grant_removes_the_conversation_rather_than_only_refusing_the_message() {
    let directory = temporary();
    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo")],
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
            ResponseEnvelope::capabilities(
                Vec::new(),
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
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
    // The fixed failure line is a delivery fact, never generated assistant text. Both the question
    // and the exact accepted warning survive so a retry can distinguish observation from generation.
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), listings(5, &["echo.echo"])).await;
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
        models.conversation_prompt(1),
        transcript(&[
            ("system", "Answer briefly."),
            ("user", "what broke?"),
            (
                "user",
                "[Exact transport-accepted text, distinct from generation]\nThe agent could not complete this request."
            ),
            ("user", "try again"),
        ]),
        "the accepted warning is a delivery fact, never generated assistant text"
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
    fn build(&self, _model: &ModelConfig) -> Result<SharedModel, SessionError> {
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

    let bound = persistent_route(model_config(), window());
    let inbound = message("what broke?");
    run_session(
        Arc::clone(&runner),
        bound.clone(),
        inbound.clone(),
        Arc::clone(&replier) as Arc<dyn ChatReplier>,
    )
    .await;

    assert_eq!(replier.replies(), vec![FAILURE_REPLY.to_owned()]);
    assert_eq!(
        runner.conversations.tracked(),
        1,
        "admission reserves one empty generation"
    );
    let seed = session_seed(
        &runner,
        &bound,
        &inbound,
        listings(1, &["echo.echo"]).remove(0),
    );
    assert!(
        seed.history.is_empty(),
        "pre-model failure commits no job, including seeded jobs"
    );
}

#[test]
fn an_idle_conversation_is_dropped_and_the_next_message_starts_fresh() {
    // The clock is a parameter because `tokio::time::pause` does not reach
    // `std::time::Instant::now()` inside a blocking task, so injecting it is the only way this is
    // deterministic rather than a sleep.
    let store = BoundedConversationStore::new(8);
    let key = conversation_key("dev", "dev", SUBJECT);
    let allowed = granted(&["echo.echo"]);
    let start = Instant::now();
    commit(
        &store,
        &key,
        &allowed,
        window(),
        JobRecord::completed("what broke?", "two things"),
        start,
    );

    let warm = store.begin(&key, &allowed, window(), start + Duration::from_secs(899));
    assert_eq!(
        warm.history.len(),
        1,
        "inside the timeout the exchange is replayed"
    );

    // Seeding a session is itself a touch — a message arrived and this history answered it — so
    // the timeout runs from the warm lookup, not from the last committed turn. That is also what
    // keeps a concurrent session's ceiling from evicting the conversation this one is answering.
    let cold = store.begin(&key, &allowed, window(), start + Duration::from_secs(900));
    assert_eq!(
        cold.history.len(),
        1,
        "the warm lookup at 899s reset the idle clock"
    );
    let cold = store.begin(
        &key,
        &allowed,
        window(),
        start + Duration::from_secs(900 + 900),
    );
    assert!(
        cold.history.is_empty(),
        "past the timeout the next message starts fresh"
    );
    assert_eq!(
        store.tracked(),
        1,
        "begin reserves a fresh empty generation after dropping the idle one"
    );
}

#[test]
fn the_conversation_ceiling_evicts_the_least_recently_used_rather_than_refusing() {
    // A person talking now matters more than one who stopped an hour ago, so a memory bound must
    // not become an admission bound.
    let store = BoundedConversationStore::new(2);
    let allowed = granted(&["echo.echo"]);
    let start = Instant::now();
    let keys = ["first", "second", "third"]
        .map(|conversation| conversation_key("dev", conversation, SUBJECT));
    let turn = |text: &str| JobRecord::completed(text, "noted");

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
    assert_eq!(
        store.begin(&keys[0], &allowed, window(), now).history.len(),
        2,
        "the conversation somebody is still having survives"
    );
    assert_eq!(
        store.begin(&keys[2], &allowed, window(), now).history.len(),
        1
    );
    assert!(
        store
            .begin(&keys[1], &allowed, window(), now)
            .history
            .is_empty(),
        "the least recently used conversation is the one that goes"
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
    // Portable structure and IDs count too, not just the question and answer text.
    let record_bytes = JobRecord::completed("question a", "an answer").bytes();
    let by_bytes = ConversationWindow {
        idle_timeout: Duration::from_secs(900),
        limits: HistoryLimits {
            max_turns: 12,
            max_bytes: record_bytes * 2,
        },
    };

    for (window, name) in [(by_turns, "turn bound"), (by_bytes, "byte bound")] {
        let store = BoundedConversationStore::new(8);
        let key = conversation_key("dev", "dev", SUBJECT);
        for text in ["question a", "question b", "question c"] {
            commit(
                &store,
                &key,
                &allowed,
                window,
                JobRecord::completed(text, "an answer"),
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
    let store = BoundedConversationStore::new(8);
    let key = conversation_key("dev", "dev", SUBJECT);
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
        JobRecord::completed("what broke?", "two things"),
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
    let store = BoundedConversationStore::new(8);
    let key = conversation_key("slack", "c0123abc:1700000000.000001", SUBJECT);
    let allowed = granted(&["echo.echo"]);
    let now = Instant::now();

    let first = store.begin(&key, &allowed, window(), now);
    let second = store.begin(&key, &allowed, window(), now);
    assert!(first.history.is_empty() && second.history.is_empty());

    store
        .commit(
            &key,
            &allowed,
            window(),
            JobRecord::completed("what broke?", "two things"),
            &first.cache_key,
            now,
        )
        .expect("live test lease");
    store
        .commit(
            &key,
            &allowed,
            window(),
            JobRecord::completed("still there?", "yes"),
            &second.cache_key,
            now,
        )
        .expect("live test lease");

    let resumed = store.begin(&key, &allowed, window(), now);
    assert_eq!(resumed.history.len(), 2);
    assert_eq!(resumed.history.turns()[0].user(), "what broke?");
    assert_eq!(resumed.history.turns()[1].user(), "still there?");
    // Two sessions opening one new conversation mint two lanes, and the one that created the entry
    // is the lane the conversation keeps. The loser paid for one cache lookup on one message; the
    // alternative — the last writer renaming the lane every message — would leave every request
    // naming a lane no earlier request had ever used.
    assert_eq!(first.cache_key, second.cache_key);
    assert_eq!(resumed.cache_key, first.cache_key);
}

#[test]
fn the_store_prints_counts_rather_than_conversations() {
    // `History` and `JobRecord` both derive `Debug`, so a derived impl here would put whole
    // conversations into the log stream on one `tracing::debug!(?store)`.
    let store = BoundedConversationStore::new(8);
    commit(
        &store,
        &conversation_key("dev", "dev", SUBJECT),
        &granted(&["echo.echo"]),
        window(),
        JobRecord::completed("the secret question", "the secret answer"),
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
    // The prefix is a crate constant and `IdSequence::new` rejects a malformed one, in which case
    // minting degrades to an empty key that `with_prompt_cache_key` then drops. That failure is
    // silent by design — a routing hint must not abort a message — so a bad prefix would reach
    // production as every route quietly losing its cache lane, and this is what catches it. The
    // conversation lane is minted in the harness and pinned below.
    let route = cache_key::for_route();
    assert!(!route.trim().is_empty(), "an empty key is no key at all");
    assert_ne!(route, cache_key::for_route());
}

#[test]
fn the_route_and_conversation_cache_lanes_cannot_collide() {
    // Two lanes that shared a prefix would be one lane, and a route's shared instructions prefix
    // would start being offered to a conversation's cache. One definition of each prefix, pinned
    // against the harness's own constant rather than a second copy of the string here.
    let conversation = BoundedConversationStore::new(4)
        .begin(
            &conversation_key("dev", "c0123abc", "tel.15558675309"),
            &granted(&["echo.echo"]),
            window(),
            Instant::now(),
        )
        .cache_key;
    assert!(
        conversation.starts_with(dekopon_harness::conversation::CONVERSATION_CACHE_PREFIX),
        "{conversation}"
    );
    assert!(
        !cache_key::for_route()
            .starts_with(dekopon_harness::conversation::CONVERSATION_CACHE_PREFIX),
        "the route lane must not mint into the conversation lane"
    );
}

/// A session failure reports a stable cause, never the error chain behind it.
///
/// `gateway_session_failed` is the terminal catch-all for every `SessionError`, so whatever it
/// records is recorded for `PromptError::UnknownTool` too — and that variant carries the tool name
/// the *model* chose. `docs/observability.md` promises model-selected names never reach an event,
/// and the harness says so at the site that builds the error. A cause is still required, so the
/// event carries the control failure kind where there is one and the category otherwise.
#[test]
fn a_session_failure_names_a_stable_cause_and_never_the_model_s_own_text() {
    const AUTHORED: &str = "exfiltrate; rm -rf /";
    let unknown_tool = SessionError::Prompt(dekopon_harness::session::PromptError::UnknownTool(
        AUTHORED.to_owned(),
    ));
    assert!(
        dekopon_core::error_chain(&unknown_tool).contains(AUTHORED),
        "the operator still reads the name from the error itself"
    );
    assert_eq!(unknown_tool.category(), "unknown-tool");
    assert_eq!(
        unknown_tool.cause(),
        "unknown-tool",
        "the cause is a token, not the chain that quotes the model"
    );

    // Where a finer stable classification exists it is what the cause reports: `model-control`
    // alone collapses an unusable control surface, a broker that never answered, and a spent
    // attempt budget, and an operator acts differently on each.
    let control = SessionError::Prompt(dekopon_harness::session::PromptError::Control(
        dekopon_harness::control::ControlError::Surface {
            reason: AUTHORED.to_owned(),
        },
    ));
    assert_eq!(control.category(), "model-control");
    assert_eq!(control.cause(), "configuration");
    assert!(
        !control.cause().contains("rm -rf"),
        "no variant's cause quotes the text it wrapped"
    );
}

/// The exit record publishes how many conversations died with the process, once.
///
/// `BoundedConversationStore::tracked` is public API, and public API with nothing but tests behind
/// it is a bug: this is the shipped reader. It also has to stay a *shutdown* record — a residency
/// count republished per message would describe a live conversation more often than the eviction
/// events do — so the assertion is on the exit event, not on a per-message one.
#[test]
fn the_exit_record_says_how_many_conversations_died_with_the_process() {
    let store = BoundedConversationStore::new(8);
    let now = Instant::now();
    for conversation in ["c0123abc", "c0456def"] {
        store.begin(
            &conversation_key("dev", conversation, SUBJECT),
            &granted(&["echo.echo"]),
            window(),
            now,
        );
    }

    let capture = dekopon_test_support::CaptureLayer::install();
    crate::gateway_stopped("shutdown", store.tracked());
    let events = capture.events_text();
    assert!(
        events.contains("gateway_stopped")
            && events.contains("reason=\"shutdown\"")
            && events.contains("conversations=2"),
        "the exit record carries the residency the store reported: {events}"
    );
    assert!(
        !events.contains("c0123abc") && !events.contains(SUBJECT),
        "it is a count, never a conversation or a sender: {events}"
    );
}

#[test]
fn a_cache_key_carries_nothing_about_the_sender() {
    // The whole reason the key is minted rather than derived. A canonical subject can be a phone
    // number, so sending it — or a hash of it, which is a stable pseudonym — would hand a model
    // provider the sender's identity in exchange for routing that happens either way.
    const DISTINCTIVE: &str = "tel.15558675309";
    let store = BoundedConversationStore::new(8);
    let key = conversation_key("dev", "c0123abc", DISTINCTIVE);
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
    let store = BoundedConversationStore::new(8);
    let key = conversation_key("dev", "dev", SUBJECT);
    let allowed = granted(&["echo.echo"]);
    let start = Instant::now();

    let first = store.begin(&key, &allowed, window(), start);
    store
        .commit(
            &key,
            &allowed,
            window(),
            JobRecord::completed("what broke?", "two things"),
            &first.cache_key,
            start,
        )
        .expect("live test lease");

    let warm = store.begin(&key, &allowed, window(), start + Duration::from_secs(60));
    assert_eq!(
        warm.cache_key, first.cache_key,
        "a live conversation stays in the lane its own turns warmed"
    );

    // The warm lookup at 60s is itself a touch, so the idle window runs from there.
    let cold = store.begin(&key, &allowed, window(), start + Duration::from_secs(960));
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
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(3, &["echo.echo"])).await;
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
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(3, &["echo.echo"])).await;
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
    fn build(&self, _model: &ModelConfig) -> Result<SharedModel, SessionError> {
        Ok(Arc::new(Self))
    }
}

impl ChatModel for KeylessModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        _tools: &[ModelTool],
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let result: Result<AssistantTurn, ModelError> = {
                Ok(answer(
                    messages
                        .last()
                        .and_then(ModelMessage::content)
                        .unwrap_or_default(),
                ))
            };
            if let Ok(turn) = &result
                && let Some(usage) = turn.usage
            {
                recorder.observe(
                    attempt,
                    dekopon_model::usage::UsageObservation {
                        usage,
                        invalid: [false; 5],
                    },
                )?;
            }
            result
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_never_heard_of_a_cache_key_still_answers() {
    // `complete_with` is a provided method precisely so this keeps working: an implementation that
    // ignores the options loses a cache lookup, never an answer.
    let directory = temporary();
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(1, &["echo.echo"])).await;
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
async fn a_transport_owned_thread_continuation_bypasses_only_the_repeat_mention() {
    let directory = temporary();
    let mut document = document(directory.path());
    document["routes"][0]["match"] = json!({"kind": "channel", "channel": "c0123abc"});
    let config = resolved(directory.path(), &document).await;
    let routes = Arc::new(
        RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).expect("route binds"),
    );
    let (broker, _observed) =
        stub_broker_with_checks(3, directory.path(), listings(1, &["echo.echo"])).await;
    let models = ModelScript::new([answer("Useful follow-up.")]);
    let runner = runner(broker, Arc::clone(&models), 4);
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
    let mut continuation = message("and then?");
    continuation.conversation = ConversationKind::Channel("c0123abc".to_owned());
    continuation.addressed = Some(false);
    continuation.thread_continuation = Some(slack_thread_continuation(true));

    crate::dispatch(
        &runner,
        &routes,
        &identities,
        &repliers,
        &mut sessions,
        continuation,
    );
    assert_eq!(
        sessions.len(),
        1,
        "the owned continuation starts one session"
    );
    while let Some(result) = sessions.join_next().await {
        result.expect("continuation session completes");
    }

    assert_eq!(models.requests(), 1);
    assert_eq!(replier.replies(), ["Useful follow-up."]);
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
    headers: Arc<Mutex<Vec<String>>>,
}

impl HttpMock {
    /// Paths and request bodies the transport sent, in order.
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("mock call log").clone()
    }

    fn headers(&self) -> Vec<String> {
        self.headers.lock().expect("mock header log").clone()
    }
}

/// Serves loopback HTTP until the test drops, answering through `handler`.
#[allow(
    clippy::let_underscore_must_use,
    reason = "a mock that cannot finish writing its canned response leaves the transport under \
              test without one, which is what the calling test already asserts on"
)]
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
    let headers = Arc::new(Mutex::new(Vec::new()));
    let recorded_headers = Arc::clone(&headers);
    tokio::spawn(async move {
        let handler = Arc::new(handler);
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let handler = Arc::clone(&handler);
            let recorded = Arc::clone(&recorded);
            let recorded_headers = Arc::clone(&recorded_headers);
            tokio::spawn(async move {
                let mut stream = stream;
                let Some((path, headers, body)) = read_http_request_parts(&mut stream).await else {
                    return;
                };
                recorded
                    .lock()
                    .expect("mock call log")
                    .push((path.clone(), body.clone()));
                recorded_headers
                    .lock()
                    .expect("mock header log")
                    .push(headers);
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
        headers,
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

#[allow(
    clippy::let_underscore_must_use,
    reason = "a mock that cannot finish writing its canned response leaves the transport under \
              test without one, which is what the calling test already asserts on"
)]
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
    // Multipart image bodies contain arbitrary bytes. Lossy rendering preserves every ASCII
    // boundary/header/JSON field tests assert on while still letting the mock answer a PNG upload.
    let body = String::from_utf8_lossy(&bytes[header_end..]).into_owned();
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
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "the acknowledgement channel is unbounded, so a send fails only once \
                              the test dropped its receiver and stopped caring"
                )]
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

fn app_mention(user: &str, ts: &str, thread_ts: Option<&str>, text: &str) -> Value {
    let mut event = channel_message(user, ts, thread_ts, text);
    event["type"] = json!("app_mention");
    event
}

/// One Telegram transport pointed at loopback mocks, with activity presentation off.
fn telegram(endpoint: &str) -> crate::transport::telegram::TelegramTransport {
    telegram_with(endpoint, ActivityMode::Off)
}

fn telegram_with(
    endpoint: &str,
    activity: ActivityMode,
) -> crate::transport::telegram::TelegramTransport {
    crate::transport::telegram::TelegramTransport::new(
        "tg".to_owned(),
        endpoint.to_owned(),
        "12345:test-token".to_owned(),
        activity,
    )
    .expect("telegram transport builds")
}

/// The Bot API mock every polling test needs: one fixed `getMe`, one batch of updates on the
/// first poll, and an empty result after it.
fn telegram_handler(updates: Vec<Value>) -> impl Fn(&str, &str) -> Value + Send + Sync + 'static {
    move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("offset=0") {
            return json!({"ok": true, "result": updates.clone()});
        }
        json!({"ok": true, "result": []})
    }
}

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

    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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
async fn slack_agent_continues_only_an_exact_claimed_sender_thread() {
    let root = "1700000000.000001";
    let socket = spawn_socket_mock(vec![
        // Channel-history scopes deliver this ambient top-level message. It must disappear inside
        // the transport before routing, authorization, telemetry payloads, or a model.
        events_envelope(
            "envelope-ambient",
            channel_message("u9xyz", "1700000000.000000", None, "ambient"),
        ),
        events_envelope(
            "envelope-opening",
            app_mention("u9xyz", root, None, "<@u0botbot> start here"),
        ),
        events_envelope(
            "envelope-owned",
            channel_message("u9xyz", "1700000000.000002", Some(root), "and then?"),
        ),
        events_envelope(
            "envelope-revoked",
            channel_message("u9xyz", "1700000000.000003", Some(root), "still there?"),
        ),
        events_envelope(
            "envelope-other-user",
            channel_message("u8other", "1700000000.000004", Some(root), "I am chatting"),
        ),
        events_envelope(
            "envelope-other-thread",
            channel_message(
                "u9xyz",
                "1700000000.000005",
                Some("1699999999.000009"),
                "another thread",
            ),
        ),
        events_envelope(
            "envelope-explicit",
            app_mention(
                "u9xyz",
                "1700000000.000006",
                Some(root),
                "<@u0botbot> explicit again",
            ),
        ),
    ]);
    let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
    let mut transport = slack_with(
        &http.base,
        SlackExperience::Agent,
        SlackActivityConfig::default(),
    );
    transport.connect().await.expect("Slack Agent connects");

    let opening = next_message(&mut transport).await;
    let opening_continuation = opening
        .thread_continuation
        .expect("an explicit Agent channel message proposes a claim");
    assert!(!opening_continuation.inherited);
    assert_eq!(opening.addressed, Some(true));

    let ownership = transport
        .thread_ownership()
        .expect("Agent transport owns a bounded thread registry");
    ownership.claim(opening_continuation.claim.clone());
    let inherited = next_message(&mut transport).await;
    assert_eq!(inherited.text, "and then?");
    assert_eq!(inherited.addressed, Some(false));
    assert!(
        inherited
            .thread_continuation
            .as_ref()
            .is_some_and(|continuation| continuation.inherited)
    );

    ownership.revoke(&opening_continuation.claim);
    let explicit = next_message(&mut transport).await;
    assert_eq!(explicit.text, "<@u0botbot> explicit again");
    assert_eq!(explicit.addressed, Some(true));
    assert!(
        explicit
            .thread_continuation
            .as_ref()
            .is_some_and(|continuation| !continuation.inherited)
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
            progress_message: false,
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
            progress_message: false,
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
            progress_message: false,
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
            progress_message: false,
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

    let (broker, _observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("echo.echo")],
            Vec::new(),
            "fixture-epoch".parse().expect("fixture epoch"),
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

#[tokio::test(flavor = "multi_thread")]
async fn slack_uploads_one_generated_png_without_sending_the_token_to_the_upload_url() {
    let base = Arc::new(Mutex::new(String::new()));
    let response_base = Arc::clone(&base);
    let api = spawn_http_mock(move |path, _body| match path {
        "/api/files.getUploadURLExternal" => json!({
            "ok": true,
            "upload_url": format!("{}/upload", response_base.lock().expect("base lock")),
            "file_id": "f-generated"
        }),
        "/upload" => json!({"uploaded": true}),
        "/api/files.completeUploadExternal" => {
            json!({"ok": true, "files": [{"id": "f-generated"}]})
        }
        other => panic!("unexpected Slack image call: {other}"),
    });
    *base.lock().expect("base lock") = api.base.clone();
    let replier = slack(&api.base).replier();

    let receipt = replier
        .reply(
            ReplyTarget::Slack {
                channel: "d0123abc".to_owned(),
                thread_ts: Some("1712345678.000100".to_owned()),
            },
            OutboundReply::with_image("Here is your kitty.", generated_image()),
        )
        .await
        .expect("the complete file share is accepted");
    assert!(receipt.accepted());

    let calls = api.calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].1.contains("filename=generated-image.png"));
    assert!(calls[0].1.contains("length=20"));
    assert!(calls[1].1.contains("kitty pixels"));
    let completed: Value = serde_json::from_str(&calls[2].1).expect("completion JSON");
    assert_eq!(completed["channel_id"], "d0123abc");
    assert_eq!(completed["thread_ts"], "1712345678.000100");
    assert_eq!(completed["initial_comment"], "Here is your kitty.");
    let headers = api.headers();
    assert_eq!(headers.len(), 3);
    assert!(
        !headers[1].to_ascii_lowercase().contains("authorization:"),
        "the service-selected upload URL must never receive the bot token"
    );
}

#[test]
fn slack_generated_upload_urls_are_origin_bounded() {
    use crate::transport::slack::is_slack_upload_url;

    assert!(is_slack_upload_url(
        "https://files.slack.com/upload/v1/abc",
        config::SLACK_ENDPOINT
    ));
    assert!(!is_slack_upload_url(
        "https://files.slack.com.evil.test/upload/v1/abc",
        config::SLACK_ENDPOINT
    ));
    assert!(!is_slack_upload_url(
        "https://files.slack.com@evil.test/upload/v1/abc",
        config::SLACK_ENDPOINT
    ));
    assert!(is_slack_upload_url(
        "http://127.0.0.1:9000/upload",
        "http://127.0.0.1:9000"
    ));
    assert!(!is_slack_upload_url(
        "http://127.0.0.1:9001/upload",
        "http://127.0.0.1:9000"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_and_telegram_never_issue_receipts_for_non_success_http_statuses() {
    let slack_http = spawn_raw_http_mock(|_| {
        (
            500,
            "application/json",
            br#"{"ok":true,"channel":"d0123abc","ts":"1712345678.000100"}"#.to_vec(),
        )
    });
    let slack = slack(&slack_http.base).replier();
    assert!(
        slack
            .reply(
                ReplyTarget::Slack {
                    channel: "d0123abc".to_owned(),
                    thread_ts: None,
                },
                OutboundReply::text("answer"),
            )
            .await
            .is_err()
    );

    let telegram_http = spawn_raw_http_mock(|_| {
        (
            500,
            "application/json",
            br#"{"ok":true,"result":{"message_id":7,"chat":{"id":42}}}"#.to_vec(),
        )
    });
    let telegram = telegram(&telegram_http.base).replier();
    assert!(
        telegram
            .reply(
                ReplyTarget::Telegram {
                    chat_id: 42,
                    reply_to: None,
                    message_thread_id: None,
                },
                OutboundReply::text("answer"),
            )
            .await
            .is_err()
    );
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
            #[allow(
                clippy::let_underscore_must_use,
                reason = "the comment above is the point: each of these four is expected to fail, \
                          and only the fifth call's refusal is asserted on"
            )]
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

#[allow(
    clippy::let_underscore_must_use,
    reason = "the observation channel is unbounded and the heartbeat acknowledgement goes back \
              over a socket the transport under test is reading; a test that needed either one \
              fails waiting for it"
)]
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
        path if path.starts_with("/api/v10/channels/") => json!({
            "id": "444444444444444444",
            "channel_id": DISCORD_CHANNEL,
        }),
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
        .reply(message.reply, OutboundReply::text("@everyone **done**"))
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
async fn discord_posts_generated_png_as_a_bounded_multipart_attachment() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let http = spawn_http_mock(move |path, _body| {
        assert_eq!(path, "/api/v10/channels/222222222222222222/messages");
        if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            json!({
                "id": "444444444444444444",
                "channel_id": DISCORD_CHANNEL,
                "attachments": [{
                    "id": "555555555555555555",
                    "filename": "generated-image.png"
                }]
            })
        } else {
            // A long answer needs a second text-only message. Failing after the image-bearing first
            // post must be reported as partial rather than as no delivery.
            json!({})
        }
    });
    let transport = discord(&http.base);

    let error = transport
        .replier()
        .reply(
            ReplyTarget::Discord {
                channel_id: DISCORD_CHANNEL.to_owned(),
                reply_to: Some(DISCORD_MESSAGE.to_owned()),
            },
            OutboundReply::with_image("x".repeat(3_000), generated_image()),
        )
        .await
        .expect_err("the second chunk fails after the image was accepted");
    assert!(matches!(error, TransportError::PartialDelivery));

    let calls = http.calls();
    assert_eq!(calls.len(), 2);
    let multipart = &calls[0].1;
    assert!(multipart.contains("name=\"payload_json\""));
    assert!(multipart.contains("name=\"files[0]\""));
    assert!(multipart.contains("filename=\"generated-image.png\""));
    assert!(multipart.contains("kitty pixels"));
    assert!(multipart.contains("\"attachments\""));
    assert!(multipart.contains(DISCORD_MESSAGE));
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
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&json!({
                        "id": "444444444444444444",
                        "channel_id": DISCORD_CHANNEL,
                    }))
                    .expect("Discord response serializes"),
                )
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
            OutboundReply::text("after a short rate limit"),
        )
        .await
        .expect("the bounded retry succeeds");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discord_rate_limit_wait_releases_the_rest_lock() {
    // The lock is there to serialize requests, not to serialize waiting. Held across the 429 sleep,
    // one throttled reply stalled every other session's answer and any model waiting on
    // `fetch_chat_asset` for as long as Discord asked this one reply to wait.
    let cdn = spawn_raw_http_mock(|path| match path {
        "/fresh/document" => (200, "application/pdf", b"fresh pdf bytes".to_vec()),
        _ => (404, "application/json", br#"{"code":404}"#.to_vec()),
    });
    let channel_id = "200000000000000007";
    let message_id = "300000000000000007";
    let attachment_id = "400000000000000007";
    let mut event = discord_message(
        message_id,
        channel_id,
        None,
        DISCORD_USER,
        false,
        "read this",
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
    let posts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&posts);
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
        "/api/v10/channels/200000000000000007/messages" => {
            // One second is long enough that the refresh below could not have finished after it by
            // accident, and short enough to keep the test quick.
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                (
                    429,
                    "application/json",
                    br#"{"retry_after":1.0,"global":false}"#.to_vec(),
                )
            } else {
                (
                    200,
                    "application/json",
                    serde_json::to_vec(&json!({
                        "id": "444444444444444444",
                        "channel_id": "200000000000000007",
                    }))
                    .expect("Create Message response serializes"),
                )
            }
        }
        "/api/v10/channels/200000000000000007/messages/300000000000000007" => (
            200,
            "application/json",
            serde_json::to_vec(&json!({
                "id": "300000000000000007",
                "attachments": [{"id": "400000000000000007", "url": fresh_url}]
            }))
            .expect("message response serializes"),
        ),
        _ => (404, "application/json", br#"{"code":10008}"#.to_vec()),
    });
    let mut transport = discord(&http.base);
    transport.connect().await.expect("Discord connects");
    let message = next_message(&mut transport).await;
    let source = message.assets[0]
        .source
        .clone()
        .expect("attachment has a source");
    let replier = transport.replier();
    let fetcher = transport
        .asset_fetcher()
        .expect("Discord has an asset fetcher");

    let reply = tokio::spawn(async move {
        replier
            .reply(
                ReplyTarget::Discord {
                    channel_id: "200000000000000007".to_owned(),
                    reply_to: None,
                },
                OutboundReply::text("throttled"),
            )
            .await
            .expect("the bounded retry still succeeds");
        Instant::now()
    });
    // The wait only exists once Discord has answered 429, so the refresh has to start after that.
    while posts.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let bytes = fetcher
        .fetch(&source, 1024)
        .await
        .expect("the expired URL is refreshed while the reply is waiting out its rate limit");
    let refreshed = Instant::now();
    assert_eq!(bytes, b"fresh pdf bytes");
    let replied = reply.await.expect("the reply task finishes");
    assert!(
        refreshed < replied,
        "the attachment refresh must not queue behind the reply's rate-limit sleep"
    );
    assert_eq!(
        posts.load(Ordering::SeqCst),
        2,
        "the reply was retried once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_failure_after_one_accepted_chunk_is_partial_delivery() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let http = spawn_raw_http_mock(move |path| {
        if path == "/api/v10/channels/222222222222222222/messages" {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                return (
                    200,
                    "application/json",
                    serde_json::to_vec(&json!({
                        "id": "444444444444444444",
                        "channel_id": DISCORD_CHANNEL,
                    }))
                    .expect("response serializes"),
                );
            }
            return (500, "application/json", br#"{"code":500}"#.to_vec());
        }
        (404, "application/json", br#"{"code":404}"#.to_vec())
    });
    let transport = discord(&http.base);
    let error = transport
        .replier()
        .reply(
            ReplyTarget::Discord {
                channel_id: DISCORD_CHANNEL.to_owned(),
                reply_to: None,
            },
            OutboundReply::text("x".repeat(3_000)),
        )
        .await
        .expect_err("the second chunk fails after the first was accepted");
    assert!(matches!(error, TransportError::PartialDelivery));
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
    let http = spawn_http_mock(telegram_handler(vec![
        json!({"update_id": 100, "message": telegram_message(7, true, 1, "a bot said this")}),
        json!({"update_id": 101, "message": telegram_message(16034700182_i64, false, 2, "a person asked this")}),
    ]));

    let mut transport = telegram(&http.base);
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
    let http = spawn_http_mock(telegram_handler(vec![json!({
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
    })]));

    let mut transport = telegram(&http.base);
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
    let http = spawn_http_mock(telegram_handler(vec![json!({
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
    })]));

    let mut transport = telegram(&http.base);
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
        if path.contains("sendChatAction") {
            return json!({"ok": true, "result": true});
        }
        if path.contains("sendMessage") {
            return json!({
                "ok": true,
                "result": {
                    "message_id": 12,
                    "message_thread_id": 99,
                    "chat": {"id": -1001}
                }
            });
        }
        json!({"ok": true, "result": []})
    });
    let mut transport = telegram_with(&http.base, ActivityMode::Native);
    transport.connect().await.expect("Telegram connects");
    let message = next_message(&mut transport).await;

    assert_eq!(message.thread.as_deref(), Some("99"));
    assert_eq!(message.conversation_id, "-1001:topic:99");
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
        .reply(message.reply, OutboundReply::text("done"))
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
async fn a_long_telegram_answer_is_split_instead_of_being_rejected_whole() {
    // `sendMessage` refuses text over 4,096 UTF-16 units, and the gateway's own outbound bound is
    // twice that. Before splitting, an ordinary long answer was rejected in full and the person who
    // asked heard nothing at all.
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("offset=0") {
            return json!({"ok": true, "result": [
                {"update_id": 400, "message": telegram_chat_message(-1001, "supergroup", 16034700182_i64, false, 21, "@dekopon_bot summarize")}
            ]});
        }
        if path.contains("sendMessage") {
            // Every chunk is acknowledged as its own message: the receipt is minted from the
            // service's answer, so a bare `true` is no longer an accepted delivery.
            return json!({"ok": true, "result": {"message_id": 7, "chat": {"id": -1001}}});
        }
        json!({"ok": true, "result": []})
    });
    let mut transport = telegram(&http.base);
    transport.connect().await.expect("Telegram connects");
    let message = next_message(&mut transport).await;

    // Astral characters are the case a scalar-value count gets wrong: 3,000 crabs are 6,000 UTF-16
    // code units, so a splitter counting characters would post one message Telegram refuses.
    let long = format!("{}\n{}", "a".repeat(2_000), "🦀".repeat(3_000));
    transport
        .replier()
        .reply(message.reply, OutboundReply::text(long.clone()))
        .await
        .expect("a long answer is delivered");

    let sent = http
        .calls()
        .into_iter()
        .filter(|(path, _)| path.contains("sendMessage"))
        .map(|(_, body)| serde_json::from_str::<Value>(&body).expect("reply body is JSON"))
        .collect::<Vec<_>>();
    assert!(sent.len() > 1, "the answer needed more than one message");
    assert!(
        sent.iter().all(|body| body["text"]
            .as_str()
            .expect("each chunk carries text")
            .encode_utf16()
            .count()
            <= 4_096),
        "every chunk is inside Telegram's UTF-16 ceiling"
    );
    let rejoined = sent
        .iter()
        .map(|body| body["text"].as_str().unwrap_or_default())
        .collect::<String>();
    assert_eq!(
        rejoined, long,
        "splitting loses nothing and reorders nothing"
    );
    assert_eq!(
        sent[0]["reply_to_message_id"], 21,
        "the first chunk quotes the message it answers"
    );
    assert!(
        sent[1..]
            .iter()
            .all(|body| body["reply_to_message_id"].is_null()),
        "a continuation must not draw a second reply arrow"
    );
}

#[test]
fn a_chunk_never_splits_a_character_and_prefers_a_line_break() {
    // The ceiling is in UTF-16 code units, and a chunk boundary is still a character boundary: an
    // astral character straddling one would be two halves of a replacement glyph in both clients.
    let text = format!("{}\n{}", "a".repeat(100), "🦀".repeat(2_048));
    let chunks = crate::transport::split_message(&text, 4_096, crate::transport::TextUnit::Utf16);

    assert_eq!(chunks.len(), 2);
    assert_eq!(
        chunks[0],
        format!("{}\n", "a".repeat(100)),
        "the split fell on the newline rather than mid-line"
    );
    assert_eq!(chunks.concat(), text);
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.encode_utf16().count() <= 4_096)
    );
}

#[test]
fn an_empty_answer_still_becomes_one_post() {
    // Every chat service refuses an empty message. Saying so is better than a silent failure the
    // sender reads as the bot ignoring them.
    assert_eq!(
        crate::transport::split_message("", 4_096, crate::transport::TextUnit::Utf16),
        vec!["[empty response]".to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_chat_is_one_conversation_and_another_chat_is_another() {
    // The Bot API puts no thread identifier on a plain message, so a conversation collapses to its
    // chat: consecutive messages continue one exchange, and a group is not the private chat.
    let http = spawn_http_mock(telegram_handler(vec![
        json!({"update_id": 200, "message": telegram_message(16034700182_i64, false, 1, "first")}),
        json!({"update_id": 201, "message": telegram_message(16034700182_i64, false, 2, "second")}),
        json!({"update_id": 202, "message": telegram_chat_message(-1001, "supergroup", 16034700182_i64, false, 3, "over here")}),
    ]));

    let mut transport = telegram(&http.base);
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

#[tokio::test(flavor = "multi_thread")]
async fn telegram_topics_have_distinct_scopes_and_replies_stay_in_the_topic() {
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("getMe") {
            return json!({"ok": true, "result": {"id": 1, "is_bot": true, "username": "dekopon_bot"}});
        }
        if path.contains("getUpdates") && path.contains("offset=0") {
            let mut message = telegram_chat_message(
                -1001,
                "supergroup",
                16034700182_i64,
                false,
                3,
                "topic question",
            );
            message["message_thread_id"] = json!(77);
            return json!({"ok": true, "result": [{"update_id": 500, "message": message}]});
        }
        if path.contains("sendMessage") {
            return json!({
                "ok": true,
                "result": {
                    "message_id": 4,
                    "message_thread_id": 77,
                    "chat": {"id": -1001, "type": "supergroup"}
                }
            });
        }
        json!({"ok": true, "result": []})
    });
    let mut transport = telegram(&http.base);
    transport.connect().await.expect("telegram connects");
    let message = next_message(&mut transport).await;
    assert_eq!(message.conversation_id, "-1001:topic:77");
    assert_eq!(message.thread.as_deref(), Some("77"));
    transport
        .replier()
        .reply(message.reply, OutboundReply::text("inside topic"))
        .await
        .expect("topic reply is accepted");
    let body = http
        .calls()
        .into_iter()
        .find_map(|(path, body)| path.contains("sendMessage").then_some(body))
        .expect("sendMessage request");
    let body: Value = serde_json::from_str(&body).expect("request JSON");
    assert_eq!(body["chat_id"], -1001);
    assert_eq!(body["message_thread_id"], 77);
    assert_eq!(body["reply_to_message_id"], 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_sends_a_generated_png_as_a_photo_in_the_authenticated_topic() {
    let http = spawn_http_mock(|path, _body| {
        assert!(path.contains("sendPhoto"));
        json!({
            "ok": true,
            "result": {
                "message_id": 12,
                "message_thread_id": 77,
                "chat": {"id": -1001},
                "photo": [{"file_id": "photo-small"}, {"file_id": "photo-large"}]
            }
        })
    });
    let transport = telegram(&http.base);

    transport
        .replier()
        .reply(
            ReplyTarget::Telegram {
                chat_id: -1001,
                reply_to: Some(3),
                message_thread_id: Some(77),
            },
            OutboundReply::with_image("Here is your kitty.", generated_image()),
        )
        .await
        .expect("photo and caption are accepted together");

    let calls = http.calls();
    assert_eq!(calls.len(), 1);
    let multipart = &calls[0].1;
    assert!(multipart.contains("name=\"photo\""));
    assert!(multipart.contains("filename=\"generated-image.png\""));
    assert!(multipart.contains("kitty pixels"));
    assert!(multipart.contains("Here is your kitty."));
    assert!(multipart.contains("name=\"reply_parameters\""));
    assert!(multipart.contains("\"message_id\":3"));
    assert!(multipart.contains("-1001"));
    assert!(multipart.contains("77"));
    assert!(multipart.contains("3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_splits_long_generated_image_text_without_losing_it() {
    let message_ids = Arc::new(AtomicUsize::new(20));
    let next_id = Arc::clone(&message_ids);
    let http = spawn_http_mock(move |path, _body| {
        if path.contains("sendPhoto") {
            json!({
                "ok": true,
                "result": {
                    "message_id": 12,
                    "chat": {"id": 42},
                    "photo": [{"file_id": "photo"}]
                }
            })
        } else {
            json!({
                "ok": true,
                "result": {
                    "message_id": next_id.fetch_add(1, Ordering::SeqCst),
                    "chat": {"id": 42}
                }
            })
        }
    });
    let transport = telegram(&http.base);
    let text = format!("{}\n{}", "a".repeat(4_000), "b".repeat(1_000));

    transport
        .replier()
        .reply(
            ReplyTarget::Telegram {
                chat_id: 42,
                reply_to: Some(3),
                message_thread_id: None,
            },
            OutboundReply::with_image(text.clone(), generated_image()),
        )
        .await
        .expect("photo and every bounded text chunk are accepted");

    let calls = http.calls();
    assert_eq!(calls.len(), 3, "one photo plus two text chunks");
    let delivered = calls[1..]
        .iter()
        .map(|(_, body)| {
            serde_json::from_str::<Value>(body).expect("text request JSON")["text"]
                .as_str()
                .expect("text field")
                .to_owned()
        })
        .collect::<String>();
    assert_eq!(delivered, text);
    assert!(
        calls[1..]
            .iter()
            .all(|(_, body)| !body.contains("reply_to_message_id")),
        "the photo already owns the inbound reply reference"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_reports_partial_delivery_when_long_image_text_fails_after_the_photo() {
    let http = spawn_http_mock(|path, _body| {
        if path.contains("sendPhoto") {
            json!({
                "ok": true,
                "result": {
                    "message_id": 12,
                    "chat": {"id": 42},
                    "photo": [{"file_id": "photo"}]
                }
            })
        } else {
            json!({"ok": false, "description": "message rejected"})
        }
    });
    let transport = telegram(&http.base);

    let error = transport
        .replier()
        .reply(
            ReplyTarget::Telegram {
                chat_id: 42,
                reply_to: None,
                message_thread_id: None,
            },
            OutboundReply::with_image("x".repeat(1_025), generated_image()),
        )
        .await
        .expect_err("the photo succeeded before the text failed");
    assert!(matches!(error, TransportError::PartialDelivery));
    assert_eq!(http.calls().len(), 2);
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
    assert_eq!(
        second.text, "second",
        "two requests on one connection are still two messages"
    );
    let parts = first.message_id.split('-').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].len(), 32, "a 128-bit boot nonce prefixes every ID");
    assert!(
        parts[0]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(named.conversation_id, "session-7");

    // The receipt resolves only after the transport writer has completed both `write_all` and
    // `flush`; reading the exact line from the peer proves the kernel-acceptance crossing.
    let reply = tokio::spawn({
        let replier = transport.replier();
        let target = first.reply.clone();
        async move {
            replier
                .reply(target, OutboundReply::text("accepted locally"))
                .await
        }
    });
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    let mut client = BufReader::new(client);
    let mut line = String::new();
    client
        .read_line(&mut line)
        .await
        .expect("the flushed reply reaches the local caller");
    let text_response = serde_json::from_str::<Value>(&line).expect("local reply is JSON");
    assert_eq!(text_response["reply"], "accepted locally");
    assert!(
        text_response.get("images").is_none(),
        "text-only local replies keep their exact legacy shape"
    );
    assert!(
        reply
            .await
            .expect("reply task completes")
            .expect("local write and flush are accepted")
            .accepted()
    );

    let image_reply = tokio::spawn({
        let replier = transport.replier();
        let target = second.reply.clone();
        async move {
            replier
                .reply(
                    target,
                    OutboundReply::with_image("a local kitty", generated_image()),
                )
                .await
        }
    });
    line.clear();
    client
        .read_line(&mut line)
        .await
        .expect("the generated image reaches the local caller");
    let response = serde_json::from_str::<Value>(&line).expect("image reply is JSON");
    assert_eq!(response["reply"], "a local kitty");
    assert_eq!(response["images"][0]["filename"], "generated-image.png");
    assert_eq!(response["images"][0]["mediaType"], "image/png");
    assert_eq!(
        STANDARD
            .decode(
                response["images"][0]["data"]
                    .as_str()
                    .expect("base64 image")
            )
            .expect("image data decodes"),
        generated_image().bytes()
    );
    assert!(
        image_reply
            .await
            .expect("image reply task completes")
            .expect("image write and flush are accepted")
            .accepted()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_uses_each_fresh_chat_surface_before_request_one_without_discovery() {
    let directory = temporary();
    let mut first_capability = capability("echo.echo");
    first_capability.capability.description = "Return the specified message".to_owned();
    first_capability.capability.input_schema = json!({"type":"object", "properties":{"message":{"type":"string"}}, "required":["message"], "additionalProperties":false});
    let schema = first_capability.capability.input_schema.clone();
    let (broker, mut observed) = stub_broker_with_checks(
        3,
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![first_capability, capability("private.first-only")],
                vec!["echo-cli".to_owned()],
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
            ResponseEnvelope::capabilities(
                vec![capability("echo.echo")],
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
        ],
    )
    .await;
    let models = ModelScript::new([answer("First answer."), answer("Second answer.")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(broker, Arc::clone(&models), 4);
    for text in ["first", "second"] {
        run_session(
            Arc::clone(&runner),
            route(model_config()),
            message(text),
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }
    assert_eq!(
        models.requests(),
        2,
        "one inference per inbound job, no discovery call"
    );
    for index in 0..2 {
        let messages = models.prompt(index);
        assert!(
            messages
                .iter()
                .all(|(role, _)| role == "system" || role == "user"),
            "no fabricated tool or assistant messages"
        );
        let context = messages
            .iter()
            .find(|(_, text)| text.starts_with("Dekopon session bootstrap\n"))
            .expect("bootstrap context");
        let document: Value =
            serde_json::from_str(context.1.lines().last().expect("metadata JSON"))
                .expect("bootstrap JSON");
        assert_eq!(document["selectedModel"], "qwen3");
        assert!(!context.1.contains(SUBJECT));
        assert!(!context.1.contains("http://127.0.0.1:1/v1"));
        assert_eq!(document["capabilities"][0]["id"], "echo.echo");
        if index == 0 {
            assert_eq!(
                document["capabilities"][0]["description"],
                "Return the specified message"
            );
            assert_eq!(document["capabilities"][0]["inputSchema"], schema);
            assert_eq!(document["commandWords"], json!(["echo-cli"]));
        } else {
            assert!(
                !context.1.contains("private.first-only"),
                "fresh narrowed grant replaces old metadata"
            );
            assert!(!context.1.contains("echo-cli"));
            assert_eq!(
                document["capabilities"]
                    .as_array()
                    .expect("capabilities")
                    .len(),
                1
            );
        }
        assert!(matches!(
            observed.recv().await.expect("one snapshot per job").request,
            BrokerRequest::Capabilities {
                attestation: Some(Attestation { scope: Some(_), .. })
            }
        ));
    }
    assert_surface_checks(&mut observed, 4);
    assert!(
        observed.try_recv().is_err(),
        "bootstrap runs no discovery commands or broker invocations"
    );
    assert_eq!(replier.replies(), ["First answer.", "Second answer."]);
}

fn conversation_key(transport: &str, conversation: &str, subject: &str) -> ConversationKey {
    ConversationKey::scoped(
        "test-agent",
        "test-route",
        transport,
        "test-channel",
        conversation,
        subject,
    )
}

#[tokio::test]
async fn configured_controls_validation_is_strict_aggregate_and_resolves_all_candidates() {
    let directory = temporary();
    let mut doc = document(directory.path());
    doc["routes"][0]["controls"] =
        json!({"models":["local-qwen","local-qwen","missing"],"maxAttempts":9});
    let error = load(directory.path(), &doc).await.unwrap_err();
    let text = error.to_string();
    assert!(text.contains("duplicate control model local-qwen"));
    assert!(text.contains("maxAttempts"));
    assert!(text.contains("missing"));
    doc["routes"][0]["controls"] = json!({"models":["local-qwen"],"endpoint":"https://invalid"});
    assert!(load(directory.path(), &doc).await.is_err());
    doc["routes"][0]["controls"] = json!({"models":["local-qwen"],"maxAttempts":2});
    doc["models"][0]["effort"] = json!("high");
    let config = load(directory.path(), &doc).await.unwrap();
    assert_eq!(config.models[0].effort(), dekopon_core::Effort::High);
    assert_eq!(config.routes[0].controls.as_ref().unwrap().max_attempts, 2);
    doc["models"][0]["effort"] = json!("extreme");
    assert!(load(directory.path(), &doc).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_controls_reuse_gateway_clients_and_authorize_each_job_from_its_baseline() {
    use dekopon_broker_protocol::{ControlDecision, ControlOutcome, ProtocolVersion};
    let directory = temporary();
    let path = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let (send, mut recv) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for _ in 0..12 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream, FrameLimits::default())
                .await
                .unwrap();
            let response = match request.request {
                BrokerRequest::Capabilities { .. } => ResponseEnvelope::capabilities(
                    vec![capability("echo.echo")],
                    vec![],
                    "fixture-epoch".parse().unwrap(),
                ),
                BrokerRequest::AuthorizeControl {
                    proposal,
                    attestation,
                } => {
                    let claim = attestation.as_ref().unwrap();
                    assert!(claim.scope.is_some());
                    assert_eq!(claim.agent, proposal.scope.agent);
                    assert_eq!(claim.invocation.as_ref(), Some(&proposal.id));
                    send.send(proposal.clone()).unwrap();
                    ResponseEnvelope {
                        api_version: ProtocolVersion::V1Alpha3,
                        response: dekopon_broker_protocol::BrokerResponse::ControlDecision {
                            decision: Box::new(ControlDecision {
                                proposal,
                                attestation,
                                surface_epoch: "fixture-epoch".parse().unwrap(),
                                decision_ref: format!("sha256:{}", "1".repeat(64)),
                                outcome: ControlOutcome::Admitted,
                            }),
                        },
                    }
                }
                _ => panic!("no provider effect or other broker operation expected"),
            };
            write_frame(&mut stream, &response, FrameLimits::default())
                .await
                .unwrap();
        }
    });
    let switch = || AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "switch".into(),
            kind: "function".into(),
            function: ModelFunctionCall {
                name: "select_model".into(),
                arguments: json!({"model":"alternate"}).to_string(),
            },
        }],
        usage: None,
        replay_items: vec![],
    };
    let models = ModelScript::new([switch(), answer("first"), switch(), answer("second")]);
    let replier = Arc::new(RecordingReplier::default());
    let runner = runner(
        ResolvedBroker {
            socket_path: path,
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        models.clone(),
        4,
    );
    let mut bound = route(model_config());
    let mut second = model_config();
    if let ModelConfig::OpenaiCompatible { name, model, .. } = &mut second {
        *name = "alternate".into();
        *model = "other-wire-model".into();
    }
    bound.controls = Some(crate::routes::BoundControls {
        models: vec![bound.model.clone(), Arc::new(second)],
        max_attempts: 4,
    });
    for question in ["one", "two"] {
        run_session(
            runner.clone(),
            bound.clone(),
            message(question),
            replier.clone() as Arc<dyn ChatReplier>,
        )
        .await;
    }
    server.await.unwrap();
    assert_eq!(replier.replies(), ["first", "second"]);
    assert_eq!(models.builds(), 2);
    let first = recv.recv().await.unwrap();
    let second = recv.recv().await.unwrap();
    assert_ne!(first.scope.job, second.scope.job);
    for proposal in [&first, &second] {
        assert_eq!(proposal.from.model.as_str(), "local-qwen");
        assert_eq!(proposal.to.model.as_str(), "alternate");
        assert_eq!(proposal.sequence, 1);
    }
    assert!(
        models
            .prompt(1)
            .iter()
            .any(|(_, text)| text.contains("other-wire-model"))
    );
    assert!(
        models
            .prompt(2)
            .iter()
            .any(|(_, text)| text.contains("qwen3"))
    );
    assert_ne!(models.cache_key(0), models.cache_key(1));
    assert_eq!(models.cache_key(0), models.cache_key(2));
}

mod activity_progress;
mod lifecycle_receipts;

#[tokio::test(flavor = "multi_thread")]
async fn repair_unknown_work_survives_evicted_history_before_a_followup() {
    let directory = temporary();
    let mut responses = listings(5, &["echo.echo"]);
    responses.push(ResponseEnvelope::error(
        "outcome-unaudited",
        "fixture unknown effect",
    ));
    responses.extend(listings(2, &["echo.echo"]));
    let (broker, mut observed) = stub_broker(directory.path(), responses).await;
    let models = ModelScript::new([script_call("echo.echo"), answer("must not infer again")]);
    let runner = runner(broker, Arc::clone(&models), 4);
    let mut tiny = window();
    tiny.limits.max_bytes = 1;
    let route = persistent_route(model_config(), tiny);
    let replier = Arc::new(RecordingReplier::default());
    for text in ["oversized request", "follow up"] {
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message(text),
            Arc::clone(&replier) as Arc<dyn ChatReplier>,
        )
        .await;
    }
    assert_eq!(
        models.requests(),
        1,
        "unknown work fences follow-up before inference"
    );
    let mut invocations = 0;
    while let Ok(request) = observed.try_recv() {
        if matches!(request.request, BrokerRequest::Invoke { .. }) {
            invocations += 1;
        }
    }
    assert_eq!(
        invocations, 1,
        "unknown effects are not automatically retried"
    );
    assert_eq!(replier.replies(), [FAILURE_REPLY, FAILURE_REPLY]);
}
