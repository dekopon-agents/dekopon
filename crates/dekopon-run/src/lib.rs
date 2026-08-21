//! Immediate-mode provider execution and an unprivileged broker client for Dekopon.
//!
//! Direct and prompt modes load only read-only, import-free provider components. Explicit broker
//! mode loads no component: it submits identity-free proposals to a separate authenticated broker
//! and never constructs or receives authorization state.

#![forbid(unsafe_code)]

use std::{
    env,
    error::Error as _,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[cfg(unix)]
use dekopon_agent::{BrokerLeg, BrokerLegError};
use dekopon_agent::{
    SessionInvoker, ShellRuntime,
    prompt::{PromptError, PromptLimits, format_script_outcome, run_prompt},
};
#[cfg(unix)]
use dekopon_broker_protocol::{
    BrokerClient, ClientError, FrameLimits, InvocationOutcome, InvocationRequest,
};
#[cfg(unix)]
use dekopon_core::IdentifierError;
use dekopon_core::{CapabilityId, ExternalSubject, ProviderId};
use dekopon_model::{
    chatgpt::{ChatGptCodexModel, ChatGptError},
    model::{ChatModel, ModelError, OpenAiChatModel},
};
use dekopon_provider_host::{HostLimits, ProviderHostError, ProviderManifest, ProviderRegistry};
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, Interpreter,
    Limits as ShellLimits,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tracing::Instrument as _;

use crate::cli::{BrokerCommand, BrokerConnectionArgs, Cli, Command, LimitArgs, ShellLimitArgs};

#[cfg(unix)]
mod chat;
pub mod cli;
mod trace;

/// The prompt loop and session types now live in `dekopon-agent`; this re-export keeps the
/// `dekopon_run::prompt` path stable for existing consumers and tests.
pub use dekopon_agent::prompt;

/// Runs a parsed CLI invocation and returns a process exit code.
///
/// Clap handles syntax failures before this function and exits with code `2`.
#[must_use]
pub async fn run(cli: Cli) -> i32 {
    let trace_guard = match trace::initialize(
        cli.verbose,
        cli.no_color,
        cli.trace.as_deref(),
        &cli.telemetry,
    ) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    let command_name = command_name(&cli.command);
    let command_span = tracing::info_span!(
        "runner.command",
        command.name = command_name,
        otel.kind = "internal"
    );
    let exit_code = {
        let _entered = command_span.enter();

        match evaluate(&cli).await {
            Ok(output) => match write_output(&output.text) {
                Ok(()) => output.exit_code,
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => output.exit_code,
                Err(error) => {
                    tracing::error!(
                        target: "dekopon_run::audit",
                        {
                            audit.event = "runner.command.failed",
                            command.name = command_name,
                            error.type = "output-write",
                        },
                        "runner command failed"
                    );
                    eprintln!("error: could not write output: {error}");
                    1
                }
            },
            Err(error) => {
                tracing::error!(
                    target: "dekopon_run::audit",
                    {
                        audit.event = "runner.command.failed",
                        command.name = command_name,
                        error.type = error.telemetry_kind(),
                    },
                    "runner command failed"
                );
                report_error(&error, cli.verbose);
                1
            }
        }
    };

    // Close the root span before flushing short-lived OTLP exporters.
    drop(command_span);
    if let Err(error) = trace_guard.shutdown() {
        eprintln!("error: {error}");
        return 1;
    }
    exit_code
}

/// Stable, low-cardinality label for the command under the root span.
///
/// Exhaustive on purpose: a new subcommand must be given a name here rather than silently
/// inheriting a catch-all that would make it invisible in a trace search.
fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Inspect { .. } => "inspect",
        Command::Invoke { .. } => "invoke",
        Command::Shell { .. } => "shell",
        Command::Prompt { .. } => "prompt",
        Command::Broker {
            command: BrokerCommand::Capabilities { .. },
        } => "broker.capabilities",
        Command::Broker {
            command: BrokerCommand::Invoke { .. },
        } => "broker.invoke",
        Command::Chat { .. } => "chat",
    }
}

