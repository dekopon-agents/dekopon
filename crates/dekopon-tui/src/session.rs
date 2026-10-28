//! Connecting to the broker, choosing a model, and driving one bounded turn.
//!
//! The console is an unprivileged broker client that happens to run the loop itself. It holds a
//! model credential and nothing else: no policy, no provider credential, no authorization. What a
//! session may do is whatever Cedar grants the attested subject through the selected agent, asked
//! fresh on every hop.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_agent::{
    BrokerLeg, BrokerLegError, SessionInvoker, ShellRuntime,
    attachment::{ChatAssetInputs, ReplyAttachments},
    meta::{
        AgentConfigView, EffectiveCapabilityView, MemoryConfigView, SessionConfigView, SkillView,
    },
    progress::ProgressSink,
    prompt::{
        CancellationProbe, ConversationTurn, History, HistoryLimits, PromptLimits,
        ReplyDisposition, SessionInputs, run_prompt_session,
    },
};
use dekopon_broker_protocol::{
    Attestation, BrokerClient, BrokerSocketDiscovery, ChatScopeClaim, ChatTransportKind,
    ClientError, Conversation, ConversationKind, DeliveredAnswer, DeliveredTurnRequest,
    DeliveryIdentity, FrameLimits, InvocationOutcome, ResolvedBrokerSocket, TraceParent, Trigger,
};
use dekopon_config::Skill;
use dekopon_core::{AgentId, ExternalSubject};
use dekopon_model::{
    blocking::BlockingModel,
    chatgpt::{self, ChatGptError},
    codex::CodexClient,
    error::InferenceError,
    inference::ModelClient,
    model::ChatModel,
    openai::OpenAiClient,
};
use dekopon_process::CancelHandle;
use dekopon_process::{CallBudget, Limits as ShellLimits, ShellThreads};
use dekopon_protocol::Agent;
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandProposal, JobControl,
    Streams,
};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::{mpsc::unbounded_channel, watch};

use crate::{
    profile::OperatorProfile,
    record::{RecordingInvoker, RecordingRuntime, RecordingUsage, Sequence, SessionEvent},
};

/// Credential file the console resolves to when nothing else names one.
///
/// Deliberately *not* `chatgpt-auth.json`. The refresh token rotates, so the process that refreshes
/// invalidates every other copy — and the gateway on this machine, plus whatever was seeded into a
/// cluster from an export of it, are all sitting on that one file. A console that shared it would
/// take it over the first time it refreshed, and both ends would fail without saying why.
pub const CONSOLE_AUTH_FILE_NAME: &str = "chatgpt-auth.console.json";

/// Default wall-clock ceiling for one model request.
const DEFAULT_MODEL_TIMEOUT: Duration = Duration::from_secs(120);

