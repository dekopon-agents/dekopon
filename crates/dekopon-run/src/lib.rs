//! Immediate-mode provider execution and an unprivileged broker client for Dekopon.
//!
//! Direct and prompt modes load only read-only, import-free provider components. Explicit broker
//! mode loads no component: it submits identity-free proposals to a separate authenticated broker
//! and never constructs or receives authorization state.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    env,
    error::Error as _,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, SystemTimeError, UNIX_EPOCH},
};

#[cfg(unix)]
use dekopon_broker_protocol::BrokerSocketDiscovery;
#[cfg(unix)]
use dekopon_broker_protocol::{BrokerClient, ClientError, InvocationOutcome, InvocationRequest};
use dekopon_config::{Skill, SkillError};
#[cfg(unix)]
use dekopon_core::IdentifierError;
use dekopon_core::{CapabilityId, ExternalSubject, ProviderId};
#[cfg(unix)]
use dekopon_harness::runtime::{BrokerLeg, BrokerLegError};
use dekopon_harness::{
    bootstrap::SessionBootstrap,
    history,
    improvement::ImprovementSuggestion,
    replay::{
        RecordedSession, RecordedToolCall, RecordingError, ReplayInputs, ReplayReport,
        SessionListing, list_sessions, replay,
    },
    runtime::{
        ScriptRuntime, SessionInvoker, ShellRuntime, command_run_from_outcome,
        report_unobserved_command_run,
    },
    session::{self, PromptError, PromptLimits, SessionEngine},
    tools::format_script_outcome,
};
use dekopon_model::{
    chatgpt::{ChatGptCodexModel, ChatGptError},
    model::{ChatModel, ModelError, OpenAiChatModel},
};
use dekopon_process::{ProcessMetadata, ProcessOutcome, ProcessRun, process_fn};
use dekopon_provider_host::{
    HostLimits, HostOptions, ProviderHostError, ProviderManifest, ProviderRegistry,
};
use dekopon_shell::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun, Interpreter,
    Limits as ShellLimits,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::runtime::Handle;
use tracing::Instrument as _;

use crate::{
    cli::{
        BrokerCommand, BrokerConnectionArgs, Cli, Command, LimitArgs, ModelArgs, ObserveArgs,
        ProviderArgs, SessionCommand, SessionSourceArgs, ShellLimitArgs,
    },
    observe::{ObserveError, OpenObserveClient, OpenObserveSettings},
};

#[cfg(unix)]
mod chat;
pub mod cli;
mod observe;
mod trace;

/// Bytes a transcript file or a `--system-file` may occupy.
///
/// A transcript is bounded by the prompt loop's own history and output ceilings, and a standing
/// instruction by a model's context; a file past this is not one of either.
const MAX_TEXT_FILE_BYTES: usize = 64 * 1024 * 1024;

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
    // Instrumented rather than entered: an `Entered` guard held across an `.await` stays on the
    // thread that parked the future, so any future refactor that polls `run` on a worker would
    // silently mis-parent every event emitted there.
    let exit_code = async {
        match evaluate(&cli).await {
            Ok(output) => match output.write_to(&mut io::stdout().lock()) {
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
    }
    .instrument(command_span.clone())
    .await;

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
        Command::Session {
            command: SessionCommand::List { .. },
        } => "session.list",
        Command::Session {
            command: SessionCommand::Show { .. },
        } => "session.show",
        Command::Session {
            command: SessionCommand::Replay { .. },
        } => "session.replay",
        Command::Chat { .. } => "chat",
    }
}