async fn evaluate(cli: &Cli) -> Result<CommandOutput, AppError> {
    match &cli.command {
        Command::Inspect { limits, providers } => {
            let components = providers.components()?;
            let span = tracing::info_span!("runner.inspect", provider.count = components.len());
            let _entered = span.enter();
            let registry = ProviderRegistry::load(components, host_limits(limits))?;
            let manifests = registry.manifests().collect::<Vec<&ProviderManifest>>();
            serde_json::to_string_pretty(&manifests)
                .map(CommandOutput::success)
                .map_err(AppError::Serialize)
        }
        Command::Invoke {
            limits,
            providers,
            capability,
            input,
            input_file,
            repeat,
        } => {
            let components = providers.components()?;
            let span = tracing::info_span!(
                "runner.invoke",
                provider.count = components.len(),
                capability.id = %capability,
                invocation.count = repeat.get()
            );
            let _entered = span.enter();
            let input = read_input(
                input.as_deref(),
                input_file.as_deref(),
                limits.max_input_bytes,
            )?;
            let registry = ProviderRegistry::load(components, host_limits(limits))?;
            let mut samples = TimingSamples::default();
            let mut last = None;
            let total_start = Instant::now();
            for iteration in 1..=repeat.get() {
                let invocation_span = tracing::info_span!(
                    "runner.provider_invocation",
                    capability.id = %capability,
                    invocation.iteration = iteration
                );
                let _entered = invocation_span.enter();
                let start = Instant::now();
                match registry.invoke(capability, &input) {
                    Ok(output) => {
                        let elapsed = start.elapsed();
                        samples.record(elapsed);
                        tracing::info!(
                            target: "dekopon_run::audit",
                            {
                                audit.event = "guest.invocation.completed",
                                provider.id = %output.provider,
                                capability.id = %output.capability,
                                invocation.iteration = iteration,
                                duration_ms = milliseconds(elapsed),
                                outcome = "succeeded",
                            },
                            "guest provider invocation completed"
                        );
                        last = Some(output);
                    }
                    Err(error) => {
                        tracing::error!(
                            target: "dekopon_run::audit",
                            {
                                audit.event = "guest.invocation.completed",
                                capability.id = %capability,
                                invocation.iteration = iteration,
                                duration_ms = milliseconds(start.elapsed()),
                                outcome = "failed",
                            },
                            "guest provider invocation failed"
                        );
                        return Err(error.into());
                    }
                }
            }
            let total = total_start.elapsed();
            let output = last.expect("repeat is represented by NonZeroU32");
            let report = InvocationReport::new(
                output.provider,
                output.capability,
                repeat.get(),
                total,
                &samples,
                output.output,
            );
            serde_json::to_string_pretty(&report)
                .map(CommandOutput::success)
                .map_err(AppError::Serialize)
        }
        Command::Shell {
            limits,
            providers,
            shell,
            curl_capability,
            script,
        } => {
            let components = providers.components()?;
            let span = tracing::info_span!(
                "runner.shell",
                provider.count = components.len(),
                shell.max_steps = shell.shell_max_steps,
                shell.max_capability_calls = shell.shell_max_capability_calls
            );
            let _entered = span.enter();
            let registry = ProviderRegistry::load(components, host_limits(limits))?;
            let invoker = RegistryInvoker {
                registry: &registry,
            };
            let outcome = Interpreter::new(shell_limits(shell))
                .with_curl_capability(curl_capability.as_ref().map(CapabilityId::to_string))
                .run(script, &invoker);
            Ok(CommandOutput {
                text: format_script_outcome(&outcome),
                exit_code: i32::from(outcome.exit_code.get()),
            })
        }
        Command::Broker { command } => evaluate_broker(command).await,
        Command::Prompt {
            limits,
            providers,
            shell,
            broker,
            connection,
            curl_capability,
            model,
            chatgpt_subscription,
            chatgpt_auth_file,
            endpoint,
            api_key_env,
            system,
            max_steps,
            model_timeout_ms,
            prompt,
        } => {
            let backend = if *chatgpt_subscription {
                "chatgpt-subscription"
            } else {
                "openai-compatible"
            };
            let components = providers.components()?;
            let settings = PromptSettings {
                limits: host_limits(limits),
                shell: shell_limits(shell),
                curl_capability: curl_capability.as_ref().map(CapabilityId::to_string),
                providers: components.clone(),
                model: model.clone(),
                chatgpt_subscription: *chatgpt_subscription,
                chatgpt_auth_file: chatgpt_auth_file.clone(),
                endpoint: endpoint.clone(),
                api_key_env: api_key_env.clone(),
                system: system.clone(),
                prompt: prompt.clone(),
                model_timeout: Duration::from_millis(*model_timeout_ms),
                prompt_limits: PromptLimits {
                    max_steps: max_steps.get(),
                    max_capability_calls: shell.shell_max_capability_calls,
                },
            };
            evaluate_prompt(settings, *broker, connection)
                .instrument(tracing::info_span!(
                    "runner.prompt",
                    provider.count = components.len(),
                    model = %model,
                    model.backend = backend,
                    prompt.max_steps = max_steps.get(),
                    prompt.broker = *broker
                ))
                .await
        }
        Command::Chat {
            gateway,
            subject,
            conversation,
        } => {
            // The span carries no fields on purpose. The two values a chat session is configured
            // by are a socket path and a declared subject, and `docs/observability.md` keeps both
            // filesystem paths and external identities out of exported telemetry.
            evaluate_chat(gateway, subject, conversation.clone())
                .instrument(tracing::info_span!("runner.chat"))
                .await
        }
    }
}