/// Failure setting up or running a console session.
#[derive(Debug, Error)]
pub enum SessionError {
    /// No discovery tier named a broker socket.
    #[error(
        "could not determine the broker socket path; pass --socket or set DEKOPON_BROKER_SOCKET"
    )]
    SocketUnresolved,
    /// The socket resolved but nothing is serving it.
    #[error("no broker found at {path} (resolved from the {tier} tier)")]
    NoBroker {
        /// The exact path that was tried. Never a guess: candidates are not probed.
        path: PathBuf,
        /// Which precedence tier produced it.
        tier: &'static str,
        /// The client's own reason.
        #[source]
        source: ClientError,
    },
    /// The broker refused or could not answer the capability snapshot. The source says which.
    #[error("could not open a broker session for this subject and agent")]
    Leg(#[from] BrokerLegError),
    /// Resolution landed on the credential file another surface owns.
    #[error(
        "refusing to use {path}: that is the credential file `dekopon-gatewayd` and `dekopon auth chatgpt` \
         resolve to, and the refresh token rotates, so sharing it would invalidate theirs. Run \
         `dekopon auth chatgpt login --auth-file <PATH>` for a console credential, or pass \
         --auth-file to accept this one deliberately"
    )]
    SharedCredential {
        /// The file the console refused.
        path: PathBuf,
    },
    /// The ChatGPT client refused.
    #[error(transparent)]
    ChatGpt(#[from] ChatGptError),
    /// The OpenAI-compatible client refused.
    #[error(transparent)]
    Model(#[from] InferenceError),
    /// An explicitly configured API-key environment variable was absent or empty.
    #[error("model credential variable {0} is absent or empty")]
    MissingApiKey(String),
    /// The blocking task carrying the prompt loop did not finish.
    #[error("the session task did not complete")]
    Task(#[source] tokio::task::JoinError),
}

/// Which model backend a session talks to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelChoice {
    /// The ChatGPT/Codex subscription, on a credential file of the console's own.
    ChatGptSubscription {
        /// Explicit path, or `None` to resolve [`CONSOLE_AUTH_FILE_NAME`].
        auth_file: Option<PathBuf>,
    },
    /// Any OpenAI-compatible chat-completions endpoint.
    OpenAiCompatible {
        /// Base URL.
        endpoint: String,
        /// Name of the environment variable holding a bearer token, if the endpoint needs one.
        api_key_env: Option<String>,
    },
}

/// Why this model was selected; retained independently of its display name.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ModelSource {
    CliOverride,
    Profile,
    #[default]
    Default,
}

impl ModelSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CliOverride => "CLI override",
            Self::Profile => "profile",
            Self::Default => "default",
        }
    }
}

/// Fallback shared by startup and subsequent authored-profile selection.
pub const DEFAULT_MODEL: &str = "gpt-5.6-luna";

/// Resolve name and provenance together, on startup and every profile switch.
#[must_use]
pub fn selected_model(cli: Option<&str>, profile: Option<&str>) -> (String, ModelSource) {
    match (cli, profile) {
        (Some(model), _) => (model.into(), ModelSource::CliOverride),
        (None, Some(model)) => (model.into(), ModelSource::Profile),
        (None, None) => (DEFAULT_MODEL.into(), ModelSource::Default),
    }
}

/// Everything a console run is configured with.
#[derive(Clone, Debug)]
pub struct ConsoleOptions {
    /// Explicit catalog path, or `None` for the documented discovery order.
    pub catalog: Option<PathBuf>,
    /// Explicit broker socket, or `None` for the documented precedence.
    pub socket: Option<PathBuf>,
    /// Trusted broker server UID; defaults to the caller's own.
    pub server_uid: Option<u32>,
    /// Frame bounds for every broker connection.
    pub frame_limits: FrameLimits,
    /// The canonical external subject sessions propose on behalf of.
    pub subject: ExternalSubject,
    /// Authored route scope, never inferred from the agent or the terminal.
    pub scope: Option<ChatScopeClaim>,
    /// Opt in to the broker-derived synthetic conversation (never real chat replay).
    pub smoke_conversation: bool,
    /// Validated operator contexts used when switching agents.
    pub profiles: Vec<OperatorProfile>,
    /// Already validated and bounded mounted skills for each agent.
    pub skills: HashMap<AgentId, Vec<Skill>>,
    /// Whether the initial context binds the console to one agent.
    pub fixed_profile: Option<String>,
    /// Requested exact agent, opened without a picker action.
    pub initial_agent: Option<AgentId>,
    /// Explicit CLI overrides, retained when hopping to another authored context.
    pub model_override: Option<String>,
    pub steps_override: Option<u32>,
    pub calls_override: Option<u32>,
    /// Model name handed to the backend.
    pub model: String,
    /// Selection provenance displayed alongside the model name.
    pub model_source: ModelSource,
    /// Which backend.
    pub model_choice: ModelChoice,
    /// Per-request model deadline.
    pub model_timeout: Duration,
    /// Session bounds.
    pub prompt_limits: PromptLimits,
    /// Per-script interpreter bounds.
    pub shell_limits: ShellLimits,
    pub shell_threads: ShellThreads,
    /// Replay window handed to the model.
    pub history_limits: HistoryLimits,
}

