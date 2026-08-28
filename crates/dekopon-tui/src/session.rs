//! Connecting to the broker, choosing a model, and driving one bounded turn.
//!
//! The console is an unprivileged broker client that happens to run the loop itself. It holds a
//! model credential and nothing else: no policy, no provider credential, no authorization. What a
//! session may do is whatever Cedar grants the attested subject through the selected agent, asked
//! fresh on every hop.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dekopon_agent::{
    BrokerLeg, BrokerLegError, SessionInvoker, ShellRuntime,
    meta::EffectiveCapabilityView,
    prompt::{
        CancellationProbe, History, HistoryLimits, PromptLimits, SessionInputs, run_prompt_session,
    },
};
use dekopon_broker_protocol::{
    BrokerClient, BrokerSocketDiscovery, ClientError, FrameLimits, ResolvedBrokerSocket,
};
use dekopon_core::{AgentId, ExternalSubject};
use dekopon_model::{
    chatgpt::{self, ChatGptCodexModel, ChatGptError},
    model::{ChatModel, ModelError, OpenAiChatModel},
};
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, Limits as ShellLimits,
};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc::unbounded_channel;

use crate::record::{RecordingInvoker, RecordingRuntime, RecordingUsage, Sequence, SessionEvent};

/// Credential file the console resolves to when nothing else names one.
///
/// Deliberately *not* `chatgpt-auth.json`. The refresh token rotates, so the process that refreshes
/// invalidates every other copy — and the gateway on this machine, plus whatever was seeded into a
/// cluster from an export of it, are all sitting on that one file. A console that shared it would
/// take it over the first time it refreshed, and both ends would fail without saying why.
pub const CONSOLE_AUTH_FILE_NAME: &str = "chatgpt-auth.console.json";

