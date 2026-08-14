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
use std::{
    collections::BTreeMap,
    collections::hash_map::RandomState,
    hash::{BuildHasher as _, Hasher as _},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use dekopon_broker_protocol::{
    BrokerClient, ClientError, ERROR_UNAUTHENTICATED, FrameLimits, InvocationOutcome,
    InvocationRequest,
};
use dekopon_core::{CapabilityId, ProviderId};
#[cfg(unix)]
use dekopon_core::{IdentifierError, InvocationId, TraceId};
use dekopon_model::{
    chatgpt::{ChatGptCodexModel, ChatGptError},
    model::{ChatModel, ModelError, OpenAiChatModel},
};
use dekopon_provider_host::{HostLimits, ProviderHostError, ProviderManifest, ProviderRegistry};
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, Interpreter,
    Limits as ShellLimits, ScriptOutcome,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tracing::Instrument as _;

use crate::{
    cli::{BrokerCommand, BrokerConnectionArgs, Cli, Command, LimitArgs, ShellLimitArgs},
    prompt::{
        PromptError, PromptLimits, ScriptRuntime, format_script_outcome, run_prompt,
        script_outcome_label,
    },
};

pub mod cli;
pub mod prompt;
mod trace;

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
        tracing::info!(
            target: "dekopon_run::audit",
            {
                audit.event = "runner.command.started",
                command.name = command_name,
            },
            "runner command started"
        );

        match evaluate(&cli).await {
            Ok(output) => {
                let exit_code = match write_output(&output.text) {
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
                };
                tracing::info!(
                    target: "dekopon_run::audit",
                    {
                        audit.event = "runner.command.completed",
                        command.name = command_name,
                        command.exit_code = exit_code,
                    },
                    "runner command completed"
                );
                exit_code
            }
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
    }
}

async fn evaluate(cli: &Cli) -> Result<CommandOutput, AppError> {
    match &cli.command {
        Command::Inspect { limits, providers } => {
            let span =
                tracing::info_span!("runner.inspect", provider.count = providers.provider.len());
            let _entered = span.enter();
            let registry = ProviderRegistry::load(providers.provider.clone(), host_limits(limits))?;
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
            let span = tracing::info_span!(
                "runner.invoke",
                provider.count = providers.provider.len(),
                capability.id = %capability,
                invocation.count = repeat.get()
            );
            let _entered = span.enter();
            let input = read_input(
                input.as_deref(),
                input_file.as_deref(),
                limits.max_input_bytes,
            )?;
            let registry = ProviderRegistry::load(providers.provider.clone(), host_limits(limits))?;
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
                tracing::info!(
                    target: "dekopon_run::audit",
                    {
                        audit.event = "guest.invocation.started",
                        capability.id = %capability,
                        invocation.iteration = iteration,
                    },
                    "guest provider invocation started"
                );
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
            let span = tracing::info_span!(
                "runner.shell",
                provider.count = providers.provider.len(),
                shell.max_steps = shell.shell_max_steps,
                shell.max_capability_calls = shell.shell_max_capability_calls
            );
            let _entered = span.enter();
            let registry = ProviderRegistry::load(providers.provider.clone(), host_limits(limits))?;
            let invoker = RegistryInvoker {
                registry: &registry,
            };
            let outcome = Interpreter::new(shell_limits(shell))
                .with_curl_capability(curl_capability.as_ref().map(CapabilityId::to_string))
                .run(script, &invoker);
            tracing::info!(
                target: "dekopon_run::audit",
                {
                    audit.event = "shell.script.completed",
                    shell.exit_code = outcome.exit_code.get(),
                    shell.steps = outcome.steps,
                    provider.invocations = outcome.capability_calls,
                    shell.truncated = outcome.truncated,
                    outcome = script_outcome_label(&outcome),
                },
                "shell script completed"
            );
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
            let settings = PromptSettings {
                limits: host_limits(limits),
                shell: shell_limits(shell),
                curl_capability: curl_capability.as_ref().map(CapabilityId::to_string),
                providers: providers.provider.clone(),
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
                    provider.count = providers.provider.len(),
                    model = %model,
                    model.backend = backend,
                    prompt.max_steps = max_steps.get(),
                    prompt.broker = *broker
                ))
                .await
        }
    }
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

    tracing::info!(
        target: "dekopon_run::audit",
        {
            audit.event = "agent.session.completed",
            model.turns = outcome.model_turns,
            script.calls = outcome.script_calls,
            provider.invocations = outcome.capability_invocations,
            outcome = "succeeded",
        },
        "prompt session completed"
    );
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