/// Runs one interactive gateway session on the blocking pool.
///
/// The loop is synchronous end to end — read a line, write a line, wait for a line — and the wait
/// lasts as long as a whole agent session inside the daemon, so it belongs off the runtime's
/// worker threads for exactly the reason a prompt session does.
#[cfg(unix)]
async fn evaluate_chat(
    gateway: &Path,
    subject: &ExternalSubject,
    conversation: Option<String>,
) -> Result<CommandOutput, AppError> {
    let session = chat::ChatSession::new(gateway.to_path_buf(), subject.clone(), conversation)?;
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        chat::run(&session)
    })
    .await
    .map_err(|error| AppError::Chat(chat::ChatError::Task(error)))??;

    // Replies were printed as they arrived; there is nothing left for the caller to write.
    Ok(CommandOutput::silent())
}

#[cfg(not(unix))]
async fn evaluate_chat(
    _gateway: &Path,
    _subject: &ExternalSubject,
    _conversation: Option<String>,
) -> Result<CommandOutput, AppError> {
    Err(AppError::ChatUnsupported)
}

/// Everything one prompt session needs, gathered before the blocking handoff.
///
/// This exists because the session runs on a blocking task: every field has to be owned and
/// `Send`, so borrowing from the parsed CLI is not an option.
struct PromptSettings {
    limits: HostLimits,
    shell: ShellLimits,
    curl_capability: Option<String>,
    providers: Vec<PathBuf>,
    model: String,
    chatgpt_subscription: bool,
    chatgpt_auth_file: Option<PathBuf>,
    endpoint: String,
    api_key_env: String,
    system: Option<String>,
    prompt: String,
    model_timeout: Duration,
    prompt_limits: PromptLimits,
}

/// Runs one prompt session, bridging the synchronous loop onto a blocking task.
///
/// Two boundaries this crate consumes are deliberately synchronous — `ChatModel` and
/// `dekopon_shell::Interpreter` — and both can block for a long time: a model request is a real
/// HTTP round trip, and a script can `sleep`, drive a broker round trip per command, or do both in
/// a loop. Running that on a runtime worker thread would stall every other task in the process, so
/// the whole session moves to the blocking pool and reaches back into the runtime only where it
/// must, in [`BrokerLeg::invoke`].
async fn evaluate_prompt(
    settings: PromptSettings,
    broker: bool,
    connection: &BrokerConnectionArgs,
) -> Result<CommandOutput, AppError> {
    // Connection flags that silently do nothing are worse than a refusal: an operator who passes
    // `--socket` believes the broker leg is live, and would read a "command not found" as the
    // broker denying a capability rather than as never having been asked.
    if !broker && (connection.socket.is_some() || connection.server_uid.is_some()) {
        return Err(AppError::BrokerFlagsWithoutOptIn);
    }

    let leg = connect_prompt_broker(broker, connection).await?;
    let span = tracing::Span::current();
    let outcome = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        run_prompt_session(settings, leg)
    })
    .await
    .map_err(AppError::PromptTask)??;

    Ok(CommandOutput::success(outcome.answer))
}