impl ConsoleOptions {
    /// The options `dekopon-console` runs with when only a subject and a model are named.
    #[must_use]
    pub fn new(subject: ExternalSubject, model: String, shell_threads: ShellThreads) -> Self {
        Self {
            catalog: None,
            socket: None,
            server_uid: None,
            frame_limits: FrameLimits::default(),
            subject,
            scope: None,
            smoke_conversation: false,
            profiles: Vec::new(),
            skills: HashMap::new(),
            fixed_profile: None,
            initial_agent: None,
            model_override: None,
            steps_override: None,
            calls_override: None,
            model,
            model_source: ModelSource::Default,
            model_choice: ModelChoice::ChatGptSubscription { auth_file: None },
            model_timeout: DEFAULT_MODEL_TIMEOUT,
            prompt_limits: PromptLimits {
                max_steps: 8,
                max_capability_calls: 16,
            },
            shell_limits: ShellLimits::default(),
            shell_threads,
            history_limits: HistoryLimits::default(),
        }
    }
}

/// Resolves the console's credential file and refuses another surface's.
///
/// An explicit path is honoured whatever it points at — that is the deliberate, typed act the rest
/// of this CLI already uses for credential decisions. Without one, the console resolves its own
/// file name; if that answer is the file every other surface resolves to, the environment sent it
/// there and it refuses rather than quietly taking the gateway's credential over.
///
/// # Errors
///
/// Returns [`SessionError::SharedCredential`] when discovery lands on the shared file, and
/// [`SessionError::ChatGpt`] when no tier names a path at all.
pub fn resolve_console_credential(explicit: Option<&Path>) -> Result<PathBuf, SessionError> {
    let resolved = chatgpt::resolve_auth_path_named(explicit, CONSOLE_AUTH_FILE_NAME)?;
    let shared = chatgpt::resolve_auth_path_named(None, chatgpt::DEFAULT_AUTH_FILE_NAME)?;
    guard_shared_credential(resolved, &shared, explicit.is_some())
}

/// Applies the guard to an already-resolved pair.
///
/// Split out from [`resolve_console_credential`] so the decision is testable without a test
/// mutating this process's environment: `set_var` is unsafe in this edition and this workspace
/// forbids unsafe outright, so the rule has to be reachable without the variable that triggers it.
fn guard_shared_credential(
    resolved: PathBuf,
    shared: &Path,
    explicit: bool,
) -> Result<PathBuf, SessionError> {
    if !explicit && resolved == shared {
        return Err(SessionError::SharedCredential { path: resolved });
    }
    Ok(resolved)
}

/// Resolves the broker socket, opens a client, and proves something is serving it.
///
/// The probe is the point. `BrokerClient::new` validates a path's ownership and mode; it does not
/// connect, and a socket path is legitimately absent whenever the daemon is stopped. Without one
/// exchange here, "no broker" would surface as an inexplicable refusal on the first hop, after the
/// console had already taken the screen. One `capabilities` request — the cheapest, least
/// privileged operation on the wire — turns that into a single startup failure naming the exact
/// path that was tried and the tier it came from.
///
/// The answer is discarded. What this session may do comes from `capabilitiesFor` on the attested
/// subject, not from what the connected peer happens to hold.
///
/// # Errors
///
/// Returns [`SessionError::SocketUnresolved`] when no tier applies and [`SessionError::NoBroker`]
/// when one did but nothing answered on it.
pub async fn connect(
    options: &ConsoleOptions,
) -> Result<(BrokerClient, ResolvedBrokerSocket), SessionError> {
    let socket = BrokerSocketDiscovery::from_process(options.socket.clone())
        .resolve()
        .ok_or(SessionError::SocketUnresolved)?;
    // The caller's own effective UID is right for a per-user broker sharing one owner-UID trust
    // domain; a dedicated service account is the case that passes it explicitly.
    let server_uid = options
        .server_uid
        .unwrap_or_else(|| rustix::process::geteuid().as_raw());
    let no_broker = |source: ClientError| SessionError::NoBroker {
        path: socket.path().to_path_buf(),
        tier: socket.tier().label(),
        source,
    };
    let client =
        BrokerClient::new(socket.path(), server_uid, options.frame_limits).map_err(no_broker)?;
    client.capabilities().await.map_err(no_broker)?;
    Ok((client, socket))
}