async fn evaluate(cli: &Cli) -> Result<CommandOutput, AppError> {
    match &cli.command {
        Command::Inspect { limits, providers } => {
            let components = providers.components()?;
            let span = tracing::info_span!("runner.inspect", provider.count = components.len());
            let _entered = span.enter();
            let registry = ProviderRegistry::load_with_options(
                components,
                host_limits(limits),
                &providers.host_options(),
            )?;
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
            let registry = ProviderRegistry::load_with_options(
                components,
                host_limits(limits),
                &providers.host_options(),
            )?;
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
                        // A benchmarking loop must not bill the sink for its own iteration count:
                        // `--repeat 10000` would otherwise ship 10,000 records saying the same
                        // thing. The first iteration names the provider and proves the loop ran;
                        // the summary below carries the aggregate, and failures still report
                        // individually because each one is a distinct fact.
                        if iteration == 1 {
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
                        }
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
            if repeat.get() > 1 {
                tracing::info!(
                    target: "dekopon_run::audit",
                    {
                        audit.event = "guest.invocation.summary",
                        provider.id = %report.provider,
                        capability.id = %report.capability,
                        invocation.count = report.iterations,
                        duration_ms = report.timing.total_ms,
                        mean_duration_ms = report.timing.mean_ms,
                        outcome = "succeeded",
                    },
                    "guest provider invocation loop completed"
                );
            }
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
            let provider_count = components.len();
            let host_limits = host_limits(limits);
            let host_options = providers.host_options();
            let interpreter_limits = shell_limits(shell);
            let curl_capability = curl_capability.as_ref().map(CapabilityId::to_string);
            let script = script.clone();
            evaluate_shell(
                components,
                host_limits,
                host_options,
                interpreter_limits,
                curl_capability,
                script,
                cli.verbose,
            )
            .instrument(tracing::info_span!(
                "runner.shell",
                provider.count = provider_count,
                shell.max_steps = shell.shell_max_steps,
                shell.max_capability_calls = shell.shell_max_capability_calls
            ))
            .await
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
            system,
            skill,
            suggestions,
            max_steps,
            prompt,
        } => {
            let components = providers.components()?;
            // Read before any model call, so a skill that does not load is a usage failure
            // naming the directory rather than a session that ran without it.
            let skills = load_skills(skill)?;
            let settings = PromptSettings {
                limits: host_limits(limits),
                options: providers.host_options(),
                shell: shell_limits(shell),
                curl_capability: curl_capability.as_ref().map(CapabilityId::to_string),
                providers: components.clone(),
                model: model.clone(),
                system: system.clone(),
                skills,
                suggestions: *suggestions,
                prompt: prompt.clone(),
                prompt_limits: PromptLimits {
                    max_steps: max_steps.get(),
                    max_capability_calls: shell.shell_max_capability_calls,
                },
                runtime: Handle::current(),
            };
            evaluate_prompt(settings, *broker, connection)
                .instrument(tracing::info_span!(
                    "runner.prompt",
                    provider.count = components.len(),
                    model = %model.model,
                    model.backend = model_backend(model),
                    prompt.max_steps = max_steps.get(),
                    prompt.broker = *broker,
                    prompt.skills = skill.len(),
                    prompt.suggestions = *suggestions
                ))
                .await
        }
        Command::Session { command } => evaluate_session(command).await,
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

/// Runs the synchronous immediate shell as one opaque process-lifecycle node.
///
/// Provider loading and interpretation are both blocking work. The node is therefore explicitly
/// non-interruptible after start and is awaited honestly on Tokio's blocking pool. The shell keeps
/// ownership of its existing value, output, status, tracing, and deadline semantics.
async fn evaluate_shell(
    components: Vec<PathBuf>,
    host_limits: HostLimits,
    host_options: HostOptions,
    interpreter_limits: ShellLimits,
    curl_capability: Option<String>,
    script: String,
    verbosity: u8,
) -> Result<CommandOutput, AppError> {
    // Captured on the async side, as the broker leg captures its own: the command-word nodes the
    // invoker runs need a handle to the runtime the blocking thread is parked against.
    let runtime = Handle::current();
    let operation = process_fn(
        ProcessMetadata::non_interruptible("legacy-shell"),
        move || async move {
            let node_span = tracing::Span::current();
            tokio::task::spawn_blocking(move || {
                node_span.in_scope(|| {
                    // Loaded here, on the blocking thread, so Cranelift never runs on a worker.
                    let registry = Arc::new(ProviderRegistry::load_with_options(
                        components,
                        host_limits,
                        &host_options,
                    )?);
                    let invoker = RegistryInvoker::new(registry, runtime);
                    let outcome = Interpreter::new(interpreter_limits)
                        .with_curl_capability(curl_capability)
                        .run(&script, &invoker);
                    Ok(CommandOutput {
                        text: format_script_outcome(&outcome),
                        exit_code: i32::from(outcome.exit_code.get()),
                        accounting: None,
                    })
                })
            })
            .await
            .map_err(AppError::ShellTask)?
        },
    );
    match ProcessRun::execute(operation, move |outcome| {
        observe_unobserved_shell_outcome(outcome, verbosity);
    })
    .await
    {
        ProcessOutcome::Completed(result) => result,
        ProcessOutcome::TaskFailed(error) => Err(AppError::ShellProcessTask(error)),
    }
}

/// Handles a shell result whose original `execute` caller was dropped.
///
/// Lifecycle telemetry carries only fixed categories. The ordinary operator error reporter remains
/// the sole destination for the complete cause; a successful `CommandOutput` is deliberately not
/// rendered because its script/provider payload no longer has a caller.
fn observe_unobserved_shell_outcome(
    outcome: ProcessOutcome<CommandOutput, AppError>,
    verbosity: u8,
) {
    match outcome {
        ProcessOutcome::Completed(Ok(_output)) => {
            tracing::warn!(
                target: "dekopon_run::audit",
                {
                    audit.event = "runner.shell.unobserved",
                    command.name = "shell",
                    outcome = "succeeded",
                    error.type = "none",
                },
                "unobserved shell process completed"
            );
        }
        ProcessOutcome::Completed(Err(error)) => {
            tracing::error!(
                target: "dekopon_run::audit",
                {
                    audit.event = "runner.shell.unobserved",
                    command.name = "shell",
                    outcome = "operation-error",
                    error.type = error.telemetry_kind(),
                },
                "unobserved shell process failed"
            );
            report_error(&error, verbosity);
        }
        ProcessOutcome::TaskFailed(error) => {
            let error = AppError::ShellProcessTask(error);
            tracing::error!(
                target: "dekopon_run::audit",
                {
                    audit.event = "runner.shell.unobserved",
                    command.name = "shell",
                    outcome = "task-failed",
                    error.type = error.telemetry_kind(),
                },
                "unobserved shell process task failed"
            );
            report_error(&error, verbosity);
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
    options: HostOptions,
    shell: ShellLimits,
    curl_capability: Option<String>,
    providers: Vec<PathBuf>,
    model: ModelArgs,
    system: Option<String>,
    skills: Vec<Skill>,
    suggestions: bool,
    prompt: String,
    prompt_limits: PromptLimits,
    /// The runtime the direct leg's command-word nodes run on, captured on the async side.
    runtime: Handle,
}

/// Stable label for which model backend a set of model arguments selects.
fn model_backend(model: &ModelArgs) -> &'static str {
    if model.chatgpt_subscription {
        "chatgpt-subscription"
    } else {
        "openai-compatible"
    }
}

/// Builds the model client the arguments select. Reads the bearer token, so it runs once per
/// command and never on a runtime worker.
fn build_model(model: &ModelArgs) -> Result<Box<dyn ChatModel>, AppError> {
    let timeout = Duration::from_millis(model.model_timeout_ms);
    if model.chatgpt_subscription {
        return Ok(Box::new(ChatGptCodexModel::new(
            &model.model,
            model.chatgpt_auth_file.as_deref(),
            timeout,
        )?));
    }
    let bearer_token = read_optional_secret(&model.api_key_env)?;
    Ok(Box::new(OpenAiChatModel::new(
        &model.endpoint,
        &model.model,
        bearer_token,
        timeout,
    )?))
}

/// Reads every `--skill` directory, in order, before a session starts.
fn load_skills(directories: &[PathBuf]) -> Result<Vec<Skill>, AppError> {
    let mut skills = Vec::with_capacity(directories.len());
    for directory in directories {
        let skill = dekopon_config::load_skill(directory).map_err(AppError::Skill)?;
        if skills
            .iter()
            .any(|loaded: &Skill| loaded.name() == skill.name())
        {
            return Err(AppError::DuplicateSkill {
                name: skill.name().to_string(),
            });
        }
        skills.push(skill);
    }
    Ok(skills)
}

/// Prints the suggestions a session recorded, for the operator who asked for them.
///
/// Standard error rather than standard output: the answer is the command's output, and a script
/// capturing it must not find a suggestion appended to the model's text.
fn report_suggestions(suggestions: &[ImprovementSuggestion]) {
    for (index, suggestion) in suggestions.iter().enumerate() {
        eprintln!(
            "suggestion {}/{} [{}, {} confidence] {}: {}",
            index + 1,
            suggestions.len(),
            suggestion.category,
            suggestion.confidence,
            suggestion.target,
            suggestion.summary
        );
        eprintln!(
            "  evidence: {}",
            suggestion.evidence.replace('\n', "\n            ")
        );
        eprintln!(
            "  proposal: {}",
            suggestion.proposal.replace('\n', "\n            ")
        );
    }
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
    if !broker && connection.any_flag_supplied() {
        return Err(AppError::BrokerFlagsWithoutOptIn);
    }

    let leg = connect_prompt_broker(broker, connection).await?;
    let span = tracing::Span::current();
    let accounting = dekopon_harness::accounting::JobAccounting::default();
    let worker_accounting = accounting.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        run_prompt_session(settings, leg, &worker_accounting)
    })
    .await
    .map_err(AppError::PromptTask)??;

    report_suggestions(&outcome.suggestions);
    Ok(CommandOutput {
        text: outcome.answer,
        exit_code: 0,
        accounting: Some(accounting),
    })
}

/// Loads providers, builds the model client, and runs the loop. Blocking throughout.
fn run_prompt_session(
    settings: PromptSettings,
    broker: Option<Box<dyn CapabilityInvoker + Send>>,
    accounting: &dekopon_harness::accounting::JobAccounting,
) -> Result<session::SessionExit, AppError> {
    let registry = Arc::new(ProviderRegistry::load_with_options(
        settings.providers,
        settings.limits,
        &settings.options,
    )?);
    let model = build_model(&settings.model)?;
    let runtime = ShellRuntime {
        invoker: SessionInvoker {
            direct: RegistryInvoker::new(registry, settings.runtime),
            broker,
        },
        limits: settings.shell,
        curl_capability: settings.curl_capability,
    };

    let mut inputs = SessionBootstrap::new(
        &settings.prompt,
        settings.prompt_limits,
        &settings.model.model,
    )
    .with_system(settings.system.as_deref())
    .with_skills(&settings.skills)
    .with_accounting(accounting);
    if settings.suggestions {
        inputs = inputs.with_improvement_suggestions();
    }
    // A one-shot session starts from an empty conversation and forgets it on the way out; the
    // accumulator exists only because the loop records into one.
    let mut history = history::History::default();
    SessionEngine::new(model.as_ref(), &runtime)
        .run(inputs, &mut history)
        .map_err(AppError::from)
}

/// Reads sessions back from the receiver, or from a transcript file, and replays one.
///
/// Every search is a blocking HTTP round trip and a replay is a whole model session, so both run
/// on the blocking pool for the reason a prompt session does.
async fn evaluate_session(command: &SessionCommand) -> Result<CommandOutput, AppError> {
    match command {
        SessionCommand::List {
            observe,
            since,
            limit,
            json,
        } => {
            let client = observe_client(observe)?;
            let (start_us, end_us) = search_window(parse_since(since)?)?;
            let span = tracing::info_span!("runner.session.list", session.limit = *limit);
            let result = tokio::task::spawn_blocking(move || {
                let _entered = span.enter();
                client.search(&client.accounting_sql(), start_us, end_us)
            })
            .await
            .map_err(AppError::ObserveTask)??;
            warn_if_truncated(result.truncated);
            let mut sessions = list_sessions(&result.hits);
            sessions.truncate(*limit);
            if *json {
                serde_json::to_string_pretty(&sessions)
                    .map(CommandOutput::success)
                    .map_err(AppError::Serialize)
            } else {
                Ok(CommandOutput::success(render_session_table(&sessions)))
            }
        }
        SessionCommand::Show { source, json } => {
            let recorded = load_recorded(source).await?;
            if *json {
                serde_json::to_string_pretty(&recorded)
                    .map(CommandOutput::success)
                    .map_err(AppError::Serialize)
            } else {
                Ok(CommandOutput::success(render_transcript(&recorded)))
            }
        }
        SessionCommand::Replay {
            source,
            model,
            system,
            system_file,
            skill,
            suggestions,
            provider,
            compile_cache,
            limits,
            shell,
            max_steps,
            json,
        } => {
            let recorded = load_recorded(source).await?;
            let system = match (system, system_file) {
                (Some(text), _) => Some(text.clone()),
                (None, Some(path)) => Some(read_text_file(path)?),
                (None, None) => None,
            };
            let skills = load_skills(skill)?;
            let providers = ProviderArgs {
                provider: provider.clone(),
                compile_cache: compile_cache.clone(),
            };
            let components = providers.components()?;
            let settings = ReplaySettings {
                recorded,
                model: model.clone(),
                system,
                skills,
                suggestions: *suggestions,
                components,
                host_limits: host_limits(limits),
                host_options: providers.host_options(),
                shell: shell_limits(shell),
                prompt_limits: PromptLimits {
                    max_steps: max_steps.get(),
                    max_capability_calls: shell.shell_max_capability_calls,
                },
                runtime: Handle::current(),
            };
            let span = tracing::info_span!(
                "runner.session.replay",
                model = %model.model,
                model.backend = model_backend(model),
                provider.count = settings.components.len(),
                prompt.max_steps = max_steps.get(),
                prompt.skills = skill.len(),
                prompt.suggestions = *suggestions,
                replay.system_replaced = settings.system.is_some()
            );
            let accounting = dekopon_harness::accounting::JobAccounting::default();
            let worker_accounting = accounting.clone();
            let report = tokio::task::spawn_blocking(move || {
                let _entered = span.enter();
                run_replay(settings, &worker_accounting)
            })
            .await
            .map_err(AppError::PromptTask)??;
            report_suggestions(&report.suggestions);
            // A replay that stopped at a divergence did its job; one whose session failed for
            // any other reason still prints the comparison, and the exit code says it failed.
            let exit_code = i32::from(report.error.is_some());
            let text = if *json {
                serde_json::to_string_pretty(&report).map_err(AppError::Serialize)?
            } else {
                render_replay(&report)
            };
            Ok(CommandOutput {
                text,
                exit_code,
                accounting: Some(accounting),
            })
        }
    }
}

/// Everything one replay needs, gathered before the blocking handoff.
struct ReplaySettings {
    recorded: RecordedSession,
    model: ModelArgs,
    system: Option<String>,
    skills: Vec<Skill>,
    suggestions: bool,
    components: Vec<PathBuf>,
    host_limits: HostLimits,
    host_options: HostOptions,
    shell: ShellLimits,
    prompt_limits: PromptLimits,
    /// The runtime a live leg's command-word nodes run on, captured on the async side.
    runtime: Handle,
}

/// Builds the model, loads any live providers, and replays. Blocking throughout.
fn run_replay(
    settings: ReplaySettings,
    accounting: &dekopon_harness::accounting::JobAccounting,
) -> Result<ReplayReport, AppError> {
    let model = build_model(&settings.model)?;
    // Providers are loaded only when named: the default replay must be provably effect-free, and
    // a loaded component is a thing that can run.
    let registry = if settings.components.is_empty() {
        None
    } else {
        Some(Arc::new(ProviderRegistry::load_with_options(
            settings.components,
            settings.host_limits,
            &settings.host_options,
        )?))
    };
    // The live leg speaks the recording's vocabulary: a recorded `probe --help` replays the same
    // way it ran, so command words are served here exactly as `prompt` serves them.
    let live = registry.map(|registry| ShellRuntime {
        invoker: RegistryInvoker::new(registry, settings.runtime),
        limits: settings.shell,
        // Direct mode cannot speak HTTP, so there is no capability for `curl` to assemble for.
        curl_capability: None,
    });
    let inputs = ReplayInputs {
        accounting: Some(accounting),
        selected_model: &settings.model.model,
        system: settings.system.as_deref(),
        skills: &settings.skills,
        improvement_suggestions: settings.suggestions,
        live: live
            .as_ref()
            .map(|runtime| runtime as &(dyn ScriptRuntime + Sync)),
        limits: settings.prompt_limits,
    };
    Ok(replay(model.as_ref(), &settings.recorded, inputs))
}

/// Loads one recorded session from a transcript file or from the receiver.
async fn load_recorded(source: &SessionSourceArgs) -> Result<RecordedSession, AppError> {
    if let Some(path) = &source.from_file {
        let text = read_text_file(path)?;
        let recorded: RecordedSession =
            serde_json::from_str(&text).map_err(|error| AppError::ParseRecording {
                path: path.clone(),
                source: error,
            })?;
        recorded.validate().map_err(AppError::Recording)?;
        return Ok(recorded);
    }
    let trace_id = source
        .trace_id
        .clone()
        .expect("Clap requires --trace-id unless --from-file is present");
    let client = observe_client(&source.observe)?;
    let sql = client.trace_sql(&trace_id)?;
    let (start_us, end_us) = search_window(parse_since(&source.since)?)?;
    let span = tracing::info_span!("runner.session.fetch");
    let result = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        client.search(&sql, start_us, end_us)
    })
    .await
    .map_err(AppError::ObserveTask)??;
    warn_if_truncated(result.truncated);
    RecordedSession::from_records(&trace_id, &result.hits).map_err(AppError::Recording)
}