/// Loads providers, builds the model client, and runs the loop. Blocking throughout.
fn run_prompt_session(
    settings: PromptSettings,
    broker: Option<Box<dyn CapabilityInvoker + Send>>,
) -> Result<prompt::PromptOutcome, AppError> {
    let registry = ProviderRegistry::load(settings.providers, settings.limits)?;
    let model: Box<dyn ChatModel> = if settings.chatgpt_subscription {
        Box::new(ChatGptCodexModel::new(
            &settings.model,
            settings.chatgpt_auth_file.as_deref(),
            settings.model_timeout,
        )?)
    } else {
        let bearer_token = read_optional_secret(&settings.api_key_env)?;
        Box::new(OpenAiChatModel::new(
            &settings.endpoint,
            &settings.model,
            bearer_token,
            settings.model_timeout,
        )?)
    };
    let runtime = ShellRuntime {
        invoker: SessionInvoker {
            direct: RegistryInvoker {
                registry: &registry,
            },
            broker,
        },
        limits: settings.shell,
        curl_capability: settings.curl_capability,
    };

    run_prompt(
        model.as_ref(),
        &runtime,
        &settings.prompt,
        settings.system.as_deref(),
        settings.prompt_limits,
    )
    .map_err(AppError::from)
}

struct CommandOutput {
    text: String,
    exit_code: i32,
}

impl CommandOutput {
    fn success(text: String) -> Self {
        Self { text, exit_code: 0 }
    }

    /// A command that already streamed its own output and has nothing left to print.
    #[cfg(unix)]
    fn silent() -> Self {
        Self {
            text: String::new(),
            exit_code: 0,
        }
    }
}

/// Adapts the direct provider registry to the interpreter's capability seam.
///
/// Direct mode performs no broker transition, so no invocation here can be *denied*: there is no
/// authorization to refuse one. `Denied` stays reachable in the shared vocabulary because a
/// broker-backed invoker will produce it; this adapter simply never does.
struct RegistryInvoker<'a> {
    registry: &'a ProviderRegistry,
}