/// The sole reserved claim; the broker replaces its conversation ID with a boot-scoped digest.
#[must_use]
pub fn smoke_claim() -> ChatScopeClaim {
    ChatScopeClaim {
        participant: None,
        transport: "console-smoke".parse().unwrap_or_else(|_| unreachable!()),
        kind: ChatTransportKind::Local,
        conversation: Conversation {
            kind: ConversationKind::DirectMessage,
            container: None,
            id: "console-smoke".into(),
            thread: None,
        },
        trigger: Trigger::Message,
        delivery: None,
    }
}

struct SmokeIdentity {
    boot_nonce: String,
    next: std::sync::atomic::AtomicU64,
}

impl SmokeIdentity {
    fn new(seed: [u8; 16]) -> Self {
        Self {
            boot_nonce: seed.iter().map(|byte| format!("{byte:02x}")).collect(),
            next: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn turn(&self, parent: TraceParent, text: String) -> DeliveredTurnRequest {
        let sequence = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        DeliveredTurnRequest::new(
            format!("console-smoke-{}-{sequence}", self.boot_nonce)
                .parse()
                .unwrap_or_else(|_| unreachable!()),
            parent,
            DeliveryIdentity::Local {
                transport: "console-smoke".parse().unwrap_or_else(|_| unreachable!()),
                conversation: smoke_claim().conversation.key(),
                boot_nonce: self.boot_nonce.clone(),
                connection: 1,
                sequence,
            },
            text,
            DeliveredAnswer::accepted_by_transport("Recorded from console smoke".into()),
        )
    }
}

/// Record one synthetic delivered turn; only called after the local CLI gate.
pub async fn smoke_record(
    client: &BrokerClient,
    subject: ExternalSubject,
    agent: AgentId,
    text: String,
) -> Result<(String, bool), ClientError> {
    static IDENTITY: std::sync::OnceLock<SmokeIdentity> = std::sync::OnceLock::new();
    let parent = dekopon_agent::session_trace_parent();
    let identity = IDENTITY.get_or_init(|| SmokeIdentity::new(parent.trace_id()));
    let turn = identity.turn(parent, text);
    let attestation =
        Attestation::for_chat(subject, agent, smoke_claim()).bound_to(turn.id.clone());
    let result = client.record_delivered_turn(attestation, turn).await?;
    let succeeded = result.outcome == InvocationOutcome::Succeeded;
    Ok((
        format!("{:?}: {}", result.outcome, result.error.unwrap_or_default()),
        succeeded,
    ))
}

/// Opens one agent's attested leg and snapshots what policy grants it.
///
/// The snapshot comes from `capabilitiesFor`, so it is what this subject may do *through this
/// agent* rather than what the connected peer holds. An empty answer is a valid result: it means
/// policy grants nothing here, which reads very differently from an unreachable broker.
///
/// # Errors
///
/// Returns [`SessionError::Leg`] when the broker refuses or cannot answer.
pub async fn open_agent(
    client: BrokerClient,
    subject: ExternalSubject,
    agent: AgentId,
    scope: Option<ChatScopeClaim>,
) -> Result<BrokerLeg, SessionError> {
    open_agent_with_assets(
        client,
        subject,
        agent,
        scope,
        Arc::new(assets::ConsoleAssets::default()),
    )
    .await
}

/// Open a new broker leg bound to the selected entry's inventory.
pub async fn open_agent_with_assets(
    client: BrokerClient,
    subject: ExternalSubject,
    agent: AgentId,
    scope: Option<ChatScopeClaim>,
    store: Arc<assets::ConsoleAssets>,
) -> Result<BrokerLeg, SessionError> {
    let attestation = match scope {
        Some(scope) => Attestation::for_chat(subject, agent, scope),
        None => Attestation::for_subject(subject, agent),
    };
    let leg = BrokerLeg::connect(client, Some(attestation))
        .await
        .map_err(SessionError::Leg)?;
    Ok(leg
        .with_provider_attachments(Arc::new(ReplyAttachments::new(
            0,
            Arc::<assets::ConsoleAssets>::clone(&store),
            "console (no delivery)".to_owned(),
        )))
        .with_chat_asset_inputs(ChatAssetInputs::new(store)))
}

/// The local leg of a console session, which is deliberately empty.
///
/// `dekopon-run` fills this slot with an import-free Wasmtime registry. The console does not: every
/// capability it cares about performs I/O, which the import-free host cannot do, so a local leg
/// would add a Wasmtime dependency to this console in exchange for nothing. Answering "not
/// mine" to everything sends all dispatch to the broker, where the authority is.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDirect;

impl CapabilityInvoker for NoDirect {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn is_granted(&self, _capability: &str) -> bool {
        false
    }

    fn command_words(&self) -> Vec<String> {
        Vec::new()
    }

    fn has_command_word(&self, _word: &str) -> bool {
        false
    }

    fn describe(&self, _capability: &str) -> Option<CapabilityDescription> {
        None
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        _streams: Streams,
        _tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        if proposal.secret_use.is_some() {
            return dekopon_shell::secret_use_unsupported();
        }
        CapabilityCallResult::NotFound
    }
}

/// Cooperative stop shared between the console's key handling and a running session.
///
/// Cancellation is not rollback. It stops the next model turn or tool call from starting; a
/// provider request the broker already accepted still finishes, and the console must say so rather
/// than claim the turn was undone.
#[derive(Clone, Default)]
pub struct StopFlag {
    requested: watch::Sender<bool>,
    broker: Arc<Mutex<Option<CancelHandle>>>,
}

impl StopFlag {
    /// Requests a stop at the session's next cooperative boundary.
    pub fn request(&self) {
        self.requested.send_replace(true);
        if let Some(handle) = self.broker.lock().as_ref() {
            handle.cancel();
        }
    }

    /// Whether a stop has been requested.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.requested.borrow()
    }

    /// Clears the flag for the next session.
    pub fn reset(&self) {
        self.requested.send_replace(false);
        *self.broker.lock() = None;
    }
}

impl StopFlag {
    /// The model client's view of Stop, so a request aborts the exchange in flight.
    #[must_use]
    pub fn model_cancel(&self) -> watch::Receiver<bool> {
        self.requested.subscribe()
    }