/// Trace prefix every invocation this console makes carries.
///
/// It is the join key between what the console shows and what the broker's audit chain recorded, so
/// a write can be found later by prefix rather than by timestamp.
pub const TRACE_PREFIX: &str = "dekopon-console";

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
    /// The broker refused or could not answer the capability snapshot.
    #[error("the broker would not open a session for this subject and agent")]
    Leg(#[from] BrokerLegError),
    /// Resolution landed on the credential file another surface owns.
    #[error(
        "refusing to use {path}: that is the credential file `dekopond` and `dekopon auth chatgpt` \
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
    Model(#[from] ModelError),
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
    /// Model name handed to the backend.
    pub model: String,
    /// Which backend.
    pub model_choice: ModelChoice,
    /// Per-request model deadline.
    pub model_timeout: Duration,
    /// Session bounds.
    pub prompt_limits: PromptLimits,
    /// Per-script interpreter bounds.
    pub shell_limits: ShellLimits,
    /// Replay window handed to the model.
    pub history_limits: HistoryLimits,
}

impl ConsoleOptions {
    /// The options a bare `dekopon console` runs with.
    #[must_use]
    pub fn new(subject: ExternalSubject, model: String) -> Self {
        Self {
            catalog: None,
            socket: None,
            server_uid: None,
            frame_limits: FrameLimits::default(),
            subject,
            model,
            model_choice: ModelChoice::ChatGptSubscription { auth_file: None },
            model_timeout: DEFAULT_MODEL_TIMEOUT,
            prompt_limits: PromptLimits {
                max_steps: 8,
                max_capability_calls: 16,
            },
            shell_limits: ShellLimits::default(),
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
        BrokerClient::new(socket.path(), server_uid, options.frame_limits).map_err(&no_broker)?;
    client.capabilities().await.map_err(no_broker)?;
    Ok((client, socket))
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
) -> Result<BrokerLeg, SessionError> {
    BrokerLeg::connect_attested(client, TRACE_PREFIX, subject, agent)
        .await
        .map_err(SessionError::Leg)
}

/// The local leg of a console session, which is deliberately empty.
///
/// `dekopon-run` fills this slot with an import-free Wasmtime registry. The console does not: every
/// capability it cares about performs I/O, which the import-free host cannot do, so a local leg
/// would add a Wasmtime dependency to the operator CLI in exchange for nothing. Answering "not
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

    fn grants_namespace(&self, _namespace: &str) -> bool {
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
        _capability: &str,
        _input: Value,
        _secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        // Deny-by-default on the direct leg. The console's local leg reaches nothing, so it owns
        // no capability and no secret; `NotFound` sends both kinds of call to the broker.
        CapabilityCallResult::NotFound
    }
}

/// Cooperative stop shared between the console's key handling and a running session.
///
/// Cancellation is not rollback. It stops the next model turn or tool call from starting; a
/// provider request the broker already accepted still finishes, and the console must say so rather
/// than claim the turn was undone.
#[derive(Clone, Debug, Default)]
pub struct StopFlag(Arc<AtomicBool>);

impl StopFlag {
    /// Requests a stop at the session's next cooperative boundary.
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether a stop has been requested.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Clears the flag for the next session.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl CancellationProbe for StopFlag {
    fn is_cancelled(&self) -> bool {
        self.is_requested()
    }
}

/// Builds the model client this session talks to.
///
/// # Errors
///
/// Returns the backend's own refusal, including the console's credential guard.
pub fn build_model(
    options: &ConsoleOptions,
) -> Result<Box<dyn ChatModel + Send + Sync>, SessionError> {
    match &options.model_choice {
        ModelChoice::ChatGptSubscription { auth_file } => {
            let path = resolve_console_credential(auth_file.as_deref())?;
            Ok(Box::new(ChatGptCodexModel::new(
                &options.model,
                Some(&path),
                options.model_timeout,
            )?))
        }
        ModelChoice::OpenAiCompatible {
            endpoint,
            api_key_env,
        } => {
            let bearer = api_key_env
                .as_deref()
                .and_then(|name| std::env::var(name).ok())
                .filter(|value| !value.is_empty());
            Ok(Box::new(OpenAiChatModel::new(
                endpoint,
                &options.model,
                bearer,
                options.model_timeout,
            )?))
        }
    }
}

/// One agent hop: the leg, its granted surface, and the conversation it accumulates.
pub struct AgentSession {
    /// Which agent this session drives.
    pub agent: AgentId,
    /// The broker's fresh answer for this subject through this agent.
    pub effective: Vec<EffectiveCapabilityView>,
    /// Command words the session's `bash` tool will accept.
    pub command_words: Vec<String>,
    /// The replay window handed to the model, which is not the console's own transcript.
    pub history: History,
    leg: Arc<BrokerLeg>,
}

impl AgentSession {
    /// Wraps one opened leg.
    #[must_use]
    pub fn new(agent: AgentId, leg: BrokerLeg, history_limits: HistoryLimits) -> Self {
        let effective = leg.effective_capabilities();
        let command_words = {
            let invoker: &dyn CapabilityInvoker = &leg;
            invoker.command_words()
        };
        Self {
            agent,
            effective,
            command_words,
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
/// Consumes and returns `history` because the loop needs `&mut` to it for the whole session; the
/// caller takes it back with this turn recorded, whether the turn succeeded or failed.
///
/// # Errors
///
/// Returns [`SessionError::Task`] only when the blocking task itself did not complete. A refused,
/// cancelled, or exhausted session is delivered as [`SessionEvent::Finished`].
#[expect(
    clippy::too_many_arguments,
    reason = "one turn genuinely needs all eight: the leg, the model, what was asked, the standing \
              orders, the bounds, the replay window, the stop flag, and where to report. Gathering \
              them into a struct would add a type whose only purpose is to satisfy this lint, and \
              whose fields would be moved out again on the first line of the body"
)]
pub async fn run_turn(
    leg: Arc<BrokerLeg>,
    model: Arc<dyn ChatModel + Send + Sync>,
    prompt: String,
    system: Option<String>,
    options: ConsoleOptions,
    mut history: History,
    stop: StopFlag,
    events: tokio::sync::mpsc::UnboundedSender<SessionEvent>,
) -> Result<History, SessionError> {
    tokio::task::spawn_blocking(move || {
        let sequence = Sequence::default();
        let runtime = RecordingRuntime::new(
            ShellRuntime {
                invoker: RecordingInvoker::new(
                    SessionInvoker {
                        direct: NoDirect,
                        broker: Some(Box::new(Arc::clone(&leg))),
                    },
                    events.clone(),
                    sequence.clone(),
                ),
                limits: options.shell_limits,
                curl_capability: None,
            },
            events.clone(),
            sequence,
        );
        let usage = RecordingUsage::new(events.clone());
        let inputs = SessionInputs::new(&prompt, options.prompt_limits)
            .with_system(system.as_deref())
            .with_usage_observer(&usage)
            .with_cancellation(&stop);

        let outcome = run_prompt_session(model.as_ref(), &runtime, inputs, &mut history)
            .map_err(|error| error.to_string());
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

/// Opens a channel for one session's events.
#[must_use]
pub fn session_channel() -> (
    tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    unbounded_channel()
}

#[cfg(test)]
mod tests;