/// Runs each model-authored script on the interpreter under this session's dispatch.
struct ShellRuntime<I> {
    invoker: I,
    limits: ShellLimits,
    curl_capability: Option<String>,
}

impl<I: CapabilityInvoker> ScriptRuntime for ShellRuntime<I> {
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
        // A fresh interpreter per script, but not a fresh budget: the prompt loop spends one
        // capability allowance across the whole session, so this script gets whatever the earlier
        // ones left. Exhausting it trips the interpreter's own ceiling, with the message and exit
        // code Phase 1 already established, rather than inventing a second way to say "no".
        let limits = ShellLimits {
            max_capability_calls: self.limits.max_capability_calls.min(max_capability_calls),
            ..self.limits
        };
        Interpreter::new(limits)
            .with_curl_capability(self.curl_capability.clone())
            .run(script, &self.invoker)
    }
}

/// Dispatches a script's commands to direct-mode providers first and a broker second.
///
/// The order is not arbitrary. A direct component call is local, synchronous, and unauthorized by
/// construction — the linker is import-free, so the component cannot reach anything. Preferring it
/// keeps every capability that *can* run without a broker transition doing exactly that, and
/// leaves the broker leg for what direct mode provably cannot reach: anything performing I/O.
struct SessionInvoker<D> {
    direct: D,
    broker: Option<Box<dyn CapabilityInvoker + Send>>,
}

impl<D: CapabilityInvoker> CapabilityInvoker for SessionInvoker<D> {
    fn granted(&self) -> Vec<String> {
        let mut granted = self.direct.granted();
        if let Some(broker) = &self.broker {
            granted.extend(broker.granted());
        }
        granted.sort_unstable();
        granted.dedup();
        granted
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.direct.is_granted(capability)
            || self
                .broker
                .as_ref()
                .is_some_and(|broker| broker.is_granted(capability))
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.direct.describe(capability).or_else(|| {
            self.broker
                .as_ref()
                .and_then(|broker| broker.describe(capability))
        })
    }

    fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
        if self.direct.is_granted(capability) {
            return self.direct.invoke(capability, input);
        }
        match &self.broker {
            Some(broker) => broker.invoke(capability, input),
            None => CapabilityCallResult::NotFound,
        }
    }
}

struct CommandOutput {
    text: String,
    exit_code: i32,
}