    /// Binds Stop to the current broker leg as well as the model loop.
    pub fn bind_broker(&self, handle: CancelHandle) {
        *self.broker.lock() = Some(handle);
    }
}

impl CancellationProbe for StopFlag {
    fn is_cancelled(&self) -> bool {
        self.is_requested()
    }
}

/// Builds the model client this session talks to, bridged onto the blocking prompt loop with
/// Stop as its cancellation.
///
/// # Errors
///
/// Returns the backend's own refusal, including the console's credential guard.
/// Must be called on the Tokio runtime; the returned model blocks on it from the turn's blocking
/// task.
pub fn build_model(
    options: &ConsoleOptions,
    stop: &StopFlag,
) -> Result<Box<dyn ChatModel + Send + Sync>, SessionError> {
    let client = match &options.model_choice {
        ModelChoice::ChatGptSubscription { auth_file } => {
            let path = resolve_console_credential(auth_file.as_deref())?;
            ModelClient::Codex(CodexClient::new(
                &options.model,
                Some(&path),
                options.model_timeout,
                &[],
            )?)
        }
        ModelChoice::OpenAiCompatible {
            endpoint,
            api_key_env,
        } => {
            let bearer = api_key_env
                .as_ref()
                .map(|name| {
                    std::env::var(name)
                        .ok()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| SessionError::MissingApiKey(name.clone()))
                })
                .transpose()?;
            ModelClient::OpenAiCompatible(OpenAiClient::new(
                endpoint,
                &options.model,
                bearer,
                options.model_timeout,
                &[],
            )?)
        }
    };
    Ok(Box::new(BlockingModel::new(
        Arc::new(client),
        tokio::runtime::Handle::current(),
        stop.model_cancel(),
        options.model_timeout,
    )))
}