impl CapabilityInvoker for RegistryInvoker<'_> {
    fn granted(&self) -> Vec<String> {
        self.registry
            .capabilities()
            .map(|(_provider, capability)| capability.id.to_string())
            .collect()
    }

    fn is_granted(&self, capability: &str) -> bool {
        capability.parse::<CapabilityId>().is_ok_and(|capability| {
            self.registry
                .capabilities()
                .any(|(_provider, candidate)| candidate.id == capability)
        })
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        let capability = capability.parse::<CapabilityId>().ok()?;
        self.registry
            .capabilities()
            .find(|(_provider, candidate)| candidate.id == capability)
            .map(|(_provider, candidate)| CapabilityDescription {
                capability: candidate.id.to_string(),
                description: candidate.description.clone(),
                input_schema: candidate.input_schema.clone(),
            })
    }

    fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
        let Ok(capability) = capability.parse::<CapabilityId>() else {
            return CapabilityCallResult::NotFound;
        };
        match self.registry.invoke(&capability, &input) {
            Ok(output) => CapabilityCallResult::Succeeded(output.output),
            Err(ProviderHostError::UnknownCapability { .. }) => CapabilityCallResult::NotFound,
            Err(error) => CapabilityCallResult::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// Connects the prompt session's optional broker leg before any blocking work starts.
///
/// The capability set is snapshotted here, on the async side, for two reasons. It lets `cap --list`
/// answer for both legs without a round trip per script, and it turns "the daemon is not running"
/// into one clear startup failure instead of a capability that inexplicably reports "command not
/// found" halfway through a script a model already committed to.
#[cfg(unix)]
async fn connect_prompt_broker(
    enabled: bool,
    connection: &BrokerConnectionArgs,
) -> Result<Option<Box<dyn CapabilityInvoker + Send>>, AppError> {
    if !enabled {
        return Ok(None);
    }
    let socket = BrokerSocketDiscovery::from_process(connection.socket.clone()).resolve()?;
    let server_uid = resolve_broker_server_uid(connection.server_uid);
    let client = BrokerClient::new(
        &socket,
        server_uid,
        FrameLimits {
            max_frame_bytes: connection.max_frame_bytes,
            io_timeout: Duration::from_millis(connection.io_timeout_ms),
        },
    )?;
    let leg = BrokerLeg::connect(client, "dekopon-run-prompt")
        .await
        .map_err(|error| match error {
            BrokerLegError::Client(source) => AppError::BrokerClient(source),
            BrokerLegError::SessionIdentifier(source) => AppError::SessionIdentifier(source),
        })?;
    tracing::info!(
        target: "dekopon_run::audit",
        {
            capability.count = leg.granted().len(),
        },
        "broker leg connected for prompt session"
    );

    Ok(Some(Box::new(leg)))
}

#[cfg(not(unix))]
async fn connect_prompt_broker(
    enabled: bool,
    _connection: &BrokerConnectionArgs,
) -> Result<Option<Box<dyn CapabilityInvoker + Send>>, AppError> {
    if enabled {
        return Err(AppError::BrokerUnsupported);
    }
    Ok(None)
}

fn shell_limits(limits: &ShellLimitArgs) -> ShellLimits {
    ShellLimits {
        max_steps: limits.shell_max_steps,
        max_recursion_depth: limits.shell_max_recursion_depth,
        max_output_bytes: limits.shell_max_output_bytes,
        max_output_lines: limits.shell_max_output_lines,
        timeout: Duration::from_millis(limits.shell_timeout_ms),
        max_capability_calls: limits.shell_max_capability_calls,
        max_value_bytes: limits.shell_max_value_bytes,
        allow_clock: limits.shell_allow_clock,
    }
}

fn host_limits(limits: &LimitArgs) -> HostLimits {
    HostLimits {
        max_memory_bytes: limits.max_memory_bytes,
        max_input_bytes: limits.max_input_bytes,
        max_output_bytes: limits.max_output_bytes,
        fuel: limits.fuel,
        timeout: Duration::from_millis(limits.timeout_ms),
        // Table, instance, and memory-count ceilings have no command-line flag; the host defaults
        // bound the allocation paths `--max-memory-bytes` does not reach.
        ..HostLimits::default()
    }
}

#[cfg(unix)]
async fn evaluate_broker(command: &BrokerCommand) -> Result<CommandOutput, AppError> {
    const ENVELOPE_RESERVE_BYTES: usize = 4 * 1024;

    match command {
        BrokerCommand::Capabilities { connection } => {
            async {
                let socket =
                    BrokerSocketDiscovery::from_process(connection.socket.clone()).resolve()?;
                let server_uid = resolve_broker_server_uid(connection.server_uid);
                let client = BrokerClient::new(
                    &socket,
                    server_uid,
                    FrameLimits {
                        max_frame_bytes: connection.max_frame_bytes,
                        io_timeout: Duration::from_millis(connection.io_timeout_ms),
                    },
                )?;
                let capabilities = client.capabilities().await?;
                serde_json::to_string_pretty(&capabilities)
                    .map(CommandOutput::success)
                    .map_err(AppError::Serialize)
            }
            .instrument(tracing::info_span!("runner.broker.capabilities"))
            .await
        }
        BrokerCommand::Invoke {
            connection,
            capability,
            invocation_id,
            trace_id,
            input,
            input_file,
        } => {
            async {
                let socket =
                    BrokerSocketDiscovery::from_process(connection.socket.clone()).resolve()?;
                let server_uid = resolve_broker_server_uid(connection.server_uid);
                let client = BrokerClient::new(
                    &socket,
                    server_uid,
                    FrameLimits {
                        max_frame_bytes: connection.max_frame_bytes,
                        io_timeout: Duration::from_millis(connection.io_timeout_ms),
                    },
                )?;
                let input = read_input(
                    input.as_deref(),
                    input_file.as_deref(),
                    connection
                        .max_frame_bytes
                        .saturating_sub(ENVELOPE_RESERVE_BYTES),
                )?;
                if !input.is_object() {
                    return Err(AppError::BrokerInputObject);
                }
                let result = client
                    .invoke(InvocationRequest {
                        id: invocation_id.clone(),
                        capability: capability.clone(),
                        trace: trace_id.clone(),
                        trace_parent: dekopon_agent::current_trace_parent(),
                        input,
                    })
                    .await?;
                let exit_code = if result.outcome == InvocationOutcome::Succeeded {
                    0
                } else {
                    1
                };
                serde_json::to_string_pretty(&result)
                    .map(|text| CommandOutput { text, exit_code })
                    .map_err(AppError::Serialize)
            }
            .instrument(tracing::info_span!(
                "runner.broker.invoke",
                capability.id = %capability,
                invocation.id = %invocation_id,
                trace.id = %trace_id
            ))
            .await
        }
    }
}

#[cfg(not(unix))]
async fn evaluate_broker(_command: &BrokerCommand) -> Result<CommandOutput, AppError> {
    Err(AppError::BrokerUnsupported)
}

/// Inputs used to resolve the broker socket precedence.
///
/// Unlike configuration discovery, no candidate is probed for existence: a broker socket is
/// absent whenever the daemon is not running, so the tightest resolved tier is always trusted
/// and connection failures are reported against that exact path.
#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct BrokerSocketDiscovery {
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    home: Option<PathBuf>,
}

