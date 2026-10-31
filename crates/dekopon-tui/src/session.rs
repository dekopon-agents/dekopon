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

pub const CONSOLE_AUTH_FILE_NAME: &str = "chatgpt-auth.console.json";

const DEFAULT_MODEL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum SessionError {
    #[error(
        "could not determine the broker socket path; pass --socket or set DEKOPON_BROKER_SOCKET"
    )]
    SocketUnresolved,
    #[error("no broker found at {path} (resolved from the {tier} tier)")]
    NoBroker {
        path: PathBuf,
        tier: &'static str,
        #[source]
        source: ClientError,
    },
    #[error("could not open a broker session for this subject and agent")]
    Leg(#[from] BrokerLegError),
    #[error(
        "refusing to use {path}: that is the credential file `dekopon-gatewayd` and `dekopon auth chatgpt` \
         resolve to, and the refresh token rotates, so sharing it would invalidate theirs. Run \
         `dekopon auth chatgpt login --auth-file <PATH>` for a console credential, or pass \
         --auth-file to accept this one deliberately"
    )]
    SharedCredential { path: PathBuf },
    #[error(transparent)]
    ChatGpt(#[from] ChatGptError),
    #[error(transparent)]
    Model(#[from] InferenceError),
    #[error("model credential variable {0} is absent or empty")]
    MissingApiKey(String),
    #[error("the session task did not complete")]
    Task(#[source] tokio::task::JoinError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelChoice {
    ChatGptSubscription {
        auth_file: Option<PathBuf>,
    },
    OpenAiCompatible {
        endpoint: String,
        api_key_env: Option<String>,
    },
}

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

pub const DEFAULT_MODEL: &str = "gpt-5.6-luna";

#[must_use]
pub fn selected_model(cli: Option<&str>, profile: Option<&str>) -> (String, ModelSource) {
    match (cli, profile) {
        (Some(model), _) => (model.into(), ModelSource::CliOverride),
        (None, Some(model)) => (model.into(), ModelSource::Profile),
        (None, None) => (DEFAULT_MODEL.into(), ModelSource::Default),
    }
}

#[derive(Clone, Debug)]
pub struct ConsoleOptions {
    pub catalog: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub server_uid: Option<u32>,
    pub frame_limits: FrameLimits,
    pub subject: ExternalSubject,
    pub scope: Option<ChatScopeClaim>,
    pub smoke_conversation: bool,
    pub profiles: Vec<OperatorProfile>,
    pub skills: HashMap<AgentId, Vec<Skill>>,
    pub fixed_profile: Option<String>,
    pub initial_agent: Option<AgentId>,
    pub model_override: Option<String>,
    pub steps_override: Option<u32>,
    pub calls_override: Option<u32>,
    pub model: String,
    pub model_source: ModelSource,
    pub model_choice: ModelChoice,
    pub model_timeout: Duration,
    pub prompt_limits: PromptLimits,
    pub shell_limits: ShellLimits,
    pub shell_threads: ShellThreads,
    pub history_limits: HistoryLimits,
}

impl ConsoleOptions {
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

pub fn resolve_console_credential(explicit: Option<&Path>) -> Result<PathBuf, SessionError> {
    let resolved = chatgpt::resolve_auth_path_named(explicit, CONSOLE_AUTH_FILE_NAME)?;
    let shared = chatgpt::resolve_auth_path_named(None, chatgpt::DEFAULT_AUTH_FILE_NAME)?;
    guard_shared_credential(resolved, &shared, explicit.is_some())
}

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

#[must_use]
pub fn smoke_claim() -> ChatScopeClaim {
    ChatScopeClaim {
        participant: None,
        github: None,
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

#[derive(Clone, Default)]
pub struct StopFlag {
    requested: watch::Sender<bool>,
    broker: Arc<Mutex<Option<CancelHandle>>>,
}

impl StopFlag {
    pub fn request(&self) {
        self.requested.send_replace(true);
        if let Some(handle) = self.broker.lock().as_ref() {
            handle.cancel();
        }
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        *self.requested.borrow()
    }

    pub fn reset(&self) {
        self.requested.send_replace(false);
        *self.broker.lock() = None;
    }
}

impl StopFlag {
    #[must_use]
    pub fn model_cancel(&self) -> watch::Receiver<bool> {
        self.requested.subscribe()
    }

    pub fn bind_broker(&self, handle: CancelHandle) {
        *self.broker.lock() = Some(handle);
    }
}

impl CancellationProbe for StopFlag {
    fn is_cancelled(&self) -> bool {
        self.is_requested()
    }
}

pub fn build_model(
    options: &ConsoleOptions,
    stop: &StopFlag,
) -> Result<BlockingModel, SessionError> {
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
    Ok(BlockingModel::new(
        Arc::new(client),
        tokio::runtime::Handle::current(),
        stop.model_cancel(),
        options.model_timeout,
    ))
}

pub struct AgentSession {
    pub agent: AgentId,
    pub effective: Vec<EffectiveCapabilityView>,
    pub command_words: Vec<String>,
    pub command_word_help: BTreeMap<String, String>,
    pub history: History,
    leg: Arc<BrokerLeg>,
}

impl AgentSession {
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

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effective.is_empty()
    }

    #[must_use]
    pub fn leg(&self) -> &Arc<BrokerLeg> {
        &self.leg
    }
}

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