/// One agent hop: the leg, its granted surface, and the conversation it accumulates.
pub struct AgentSession {
    /// Which agent this session drives.
    pub agent: AgentId,
    /// The broker's fresh answer for this subject through this agent.
    pub effective: Vec<EffectiveCapabilityView>,
    /// Command words the session's `bash` tool will accept.
    pub command_words: Vec<String>,
    /// The broker's help page for each command word, as the model sees it.
    pub command_word_help: BTreeMap<String, String>,
    /// The replay window handed to the model, which is not the console's own transcript.
    pub history: History,
    leg: Arc<BrokerLeg>,
}

impl AgentSession {
    /// Wraps one opened leg.
    #[must_use]
    pub fn new(agent: AgentId, leg: BrokerLeg, history_limits: HistoryLimits) -> Self {
        let effective = leg.effective_capabilities();
        let invoker: &dyn CapabilityInvoker = &leg;
        let command_words = invoker.command_words();
        let command_word_help = invoker.command_word_help();
        Self {
            agent,
            effective,
            command_words,
            command_word_help,
            history: History::new(history_limits),
            leg: Arc::new(leg),
        }
    }

    /// Whether policy granted this subject anything at all through this agent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effective.is_empty()
    }

    /// Borrows the leg, for the shell pane's direct dispatch.
    #[must_use]
    pub fn leg(&self) -> &Arc<BrokerLeg> {
        &self.leg
    }
}