/// Builds the search client from the shared receiver flags, reading the credential by name.
fn observe_client(args: &ObserveArgs) -> Result<OpenObserveClient, AppError> {
    let url = args
        .openobserve_url
        .clone()
        .ok_or(AppError::ObserveUrlMissing)?;
    let authorization = read_optional_secret(&args.openobserve_auth_env)?.ok_or_else(|| {
        AppError::ObserveCredentialMissing {
            variable: args.openobserve_auth_env.clone(),
        }
    })?;
    OpenObserveClient::new(OpenObserveSettings {
        url,
        stream: args.openobserve_stream.clone(),
        authorization,
        timeout: Duration::from_millis(args.openobserve_timeout_ms),
    })
    .map_err(AppError::Observe)
}

fn warn_if_truncated(truncated: bool) {
    if truncated {
        eprintln!(
            "warning: the search stopped after {} pages of {} records; narrow --since to see the rest",
            observe::MAX_PAGES,
            observe::PAGE_SIZE
        );
    }
}

/// Parses a `--since` window: a count followed by `s`, `m`, `h`, or `d`.
fn parse_since(text: &str) -> Result<Duration, AppError> {
    let trimmed = text.trim();
    let invalid = || AppError::Since {
        text: text.to_owned(),
    };
    let unit = trimmed.chars().last().ok_or_else(invalid)?;
    let count = trimmed[..trimmed.len() - unit.len_utf8()]
        .parse::<u64>()
        .map_err(|_error| invalid())?;
    let seconds_per_unit = match unit {
        's' => 1,
        'm' => 60,
        'h' => 60 * 60,
        'd' => 24 * 60 * 60,
        _ => return Err(invalid()),
    };
    let seconds = count.checked_mul(seconds_per_unit).ok_or_else(invalid)?;
    if seconds == 0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs(seconds))
}