#[cfg(unix)]
impl BrokerSocketDiscovery {
    /// Captures discovery inputs from the current process.
    fn from_process(explicit: Option<PathBuf>) -> Self {
        Self {
            explicit,
            environment: env::var_os("DEKOPON_BROKER_SOCKET")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            xdg_runtime_dir: env::var_os("XDG_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            home: env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Creates an injectable discovery context for deterministic tests.
    #[cfg(test)]
    fn new(
        explicit: Option<PathBuf>,
        environment: Option<PathBuf>,
        xdg_runtime_dir: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            explicit,
            environment,
            xdg_runtime_dir,
            home,
        }
    }

    /// Resolves the highest-precedence broker socket path.
    fn resolve(&self) -> Result<PathBuf, AppError> {
        if let Some(path) = &self.explicit {
            return Ok(path.clone());
        }
        if let Some(path) = &self.environment {
            return Ok(path.clone());
        }
        if let Some(root) = &self.xdg_runtime_dir {
            return Ok(root.join("dekopon/broker.sock"));
        }
        if let Some(home) = &self.home {
            return Ok(home.join(".local/run/dekopon/broker.sock"));
        }
        Err(AppError::BrokerSocketUnresolved)
    }
}

/// Resolves the trusted broker server UID, defaulting to the caller's own effective UID.
#[cfg(unix)]
fn resolve_broker_server_uid(explicit: Option<u32>) -> u32 {
    explicit.unwrap_or_else(|| rustix::process::geteuid().as_raw())
}

fn read_input(
    inline: Option<&str>,
    path: Option<&Path>,
    maximum: usize,
) -> Result<Value, AppError> {
    let source = match (inline, path) {
        (Some(input), None) => {
            if input.len() > maximum {
                return Err(AppError::InputTooLarge {
                    length: input.len(),
                    maximum,
                });
            }
            input.to_owned()
        }
        (None, Some(path)) if path == Path::new("-") => {
            let stdin = io::stdin();
            read_bounded(stdin.lock(), "stdin", maximum)?
        }
        (None, Some(path)) => {
            let file = File::open(path).map_err(|source| AppError::ReadInput {
                path: path.to_path_buf(),
                source,
            })?;
            read_bounded(file, &path.display().to_string(), maximum)?
        }
        (None, None) => "{}".to_owned(),
        (Some(_), Some(_)) => unreachable!("Clap rejects conflicting input sources"),
    };

    serde_json::from_str(&source).map_err(AppError::ParseInput)
}

fn read_bounded(reader: impl Read, source_name: &str, maximum: usize) -> Result<String, AppError> {
    let read_limit = u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::new();
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|source| AppError::ReadInputStream {
            source_name: source_name.to_owned(),
            source,
        })?;
    if bytes.len() > maximum {
        return Err(AppError::InputTooLarge {
            length: bytes.len(),
            maximum,
        });
    }
    String::from_utf8(bytes).map_err(|source| AppError::InputUtf8 {
        source_name: source_name.to_owned(),
        source,
    })
}

fn read_optional_secret(variable: &str) -> Result<Option<String>, AppError> {
    if variable.trim().is_empty() {
        return Err(AppError::Environment(
            "API key environment variable name must not be empty".to_owned(),
        ));
    }
    let Some(value) = env::var_os(variable) else {
        return Ok(None);
    };
    value
        .into_string()
        .map(Some)
        .map_err(|_| AppError::Environment(format!("environment variable {variable} is not UTF-8")))
}

fn write_output(output: &str) -> io::Result<()> {
    // A command that printed as it went returns nothing here, and a bare newline would append a
    // blank line to output it already finished.
    if output.is_empty() {
        return Ok(());
    }
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(output.as_bytes())?;
    if !output.ends_with('\n') {
        handle.write_all(b"\n")?;
    }
    handle.flush()
}

fn report_error(error: &AppError, verbosity: u8) {
    eprintln!("error: {error}");
    if verbosity > 0 {
        let mut source = error.source();
        while let Some(cause) = source {
            eprintln!("  caused by: {cause}");
            source = cause.source();
        }
    }
    if verbosity > 1 {
        eprintln!("  debug: {error:#?}");
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InvocationReport {
    provider: ProviderId,
    capability: CapabilityId,
    iterations: u32,
    timing: TimingReport,
    output: Value,
}

impl InvocationReport {
    fn new(
        provider: ProviderId,
        capability: CapabilityId,
        iterations: u32,
        total: Duration,
        samples: &TimingSamples,
        output: Value,
    ) -> Self {
        let minimum = samples.minimum.unwrap_or_default();
        let maximum = samples.maximum.unwrap_or_default();
        let mean = samples.total / iterations;

        Self {
            provider,
            capability,
            iterations,
            timing: TimingReport {
                total_ms: milliseconds(total),
                min_ms: milliseconds(minimum),
                mean_ms: milliseconds(mean),
                max_ms: milliseconds(maximum),
            },
            output,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimingReport {
    total_ms: f64,
    min_ms: f64,
    mean_ms: f64,
    max_ms: f64,
}

#[derive(Debug, Default)]
struct TimingSamples {
    total: Duration,
    minimum: Option<Duration>,
    maximum: Option<Duration>,
}

impl TimingSamples {
    fn record(&mut self, sample: Duration) {
        self.total = self.total.saturating_add(sample);
        self.minimum = Some(self.minimum.map_or(sample, |current| current.min(sample)));
        self.maximum = Some(self.maximum.map_or(sample, |current| current.max(sample)));
    }
}

pub(crate) fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Debug, Error)]
enum AppError {
    #[cfg(unix)]
    #[error(transparent)]
    BrokerClient(#[from] ClientError),
    #[cfg(unix)]
    #[error("could not determine broker socket path; pass --socket or set DEKOPON_BROKER_SOCKET")]
    BrokerSocketUnresolved,
    #[cfg(not(unix))]
    #[error("broker client mode requires Unix peer credentials and Unix-domain sockets")]
    BrokerUnsupported,
    #[cfg(unix)]
    #[error(transparent)]
    Chat(#[from] chat::ChatError),
    #[cfg(not(unix))]
    #[error("the gateway chat client requires a Unix-domain socket")]
    ChatUnsupported,
    #[error(
        "broker connection flags were supplied without --broker; \
         a prompt session contacts no broker until you ask it to"
    )]
    BrokerFlagsWithoutOptIn,
    #[cfg(unix)]
    #[error("could not derive a unique identifier for this broker session")]
    SessionIdentifier(#[source] IdentifierError),
    #[error("the prompt session did not run to completion")]
    PromptTask(#[source] tokio::task::JoinError),
    #[error("broker capability input must be a JSON object")]
    BrokerInputObject,
    #[error(transparent)]
    ChatGpt(#[from] ChatGptError),
    #[error(transparent)]
    Provider(#[from] ProviderHostError),
    #[error(transparent)]
    ProviderArgs(#[from] cli::ProviderArgsError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("could not read input file {}", path.display())]
    ReadInput {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read capability input from {source_name}")]
    ReadInputStream {
        source_name: String,
        #[source]
        source: io::Error,
    },
    #[error("capability input from {source_name} is not UTF-8")]
    InputUtf8 {
        source_name: String,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("capability input is {length} bytes; the maximum is {maximum}")]
    InputTooLarge { length: usize, maximum: usize },
    #[error("capability input is not valid JSON")]
    ParseInput(#[source] serde_json::Error),
    #[error("could not serialize command output")]
    Serialize(#[source] serde_json::Error),
    #[error("invalid environment configuration: {0}")]
    Environment(String),
}

impl AppError {
    /// Stable, low-cardinality failure category for telemetry.
    ///
    /// Deliberately not the error's own message: several variants wrap untrusted provider,
    /// transport, or model text, and `docs/observability.md` keeps that text out of exported
    /// telemetry. An operator correlates the category here with the full message on stderr.
    fn telemetry_kind(&self) -> &'static str {
        match self {
            #[cfg(unix)]
            Self::BrokerClient(_) => "broker-client",
            #[cfg(unix)]
            Self::BrokerSocketUnresolved => "broker-socket-unresolved",
            #[cfg(not(unix))]
            Self::BrokerUnsupported => "broker-unsupported",
            // Delegated rather than flattened: a chat session fails in a dozen distinct ways and
            // collapsing them to one category would hide which end of the socket went wrong.
            #[cfg(unix)]
            Self::Chat(error) => error.telemetry_kind(),
            #[cfg(not(unix))]
            Self::ChatUnsupported => "chat-unsupported",
            Self::BrokerFlagsWithoutOptIn => "broker-flags-without-opt-in",
            #[cfg(unix)]
            Self::SessionIdentifier(_) => "session-identifier",
            Self::PromptTask(_) => "prompt-task",
            Self::BrokerInputObject => "broker-input-object",
            Self::ChatGpt(_) => "chatgpt",
            Self::Provider(_) => "provider",
            Self::ProviderArgs(_) => "provider-args",
            Self::Model(_) => "model",
            Self::Prompt(_) => "prompt",
            Self::ReadInput { .. } => "input-read",
            Self::ReadInputStream { .. } => "input-stream-read",
            Self::InputUtf8 { .. } => "input-utf8",
            Self::InputTooLarge { .. } => "input-too-large",
            Self::ParseInput(_) => "input-json",
            Self::Serialize(_) => "output-serialize",
            Self::Environment(_) => "environment",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use serde_json::json;

    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    use super::{AppError, BrokerSocketDiscovery, resolve_broker_server_uid};
    use super::{InvocationReport, TimingSamples, read_input};

    #[test]
    fn defaults_direct_invocations_to_an_empty_object() {
        assert_eq!(
            read_input(None, None, 1024).expect("default input"),
            json!({})
        );
    }

    #[test]
    fn reads_json_input_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("input.json");
        fs::write(&path, r#"{"message":"hello"}"#).expect("fixture writes");

        assert_eq!(
            read_input(None, Some(&path), 1024).expect("file input"),
            json!({"message": "hello"})
        );
    }

    #[test]
    fn timing_reports_include_all_samples() {
        let mut samples = TimingSamples::default();
        samples.record(Duration::from_millis(2));
        samples.record(Duration::from_millis(4));
        let report = InvocationReport::new(
            "echo".parse().expect("valid provider"),
            "echo.echo".parse().expect("valid capability"),
            2,
            Duration::from_millis(7),
            &samples,
            json!({}),
        );

        assert_eq!(report.timing.total_ms, 7.0);
        assert_eq!(report.timing.min_ms, 2.0);
        assert_eq!(report.timing.mean_ms, 3.0);
        assert_eq!(report.timing.max_ms, 4.0);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_broker_socket_outranks_every_default() {
        let discovery = BrokerSocketDiscovery::new(
            Some(PathBuf::from("/explicit/broker.sock")),
            Some(PathBuf::from("/environment/broker.sock")),
            Some(PathBuf::from("/run/user/1000")),
            Some(PathBuf::from("/home/dekopon")),
        );

        assert_eq!(
            discovery.resolve().expect("explicit socket"),
            PathBuf::from("/explicit/broker.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn broker_socket_environment_outranks_runtime_and_home_defaults() {
        let discovery = BrokerSocketDiscovery::new(
            None,
            Some(PathBuf::from("/environment/broker.sock")),
            Some(PathBuf::from("/run/user/1000")),
            Some(PathBuf::from("/home/dekopon")),
        );

        assert_eq!(
            discovery.resolve().expect("environment socket"),
            PathBuf::from("/environment/broker.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn broker_socket_runtime_directory_outranks_the_home_default() {
        let discovery = BrokerSocketDiscovery::new(
            None,
            None,
            Some(PathBuf::from("/run/user/1000")),
            Some(PathBuf::from("/home/dekopon")),
        );

        assert_eq!(
            discovery.resolve().expect("runtime socket"),
            PathBuf::from("/run/user/1000/dekopon/broker.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn broker_socket_falls_back_to_the_documented_home_path() {
        let discovery =
            BrokerSocketDiscovery::new(None, None, None, Some(PathBuf::from("/home/dekopon")));

        assert_eq!(
            discovery.resolve().expect("home socket"),
            PathBuf::from("/home/dekopon/.local/run/dekopon/broker.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unresolvable_broker_socket_reports_actionable_guidance() {
        let error = BrokerSocketDiscovery::new(None, None, None, None)
            .resolve()
            .expect_err("no socket candidate");

        assert!(matches!(error, AppError::BrokerSocketUnresolved));
    }

    #[cfg(unix)]
    #[test]
    fn broker_server_uid_defaults_to_the_calling_process() {
        assert_eq!(
            resolve_broker_server_uid(None),
            rustix::process::geteuid().as_raw()
        );
        assert_eq!(resolve_broker_server_uid(Some(4242)), 4242);
    }

    // Composite dispatch and broker-leg behavior are covered in `dekopon-agent`, where those
    // types now live.
}