/// Runs one bounded turn on a blocking task, streaming what it does over `events`.
///
/// The prompt loop and the interpreter are synchronous by design and `BrokerLeg` is only valid on a
/// blocking task, so the whole session runs on one and reports back through the channel rather than
/// being made `async`.
///
/// The caller takes `history` back with this turn recorded, whether the turn succeeded or failed.
///
/// # Errors
///
/// Returns [`SessionError::Task`] only when the blocking task itself did not complete. A refused,
/// cancelled, or exhausted session is delivered as [`SessionEvent::Finished`].
#[expect(
    clippy::too_many_arguments,
    reason = "a turn carries its broker leg, model, authored agent, limits, history, cancellation, progress and output channel together"
)]
pub async fn run_turn(
    leg: BrokerLeg,
    model: Arc<dyn ChatModel + Send + Sync>,
    prompt: String,
    system: Option<String>,
    agent: Agent,
    options: ConsoleOptions,
    mut history: History,
    stop: StopFlag,
    progress: Arc<dyn ProgressSink>,
    events: tokio::sync::mpsc::UnboundedSender<SessionEvent>,
) -> Result<History, SessionError> {
    // Tokio does not inherit tracing context on its blocking pool. Enter only on that thread.
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        let sequence = Sequence::default();
        let calls = CallBudget::new(options.prompt_limits.max_capability_calls);
        let leg = Arc::new(leg.with_progress(Arc::clone(&progress), calls.clone()));
        let runtime = RecordingRuntime::new(
            ShellRuntime {
                invoker: RecordingInvoker::new(
                    SessionInvoker {
                        direct: NoDirect,
                        broker: Some(Box::new(LegHandle(Arc::clone(&leg)))),
                    },
                    events.clone(),
                    sequence.clone(),
                ),
                limits: options.shell_limits,
                calls,
                threads: options.shell_threads.clone(),
            },
            events.clone(),
            sequence,
        );
        let usage = RecordingUsage::new(events.clone());
        let skills = options
            .skills
            .get(
                &agent
                    .metadata
                    .name
                    .parse::<AgentId>()
                    .expect("validated catalog agent"),
            )
            .map_or(&[][..], Vec::as_slice);
        let config = AgentConfigView::new(
            agent.metadata.name.clone(),
            agent.spec.description.clone(),
            agent.spec.model_class.clone(),
            system.clone(),
            SessionConfigView {
                max_steps: options.prompt_limits.max_steps,
                max_capability_calls: options.prompt_limits.max_capability_calls,
                memory: MemoryConfigView::OneShot,
            },
            leg.effective_capabilities(),
        )
        .with_skills(
            skills
                .iter()
                .map(|skill| SkillView {
                    name: skill.name().to_string(),
                    description: skill.description().to_owned(),
                    resources: skill
                        .resources()
                        .iter()
                        .map(|resource| resource.path.clone())
                        .collect(),
                })
                .collect(),
        );
        let inputs = SessionInputs::new(&prompt, options.prompt_limits)
            .with_system(system.as_deref())
            .with_skills(skills)
            .with_agent_config(&config)
            .with_usage_observer(&usage)
            .with_cancellation(&stop)
            .with_progress(progress);

        let (result, captured) =
            run_prompt_session(model.as_ref(), &runtime, inputs, &history.replay());
        if let Some(captured) = &captured {
            let answer = match &result {
                Ok(outcome) if outcome.disposition == ReplyDisposition::Send => {
                    Some(outcome.answer.as_str())
                }
                Ok(_) | Err(_) => None,
            };
            history.record(ConversationTurn::from_captured(captured, answer));
        }
        let outcome = result.map_err(|error| crate::report::chain(&error));
        if events
            .send(SessionEvent::Finished(Box::new(outcome)))
            .is_err()
        {
            // The console is gone, so nothing will draw this turn's outcome. The history still
            // comes back below, so a console that survives keeps a replay window matching what
            // actually ran.
            tracing::debug!(
                reason = "console-receiver-closed",
                "a completed turn had nowhere to report"
            );
        }
        history
    })
    .await
    .map_err(SessionError::Task)
}

/// Shares one leg across a session's dispatch without cloning its capability snapshot.
///
/// `SessionInvoker` wants an owned `Box<dyn CapabilityInvoker + Send>` while the console keeps the
/// leg for its shell pane, so this forwards through the `Arc` both hold.
#[doc(hidden)]
pub struct LegHandle(pub Arc<BrokerLeg>);

impl CapabilityInvoker for LegHandle {
    fn cancel_signals(&self) -> Vec<dekopon_process::CancelSignal> {
        self.0.cancel_signals()
    }

    fn script_started(&self, tree: &mut dekopon_process::TreeContext) {
        self.0.script_started(tree);
    }

    fn job_control(&self) -> Option<&dyn JobControl> {
        self.0.job_control()
    }

    fn granted(&self) -> Vec<String> {
        self.0.granted()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.0.is_granted(capability)
    }

    fn command_words(&self) -> Vec<String> {
        self.0.command_words()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.0.command_word_help()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.0.has_command_word(word)
    }

    fn note(&self, text: &str, eta: Option<Duration>) {
        self.0.note(text, eta);
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.0.describe(capability)
    }

    fn script_finished(&self) {
        self.0.script_finished();
    }

    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: Streams,
        tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::RunResult {
        self.0.run(call, streams, tree)
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        self.0.invoke(proposal, streams, tree)
    }
}

/// Opens a channel for one session's events.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "owner: one RunningTurn drained and joined by the event loop; bound: one active turn with configured prompt steps, capability calls and interpreter limits"
)]
pub fn session_channel() -> (
    tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    unbounded_channel()
}

pub mod assets;

#[cfg(test)]
mod tests;