/// The `[start, end)` microsecond window ending now that a `--since` duration selects.
fn search_window(window: Duration) -> Result<(i64, i64), AppError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(AppError::Clock)?;
    let end_us = i64::try_from(now.as_micros()).unwrap_or(i64::MAX);
    let window_us = i64::try_from(window.as_micros()).unwrap_or(i64::MAX);
    Ok((end_us.saturating_sub(window_us).max(0), end_us))
}

/// Reads one UTF-8 text file the operator named, bounded.
fn read_text_file(path: &Path) -> Result<String, AppError> {
    let file = File::open(path).map_err(|source| AppError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    let read_limit = u64::try_from(MAX_TEXT_FILE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|source| AppError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_TEXT_FILE_BYTES {
        return Err(AppError::FileTooLarge {
            path: path.to_path_buf(),
            maximum: MAX_TEXT_FILE_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|source| AppError::FileUtf8 {
        path: path.to_path_buf(),
        source,
    })
}

/// Renders microseconds since the epoch as an RFC 3339 UTC timestamp, to the second.
fn format_timestamp(micros: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1_000)
        .ok()
        .and_then(|time| time.replace_nanosecond(0).ok())
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| micros.to_string())
}

fn render_session_table(sessions: &[SessionListing]) -> String {
    let mut text = format!(
        "{:<32}  {:<20}  {:>5}  {:>8}  {:<9}  SERVICE\n",
        "TRACE", "STARTED", "TURNS", "TOKENS", "OUTCOME"
    );
    if sessions.is_empty() {
        text.push_str("(no sessions in the window)\n");
        return text;
    }
    for session in sessions {
        let outcome = match (session.failed, session.answered) {
            (true, _) => "failed",
            (false, true) => "answered",
            (false, false) => "no-answer",
        };
        text.push_str(&format!(
            "{:<32}  {:<20}  {:>5}  {:>8}  {:<9}  {}\n",
            session.trace_id,
            format_timestamp(session.started_us),
            session.model_turns,
            session
                .total_tokens
                .map_or_else(|| "-".to_owned(), |tokens| tokens.to_string()),
            outcome,
            session.service.as_deref().unwrap_or("-")
        ));
    }
    text
}

/// Indents every line of a block so it reads as one field's value.
fn indented(block: &str) -> String {
    let trimmed = block.trim_end();
    if trimmed.is_empty() {
        return "    (empty)".to_owned();
    }
    trimmed
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_transcript(recorded: &RecordedSession) -> String {
    let mut text = format!("trace: {}\n", recorded.trace_id);
    for system in &recorded.system {
        text.push_str(&format!("system:\n{}\n", indented(system)));
    }
    if let Some(initial) = recorded.contexts.first() {
        for message in initial
            .messages
            .iter()
            .take(initial.messages.len().saturating_sub(1))
            .filter(|m| m.role != "system")
        {
            render_context_message(&mut text, message, " (earlier)");
        }
    } else {
        for exchange in &recorded.history {
            text.push_str(&format!("user (earlier):\n{}\n", indented(&exchange.user)));
            if let Some(answer) = &exchange.answer {
                text.push_str(&format!("assistant (earlier):\n{}\n", indented(answer)));
            }
        }
    }
    text.push_str(&format!("user:\n{}\n", indented(&recorded.prompt)));
    for turn in &recorded.turns {
        if let Some(context) = recorded
            .contexts
            .iter()
            .skip(1)
            .find(|c| c.turn == turn.turn && c.scope == "full")
        {
            text.push_str(&format!(
                "context revision {} (turn {}):\n",
                context.revision.unwrap_or_default(),
                context.turn
            ));
            for message in &context.messages {
                render_context_message(&mut text, message, " (context)");
            }
        }
        let mut header = format!("turn {}", turn.turn);
        if let Some(duration) = turn.duration_ms {
            header.push_str(&format!(" [{duration:.0} ms"));
            if let Some(total) = turn.usage.and_then(|usage| usage.total_tokens) {
                header.push_str(&format!(", {total} tokens"));
            }
            header.push(']');
        }
        text.push_str(&format!("{header}:\n"));
        if let Some(content) = turn
            .content
            .as_deref()
            .filter(|content| !content.trim().is_empty())
        {
            text.push_str(&format!("  assistant:\n{}\n", indented(content)));
        }
        for call in &turn.tool_calls {
            render_tool_call(&mut text, call);
        }
    }
    match &recorded.answer {
        Some(answer) => {
            text.push_str(&format!("answer:\n{}\n", indented(answer)));
        }
        None => text.push_str("answer: (none recorded)\n"),
    }
    text
}

fn render_context_message(
    text: &mut String,
    message: &dekopon_harness::replay::RecordedMessage,
    label: &str,
) {
    text.push_str(&format!(
        "{}{label}:\n{}\n",
        message.role,
        indented(message.content.as_deref().unwrap_or_default())
    ));
    for call in &message.tool_calls {
        text.push_str(&format!(
            "  tool {} [{}]:\n{}\n",
            call.function.name,
            call.id,
            indented(&call.function.arguments)
        ));
    }
    if let Some(id) = &message.tool_call_id {
        text.push_str(&format!("  answers tool {id}\n"));
    }
}

fn render_tool_call(text: &mut String, call: &RecordedToolCall) {
    match call.script() {
        Some(script) => {
            text.push_str(&format!("  script:\n{}\n", indented(&script)));
        }
        None => {
            text.push_str(&format!(
                "  tool {}:\n{}\n",
                call.name,
                indented(&call.arguments)
            ));
        }
    }
    match &call.result {
        Some(result) => {
            text.push_str(&format!("  output:\n{}\n", indented(result)));
        }
        None => text.push_str("  output: (not recorded)\n"),
    }
}

fn render_replay(report: &ReplayReport) -> String {
    let mut text = format!("trace: {}\n", report.trace_id);
    for (label, summary) in [
        ("recorded", &report.recorded),
        ("replayed", &report.replayed),
    ] {
        text.push_str(&format!(
            "{label}: {} turn(s), {} script(s), {} token(s), answer: {}\n",
            summary
                .model_turns
                .map_or_else(|| "-".to_owned(), |turns| turns.to_string()),
            summary.scripts.len(),
            summary
                .usage
                .total_tokens
                .map_or_else(|| "-".to_owned(), |tokens| tokens.to_string()),
            if summary.answer.is_some() {
                "yes"
            } else {
                "no"
            }
        ));
    }
    match &report.divergence {
        Some(divergence) => {
            text.push_str(&format!(
                "divergence: turn {} ({}), {} recorded script(s) unused\n  script:\n{}\n",
                divergence.turn,
                match divergence.handling {
                    dekopon_harness::replay::DivergenceHandling::Stopped => "stopped there",
                    dekopon_harness::replay::DivergenceHandling::Live => "ran live",
                },
                divergence.unused_recorded_scripts.len(),
                indented(&divergence.script)
            ));
        }
        None => text.push_str("divergence: none\n"),
    }
    if report.dropped_history_turns > 0 {
        text.push_str(&format!(
            "history: {} recorded exchange(s) did not fit the replay's retention window\n",
            report.dropped_history_turns
        ));
    }
    let width = report
        .recorded
        .scripts
        .len()
        .max(report.replayed.scripts.len());
    for index in 0..width {
        let recorded = report.recorded.scripts.get(index);
        let replayed = report.replayed.scripts.get(index);
        let status = match (recorded, replayed) {
            (Some(left), Some(right)) if left == right => "same",
            (Some(_), Some(_)) => "differs",
            (Some(_), None) => "recorded only",
            (None, Some(_)) => "replayed only",
            (None, None) => unreachable!("index is below the longer list"),
        };
        text.push_str(&format!("script {} ({status}):\n", index + 1));
        if let Some(script) = recorded {
            text.push_str(&format!("  recorded:\n{}\n", indented(script)));
        }
        if let Some(script) = replayed
            && status != "same"
        {
            text.push_str(&format!("  replayed:\n{}\n", indented(script)));
        }
    }
    for (label, summary) in [
        ("recorded", &report.recorded),
        ("replayed", &report.replayed),
    ] {
        match &summary.answer {
            Some(answer) => {
                text.push_str(&format!("answer ({label}):\n{}\n", indented(answer)));
            }
            None => {
                text.push_str(&format!("answer ({label}): (none)\n"));
            }
        }
    }
    if let Some(error) = &report.error {
        text.push_str(&format!("error: {error}\n"));
    }
    text
}

struct CommandOutput {
    text: String,
    exit_code: i32,
    accounting: Option<dekopon_harness::accounting::JobAccounting>,
}

impl CommandOutput {
    fn success(text: String) -> Self {
        Self {
            text,
            exit_code: 0,
            accounting: None,
        }
    }

    /// A command that already streamed its own output and has nothing left to print.
    #[cfg(unix)]
    fn silent() -> Self {
        Self {
            text: String::new(),
            exit_code: 0,
            accounting: None,
        }
    }
}

/// Adapts the direct provider registry to the interpreter's capability seam.
///
/// Direct mode performs no broker transition, so no invocation here can be *denied*: there is no
/// authorization to refuse one. `Denied` stays reachable in the shared vocabulary because a
/// broker-backed invoker will produce it; this adapter produces it only for a command-word run
/// the session cancelled underneath, which direct mode has no way to do today.
///
/// The registry is shared rather than borrowed because each command-word run is one process node
/// whose blocking body outlives this invoker's stack frame on the Tokio blocking pool.
struct RegistryInvoker {
    registry: Arc<ProviderRegistry>,
    runtime: Handle,
    /// The words every loaded provider declared, snapshotted once: dispatch asks per word, and a
    /// script running a thousand commands must not rebuild the list a thousand times.
    command_words: BTreeSet<String>,
}

impl RegistryInvoker {
    fn new(registry: Arc<ProviderRegistry>, runtime: Handle) -> Self {
        let command_words = registry
            .command_words_by_provider()
            .into_iter()
            .flat_map(|(_provider, words)| words.iter().cloned())
            .collect();
        Self {
            registry,
            runtime,
            command_words,
        }
    }
}

impl CapabilityInvoker for RegistryInvoker {
    fn command_words(&self) -> Vec<String> {
        self.command_words.iter().cloned().collect()
    }

    fn has_command_word(&self, word: &str) -> bool {
        self.command_words.contains(word)
    }

    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        if !self.command_words.contains(word) {
            return None;
        }
        // One non-interruptible node per run. The guest call blocks a thread the way a capability
        // call does, and the supervisor never joins `spawn_blocking` work, so a cancellable node
        // here would report `cancelled` while the Wasm call still ran; the run therefore stays
        // joined to its end, exactly as the `legacy-shell` node around the whole script does.
        let registry = Arc::clone(&self.registry);
        let (owned_word, argv, stdin) = (word.to_owned(), argv.to_vec(), stdin.map(str::to_owned));
        let operation = process_fn(
            ProcessMetadata::non_interruptible("direct-command"),
            move || async move {
                let node_span = tracing::Span::current();
                tokio::task::spawn_blocking(move || {
                    node_span.in_scope(|| {
                        registry
                            .run_command(&owned_word, &argv, stdin.as_deref())
                            .map(command_run_from_outcome)
                            .map_err(AppError::from)
                    })
                })
                .await
                .map_err(AppError::CommandTask)?
            },
        );
        // The interpreter runs on the blocking pool, never on a worker, so blocking here is legal
        // for the same reason the broker leg's `invoke` documents.
        let outcome = self
            .runtime
            .block_on(ProcessRun::execute(operation, |outcome| {
                report_unobserved_command_run("direct", outcome, AppError::telemetry_kind);
            }));
        Some(match outcome {
            ProcessOutcome::Completed(Ok(run)) => run,
            // A host refusal — an input past `--max-input-bytes`, a trap, a deadline — is not the
            // provider declining the argv; its message already names the provider and the bound,
            // and the interpreter prefixes the word.
            ProcessOutcome::Completed(Err(error)) => CommandRun::Errored {
                message: error.to_string(),
            },
            ProcessOutcome::TaskFailed(error) => CommandRun::Errored {
                message: AppError::CommandTask(error).to_string(),
            },
        })
    }

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

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        // Deny-by-default on the direct leg: immediate mode has no authorizer and no credential
        // store, so a proposal naming a DRN is refused here rather than run without it.
        if secret_use.is_some() {
            return dekopon_shell::secret_use_unsupported();
        }
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
    let (client, socket_tier) = broker_client(connection)?;
    let leg = BrokerLeg::connect(client, "dekopon-run-prompt", None)
        .await
        .map_err(|error| match error {
            BrokerLegError::Bootstrap(source) => AppError::from(PromptError::from(source)),
            BrokerLegError::Client(source) => AppError::BrokerClient(source),
            BrokerLegError::SessionIdentifier(source) => AppError::SessionIdentifier(source),
            BrokerLegError::DuplicateCapabilities { capabilities } => {
                AppError::BrokerDuplicateCapabilities { capabilities }
            }
        })?;
    // The socket tier and the session trace are what a "this session saw zero capabilities"
    // investigation asks for first: which broker was reached, and which audit records are this
    // session's. The socket path itself stays out, as it does everywhere else in telemetry.
    tracing::info!(
        target: "dekopon_run::audit",
        {
            audit.event = "broker.leg.connected",
            broker.socket.tier = socket_tier,
            session.trace = %leg.session_trace(),
            capability.count = leg.granted().len(),
        },
        "broker leg connected for prompt session"
    );

    Ok(Some(Box::new(leg)))
}

/// Builds one authenticated broker client from the shared connection flags.
///
/// Every broker-reaching command resolves the socket, the trusted server UID, and the frame limits
/// the same way; one copy means a new discovery tier or limit floor lands everywhere at once.
/// Returns the resolved socket tier alongside the client so callers can report which one answered
/// without repeating the precedence rules.
#[cfg(unix)]
fn broker_client(
    connection: &BrokerConnectionArgs,
) -> Result<(BrokerClient, &'static str), AppError> {
    let socket = BrokerSocketDiscovery::from_process(connection.socket.clone())
        .resolve()
        .ok_or(AppError::BrokerSocketUnresolved)?;
    let server_uid = resolve_broker_server_uid(connection.server_uid);
    let client = BrokerClient::new(socket.path(), server_uid, connection.frame_limits())?;
    Ok((client, socket.tier().label()))
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
                let (client, _tier) = broker_client(connection)?;
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
                let (client, _tier) = broker_client(connection)?;
                let input = read_input(
                    input.as_deref(),
                    input_file.as_deref(),
                    connection
                        .frame_limits()
                        .max_frame_bytes
                        .saturating_sub(ENVELOPE_RESERVE_BYTES),
                )?;
                if !input.is_object() {
                    return Err(AppError::BrokerInputObject);
                }
                let result = client
                    .invoke(
                        None,
                        InvocationRequest {
                            id: invocation_id.clone(),
                            capability: capability.clone(),
                            trace: trace_id.clone(),
                            trace_parent: dekopon_harness::runtime::current_trace_parent(),
                            secret_use: None,
                            input,
                        },
                    )
                    .await?;
                let exit_code = if result.outcome == InvocationOutcome::Succeeded {
                    0
                } else {
                    1
                };
                serde_json::to_string_pretty(&result)
                    .map(|text| CommandOutput {
                        text,
                        exit_code,
                        accounting: None,
                    })
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

#[allow(
    clippy::map_err_ignore,
    reason = "the error `OsString::into_string` returns is the rejected OsString itself — the \
              secret this function reads — so naming the cause would print key bytes to stderr"
)]
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

impl CommandOutput {
    fn write_to(&self, writer: &mut impl io::Write) -> io::Result<()> {
        use history::DeliveryDisposition;
        let mut bytes = self.text.as_bytes().to_vec();
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        let mut written = 0;
        let mut flushing = false;
        let result = (|| {
            while written < bytes.len() {
                match writer.write(&bytes[written..]) {
                    Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            flushing = true;
            writer.flush()
        })();
        let disposition = if result.is_ok() {
            DeliveryDisposition::Accepted {
                text: self.text.clone(),
            }
        } else if flushing {
            DeliveryDisposition::Unknown
        } else if written == 0 {
            DeliveryDisposition::Failed
        } else {
            DeliveryDisposition::Partial
        };
        if let Some(accounting) = &self.accounting {
            accounting.finalize(&disposition);
        }
        result
    }
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
    #[cfg(unix)]
    #[error("the broker answered with duplicate capability identifiers: {capabilities}")]
    BrokerDuplicateCapabilities {
        /// Every repeated identifier, in identifier order.
        capabilities: String,
    },
    #[error("the prompt session did not run to completion")]
    PromptTask(#[source] tokio::task::JoinError),
    #[error("a --skill directory could not be mounted")]
    Skill(#[source] SkillError),
    #[error("skill {name:?} was mounted twice; a model could not tell the two apart")]
    DuplicateSkill { name: String },
    #[error(transparent)]
    Observe(#[from] ObserveError),
    #[error("no OpenObserve URL; pass --openobserve-url or set DEKOPON_OPENOBSERVE_URL")]
    ObserveUrlMissing,
    #[error(
        "environment variable {variable} is not set; it must hold the OpenObserve Authorization header value"
    )]
    ObserveCredentialMissing { variable: String },
    #[error("the OpenObserve search did not run to completion")]
    ObserveTask(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Recording(RecordingError),
    #[error("could not read {}", path.display())]
    ReadFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is larger than the {maximum}-byte maximum", path.display())]
    FileTooLarge { path: PathBuf, maximum: usize },
    #[error("{} is not UTF-8", path.display())]
    FileUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("{} is not a transcript `session show --json` printed", path.display())]
    ParseRecording {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("--since {text:?} must be a count followed by s, m, h, or d, such as 24h")]
    Since { text: String },
    #[error("the system clock is before the Unix epoch")]
    Clock(#[source] SystemTimeError),
    #[error("the shell blocking task did not run to completion")]
    ShellTask(#[source] tokio::task::JoinError),
    #[error("the shell process task did not run to completion")]
    ShellProcessTask(#[source] tokio::task::JoinError),
    #[error("the command-word blocking task did not run to completion")]
    CommandTask(#[source] tokio::task::JoinError),
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
            #[cfg(unix)]
            Self::BrokerDuplicateCapabilities { .. } => "broker-duplicate-capabilities",
            Self::PromptTask(_) => "prompt-task",
            Self::Skill(_) => "skill",
            Self::DuplicateSkill { .. } => "duplicate-skill",
            Self::Observe(_) => "observe",
            Self::ObserveUrlMissing => "observe-url-missing",
            Self::ObserveCredentialMissing { .. } => "observe-credential-missing",
            Self::ObserveTask(_) => "observe-task",
            Self::Recording(_) => "recording",
            Self::ReadFile { .. } => "file-read",
            Self::FileTooLarge { .. } => "file-too-large",
            Self::FileUtf8 { .. } => "file-utf8",
            Self::ParseRecording { .. } => "recording-json",
            Self::Since { .. } => "since",
            Self::Clock(_) => "clock",
            Self::ShellTask(_) => "shell-task",
            Self::ShellProcessTask(_) => "shell-process-task",
            Self::CommandTask(_) => "command-task",
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
    use super::{AppError, BrokerSocketDiscovery, resolve_broker_server_uid};
    use super::{
        CapabilityCallResult, CapabilityInvoker, HostLimits, InvocationReport, ProviderRegistry,
        RegistryInvoker, TimingSamples, read_input,
    };
    use std::sync::Arc;
    use tokio::runtime::Handle;

    /// The direct leg has no authorizer and no credential store, so a DRN cannot be proven here.
    ///
    /// Dropping the field and running the call anyway is the one thing it must not do: the caller
    /// asked for a credential this leg cannot show it may use. The refusal is the shared wording
    /// from `dekopon_shell::secret_use_unsupported`, so an operator reading it in immediate mode
    /// and in a session sees one message rather than two spellings of the same limit.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_direct_leg_refuses_a_secret_use_proposal_it_cannot_authorize() {
        let registry = ProviderRegistry::load(
            [dekopon_test_support::provider_fixture("echo-provider.wasm")],
            HostLimits::default(),
        )
        .expect("echo provider loads");
        let invoker = RegistryInvoker::new(Arc::new(registry), Handle::current());
        let proposal = dekopon_core::SecretUseProposal::HttpBearer {
            secret: "drn:com.xrl:secret:prod:api/token"
                .parse::<dekopon_core::SecretDrn>()
                .expect("canonical DRN"),
        };

        // The control: the same capability, the same input, no secret named.
        assert!(matches!(
            invoker.invoke("echo.echo", json!({"message": "hello"}), None),
            CapabilityCallResult::Succeeded(_)
        ));

        assert_eq!(
            invoker.invoke("echo.echo", json!({"message": "hello"}), Some(proposal)),
            dekopon_shell::secret_use_unsupported()
        );
        assert_eq!(
            dekopon_shell::secret_use_unsupported(),
            CapabilityCallResult::Denied {
                reason: "secret references require a broker-backed capability".to_owned(),
            }
        );
    }

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

    // The precedence itself is pinned in `dekopon-broker-protocol`, which owns the one definition
    // every client shares. What belongs here is the mapping this crate applies on top of it: an
    // unresolvable socket is a runner usage failure with actionable guidance, not a silent default.
    #[cfg(unix)]
    #[test]
    fn unresolvable_broker_socket_reports_actionable_guidance() {
        let error = BrokerSocketDiscovery::new(None, None, None, None)
            .resolve()
            .ok_or(AppError::BrokerSocketUnresolved)
            .expect_err("no socket candidate");

        assert!(matches!(error, AppError::BrokerSocketUnresolved));
        assert!(
            error
                .to_string()
                .contains("pass --socket or set DEKOPON_BROKER_SOCKET"),
            "the refusal must name both ways out: {error}"
        );
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

    // Composite dispatch and broker-leg behavior are covered in `dekopon-harness`, where those
    // types now live.
}

#[cfg(test)]
mod output_accounting_tests {
    use super::*;
    use dekopon_model::model::{AssistantTurn, ModelError, ModelMessage, ModelTool};
    struct Model;
    impl ChatModel for Model {
        fn complete(
            &self,
            _: &[ModelMessage],
            _: &[ModelTool],
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            recorder.observe(
                attempt,
                dekopon_model::usage::UsageObservation::from_json(
                    &serde_json::json!({"input_tokens":7}),
                    false,
                ),
            )?;
            Ok(AssistantTurn {
                content: Some("answer".into()),
                tool_calls: vec![],
                usage: None,
                replay_items: vec![],
            })
        }
    }
    struct NoCapabilities;
    impl CapabilityInvoker for NoCapabilities {
        fn granted(&self) -> Vec<String> {
            vec![]
        }
        fn describe(&self, _: &str) -> Option<dekopon_shell::CapabilityDescription> {
            None
        }
        fn invoke(
            &self,
            _: &str,
            _: Value,
            _: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }
    struct Fail(io::ErrorKind);
    impl io::Write for Fail {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn host_output_finalizes_after_success_broken_pipe_or_failed_write() {
        for failure in [
            None,
            Some(io::ErrorKind::BrokenPipe),
            Some(io::ErrorKind::PermissionDenied),
        ] {
            let ledger = dekopon_harness::accounting::JobAccounting::default();
            let runtime = ShellRuntime {
                invoker: NoCapabilities,
                limits: dekopon_shell::Limits::default(),
                curl_capability: None,
            };
            SessionEngine::new(&Model, &runtime)
                .run(
                    SessionBootstrap::new(
                        "request",
                        PromptLimits {
                            max_steps: 1,
                            max_capability_calls: 1,
                        },
                        "fixture",
                    )
                    .with_accounting(&ledger),
                    &mut history::History::default(),
                )
                .unwrap();
            assert!(!ledger.snapshot().finalized);
            let output = CommandOutput {
                text: "answer".into(),
                exit_code: 0,
                accounting: Some(ledger.clone()),
            };
            match failure {
                None => output.write_to(&mut Vec::new()).unwrap(),
                Some(kind) => {
                    assert_eq!(output.write_to(&mut Fail(kind)).unwrap_err().kind(), kind)
                }
            }
            let tracked = ledger.snapshot();
            assert!(tracked.finalized);
            assert_eq!(
                tracked.delivery,
                if failure.is_some() {
                    "failed"
                } else {
                    "accepted"
                }
            );
            assert_eq!(tracked.totals().cumulative.usage().input_tokens, Some(7));
            assert!(!ledger.finalize(&history::DeliveryDisposition::Unknown));
        }
    }
}