impl CommandOutput {
    fn success(text: String) -> Self {
        Self { text, exit_code: 0 }
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
    let capabilities = client
        .capabilities()
        .await?
        .into_iter()
        .map(|available| {
            (
                available.capability.id.to_string(),
                CapabilityDescription {
                    capability: available.capability.id.to_string(),
                    description: available.capability.description,
                    input_schema: available.capability.input_schema,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    tracing::info!(
        target: "dekopon_run::audit",
        {
            capability.count = capabilities.len(),
        },
        "broker leg connected for prompt session"
    );

    Ok(Some(Box::new(BrokerLeg {
        client,
        runtime: tokio::runtime::Handle::current(),
        capabilities,
        identifiers: IdSequence::new()?,
    })))
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

/// The broker half of a prompt session's capability dispatch.
///
/// This is a client of `dekopon-brokerd`'s authorization path, never a participant in it: it
/// submits an identity-free proposal and reports back whatever the broker decided. Nothing here
/// interprets policy, and nothing here can mint authorization.
#[cfg(unix)]
struct BrokerLeg {
    client: BrokerClient,
    runtime: tokio::runtime::Handle,
    capabilities: BTreeMap<String, CapabilityDescription>,
    identifiers: IdSequence,
}

#[cfg(unix)]
impl CapabilityInvoker for BrokerLeg {
    fn granted(&self) -> Vec<String> {
        self.capabilities.keys().cloned().collect()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.capabilities.contains_key(capability)
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        self.capabilities.get(capability).cloned()
    }

    fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
        let Ok(parsed) = capability.parse::<CapabilityId>() else {
            return CapabilityCallResult::NotFound;
        };
        // A visibility check, deliberately not an authorization one. Bare-word dispatch already
        // filters on `is_granted`, but the `cap <id>` escape hatch does not, so without this a
        // script could spend its whole capability budget probing the broker with guessed
        // identifiers. What this must never do is decide a *refusal*: anything policy makes
        // visible goes to the broker and comes back with the broker's own answer, including the
        // denials that only it can issue.
        if !self.capabilities.contains_key(capability) {
            return CapabilityCallResult::NotFound;
        }
        let Ok(id) = self.identifiers.next_invocation() else {
            return CapabilityCallResult::Failed {
                error: "could not derive a unique invocation identifier".to_owned(),
            };
        };
        let request = InvocationRequest {
            id,
            capability: parsed,
            trace: self.identifiers.trace().clone(),
            input,
        };

        // Safe specifically because this runs on a `spawn_blocking` thread rather than a runtime
        // worker: `Handle::block_on` from a worker would deadlock the executor, and from the
        // blocking pool it is the ordinary bridge back into async code.
        match self.runtime.block_on(self.client.invoke(request)) {
            Ok(result) => match result.outcome {
                InvocationOutcome::Succeeded => {
                    CapabilityCallResult::Succeeded(result.output.unwrap_or(Value::Null))
                }
                // A refusal has to stay a refusal all the way to the script's exit code. The
                // interpreter maps `Denied` to 126 and `Failed` to 1, and a model that reads
                // "policy said no" as "the call errored" will retry something it must not retry.
                InvocationOutcome::Denied => CapabilityCallResult::Denied {
                    reason: result
                        .error
                        .unwrap_or_else(|| "authorization refused this invocation".to_owned()),
                },
                InvocationOutcome::Failed => CapabilityCallResult::Failed {
                    error: result
                        .error
                        .unwrap_or_else(|| "the broker reported a failed invocation".to_owned()),
                },
            },
            // An unmapped peer is an authorization refusal that never reached a decision record,
            // so it arrives as a transport-level code rather than a `Denied` outcome. It is still
            // a refusal, and collapsing it into a generic failure would tell a model to retry.
            Err(ClientError::Remote { code, message }) if code == ERROR_UNAUTHENTICATED => {
                CapabilityCallResult::Denied { reason: message }
            }
            // Every `ClientError` renders without the socket path, so a script cannot learn where
            // the broker lives — the interpreter refuses to read the process environment, and this
            // is the one path that could otherwise leak `DEKOPON_BROKER_SOCKET` back into it.
            Err(error) => CapabilityCallResult::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// Generates the trace and invocation identifiers one prompt session needs.
///
/// The broker treats an invocation identifier as a durable replay-rejection key, so two calls must
/// never share one and a script that calls the same capability in a loop must not collide with
/// itself. Nothing in this workspace generates randomness, and a dependency is not worth 64 bits
/// of it, so the session prefix mixes an OS-seeded `RandomState` key with the process ID and a
/// wall-clock reading, and a monotonic counter makes collisions *within* a session impossible
/// rather than merely unlikely. Invocation identifiers extend the session trace, so every call a
/// session made is recoverable from the broker's audit log by prefix.
#[cfg(unix)]
struct IdSequence {
    trace: TraceId,
    next: AtomicU32,
}

#[cfg(unix)]
impl IdSequence {
    fn new() -> Result<Self, AppError> {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default(),
        );
        let trace = format!("dekopon-run-prompt-{:016x}", hasher.finish())
            .parse::<TraceId>()
            .map_err(AppError::SessionIdentifier)?;
        Ok(Self {
            trace,
            next: AtomicU32::new(1),
        })
    }

    fn trace(&self) -> &TraceId {
        &self.trace
    }

    fn next_invocation(&self) -> Result<InvocationId, IdentifierError> {
        let counter = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{}-{counter}", self.trace).parse()
    }
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
    }
}

fn host_limits(limits: &LimitArgs) -> HostLimits {
    HostLimits {
        max_memory_bytes: limits.max_memory_bytes,
        max_input_bytes: limits.max_input_bytes,
        max_output_bytes: limits.max_output_bytes,
        fuel: limits.fuel,
        timeout: Duration::from_millis(limits.timeout_ms),
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
            Self::BrokerFlagsWithoutOptIn => "broker-flags-without-opt-in",
            #[cfg(unix)]
            Self::SessionIdentifier(_) => "session-identifier",
            Self::PromptTask(_) => "prompt-task",
            Self::BrokerInputObject => "broker-input-object",
            Self::ChatGpt(_) => "chatgpt",
            Self::Provider(_) => "provider",
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

    // -----------------------------------------------------------------------
    // Composite dispatch
    // -----------------------------------------------------------------------

    use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker};
    use serde_json::Value;

    use super::SessionInvoker;

    /// A leg that answers for a fixed capability set and records what it was asked to run.
    struct FakeLeg {
        capability: &'static str,
        marker: &'static str,
        invoked: std::sync::Mutex<Vec<String>>,
    }

    impl FakeLeg {
        fn new(capability: &'static str, marker: &'static str) -> Self {
            Self {
                capability,
                marker,
                invoked: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CapabilityInvoker for FakeLeg {
        fn granted(&self) -> Vec<String> {
            vec![self.capability.to_owned()]
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            (capability == self.capability).then(|| CapabilityDescription {
                capability: capability.to_owned(),
                description: self.marker.to_owned(),
                input_schema: json!({"type": "object"}),
            })
        }

        fn invoke(&self, capability: &str, _input: Value) -> CapabilityCallResult {
            if capability != self.capability {
                return CapabilityCallResult::NotFound;
            }
            self.invoked
                .lock()
                .expect("invocation lock")
                .push(capability.to_owned());
            CapabilityCallResult::Succeeded(json!({ "leg": self.marker }))
        }
    }

    #[test]
    fn direct_capabilities_are_preferred_over_the_broker() {
        // A capability reachable without a broker transition must never take one: the direct call
        // is local and unauthorized by construction, so routing it through the broker would add an
        // authorization decision, an audit record, and a round trip for no gain.
        let shared = Box::new(FakeLeg::new("shared.capability", "broker"));
        let invoker = SessionInvoker {
            direct: FakeLeg::new("shared.capability", "direct"),
            broker: Some(shared),
        };

        assert_eq!(
            invoker.invoke("shared.capability", json!({})),
            CapabilityCallResult::Succeeded(json!({"leg": "direct"}))
        );
    }

    #[test]
    fn capabilities_absent_from_direct_mode_fall_through_to_the_broker() {
        let invoker = SessionInvoker {
            direct: FakeLeg::new("echo.echo", "direct"),
            broker: Some(Box::new(FakeLeg::new("http-probe.fetch", "broker"))),
        };

        assert_eq!(
            invoker.invoke("http-probe.fetch", json!({})),
            CapabilityCallResult::Succeeded(json!({"leg": "broker"}))
        );
        assert!(invoker.is_granted("http-probe.fetch"));
        assert_eq!(
            invoker.granted(),
            vec!["echo.echo".to_owned(), "http-probe.fetch".to_owned()]
        );
        assert_eq!(
            invoker
                .describe("http-probe.fetch")
                .map(|it| it.description),
            Some("broker".to_owned())
        );
    }

    #[test]
    fn a_session_without_a_broker_is_exactly_as_capable_as_direct_mode() {
        // Omitting `--broker` has to leave prompt mode behaving as it did before this leg existed,
        // so a local demo or a CI run with no daemon is unaffected.
        let invoker = SessionInvoker {
            direct: FakeLeg::new("echo.echo", "direct"),
            broker: None,
        };

        assert_eq!(invoker.granted(), vec!["echo.echo".to_owned()]);
        assert!(!invoker.is_granted("http-probe.fetch"));
        assert_eq!(
            invoker.invoke("http-probe.fetch", json!({})),
            CapabilityCallResult::NotFound
        );
    }

    // -----------------------------------------------------------------------
    // Broker leg
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    mod broker_leg {
        use std::{collections::BTreeMap, os::unix::fs::PermissionsExt as _, path::Path};

        use dekopon_broker_protocol::{
            BrokerClient, ERROR_UNAUTHENTICATED, FrameLimits, InvocationOutcome, InvocationResult,
            RequestEnvelope, ResponseEnvelope, read_frame, write_frame,
        };
        use dekopon_capability::DecisionReference;
        use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker};
        use serde_json::json;
        use tokio::net::UnixListener;

        use crate::{BrokerLeg, IdSequence, resolve_broker_server_uid};

        const CAPABILITY: &str = "http-probe.fetch";

        fn result(outcome: InvocationOutcome, error: Option<&str>) -> InvocationResult {
            InvocationResult {
                invocation: "invoke-stub".parse().expect("valid invocation fixture"),
                decision: DecisionReference {
                    decision_id: "decision-stub".to_owned(),
                    authorized_by: "broker-stub".parse().expect("valid principal fixture"),
                    policy_revision: "policy-stub".to_owned(),
                },
                outcome,
                output: matches!(outcome, InvocationOutcome::Succeeded)
                    .then(|| json!({"status": 200})),
                error: error.map(str::to_owned),
                evidence: Vec::new(),
            }
        }

        /// Serves a fixed script of responses over a private Unix socket.
        ///
        /// A real socket rather than an in-memory duplex, because the client authenticates the
        /// server by socket ownership and peer UID before it writes a byte; a stub that skipped
        /// that would not be exercising the path the runner actually takes.
        async fn stub_leg(directory: &Path, responses: Vec<ResponseEnvelope>) -> BrokerLeg {
            let socket = directory.join("broker.sock");
            let listener = UnixListener::bind(&socket).expect("bind stub broker");
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .expect("secure stub socket");
            tokio::spawn(async move {
                for response in responses {
                    let (mut stream, _) = listener.accept().await.expect("stub broker accepts");
                    let _request =
                        read_frame::<_, RequestEnvelope>(&mut stream, FrameLimits::default())
                            .await
                            .expect("stub broker reads one request");
                    write_frame(&mut stream, &response, FrameLimits::default())
                        .await
                        .expect("stub broker writes one response");
                }
            });

            leg_for(&socket)
        }

        fn leg_for(socket: &Path) -> BrokerLeg {
            let mut capabilities = BTreeMap::new();
            capabilities.insert(
                CAPABILITY.to_owned(),
                CapabilityDescription {
                    capability: CAPABILITY.to_owned(),
                    description: "Fetches one broker-authorized URI".to_owned(),
                    input_schema: json!({"type": "object"}),
                },
            );
            BrokerLeg {
                client: BrokerClient::new(
                    socket,
                    resolve_broker_server_uid(None),
                    FrameLimits::default(),
                )
                .expect("stub broker client"),
                runtime: tokio::runtime::Handle::current(),
                capabilities,
                identifiers: IdSequence::new().expect("session identifiers"),
            }
        }

        /// Runs one dispatch the way the runner does: from a blocking thread, never a worker.
        async fn invoke(leg: BrokerLeg, capability: &'static str) -> CapabilityCallResult {
            tokio::task::spawn_blocking(move || leg.invoke(capability, json!({"uri": "http://x/"})))
                .await
                .expect("blocking dispatch completes")
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_denied_invocation_stays_denied_all_the_way_to_the_exit_code() {
            // The interpreter maps `Denied` to 126 and `Failed` to 1. A model that reads "policy
            // refused this" as "the call errored" will retry something it must not retry, so this
            // distinction has to survive the whole trip back.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Denied,
                    Some("policy-denied"),
                ))],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied {
                    reason: "policy-denied".to_owned()
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn an_unmapped_peer_is_a_denial_rather_than_an_infrastructure_failure() {
            // This refusal never reaches a decision record, so it arrives as a transport-level
            // code instead of a `Denied` outcome. It is still policy saying no.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "peer is not mapped by broker policy",
                )],
            )
            .await;

            assert!(matches!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Denied { .. }
            ));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_failed_invocation_carries_the_broker_reason_without_becoming_a_denial() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Failed,
                    Some("provider trapped"),
                ))],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Failed {
                    error: "provider trapped".to_owned()
                }
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_successful_invocation_hands_provider_output_to_the_script() {
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = stub_leg(
                directory.path(),
                vec![ResponseEnvelope::invocation(result(
                    InvocationOutcome::Succeeded,
                    None,
                ))],
            )
            .await;

            assert_eq!(
                invoke(leg, CAPABILITY).await,
                CapabilityCallResult::Succeeded(json!({"status": 200}))
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn capabilities_outside_the_session_never_reach_the_broker() {
            // No stub server at all: if this dispatched, the call would fail against a missing
            // socket instead of reporting the capability as absent.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let leg = leg_for(&directory.path().join("absent.sock"));

            assert_eq!(
                invoke(leg, "totally.unknown").await,
                CapabilityCallResult::NotFound
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn transport_failures_never_disclose_where_the_broker_lives() {
            // The interpreter refuses to read the process environment precisely so a script cannot
            // learn about its host. This is the one path that could hand `DEKOPON_BROKER_SOCKET`
            // straight back to a model inside an error string.
            let directory = tempfile::tempdir().expect("temporary broker directory");
            let socket = directory.path().join("dekopon-secret-broker.sock");
            let leg = leg_for(&socket);

            let CapabilityCallResult::Failed { error } = invoke(leg, CAPABILITY).await else {
                panic!("a missing broker socket is an infrastructure failure");
            };
            assert!(!error.contains("dekopon-secret-broker"), "{error}");
            assert!(!error.contains(&socket.display().to_string()), "{error}");
        }

        #[tokio::test]
        async fn invocation_identifiers_are_unique_and_extend_the_session_trace() {
            // The broker treats an invocation ID as a durable replay-rejection key, so a script
            // calling one capability in a loop must not collide with itself.
            let identifiers = IdSequence::new().expect("session identifiers");
            let first = identifiers.next_invocation().expect("first identifier");
            let second = identifiers.next_invocation().expect("second identifier");

            assert_ne!(first, second);
            let trace = identifiers.trace().as_str();
            assert!(first.as_str().starts_with(trace), "{first} vs {trace}");
            assert!(second.as_str().starts_with(trace), "{second} vs {trace}");
            assert!(trace.starts_with("dekopon-run-prompt-"), "{trace}");

            // Two sessions in the same process must not share a key space either.
            let other = IdSequence::new().expect("second session identifiers");
            assert_ne!(identifiers.trace(), other.trace());
        }
    }
}
