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
use dekopon_broker_protocol::{
    BrokerClient, ClientError, FrameLimits, InvocationOutcome, InvocationRequest,
};
use dekopon_core::{CapabilityId, ProviderId};
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
#[cfg(unix)]
use tracing::Instrument as _;

use crate::{
    cli::{BrokerCommand, Cli, Command, LimitArgs, ShellLimitArgs},
    prompt::{PromptError, run_prompt},
};

pub mod cli;
pub mod prompt;
mod trace;

/// Runs a parsed CLI invocation and returns a process exit code.
///
/// Clap handles syntax failures before this function and exits with code `2`.
#[must_use]
pub async fn run(cli: Cli) -> i32 {
    let _trace_guard = match trace::initialize(cli.verbose, cli.no_color, cli.trace.as_deref()) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    match evaluate(&cli).await {
        Ok(output) => match write_output(&output.text) {
            Ok(()) => output.exit_code,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => output.exit_code,
            Err(error) => {
                eprintln!("error: could not write output: {error}");
                1
            }
        },
        Err(error) => {
            report_error(&error, cli.verbose);
            1
        }
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
            for _ in 0..repeat.get() {
                let start = Instant::now();
                let output = registry.invoke(capability, &input)?;
                samples.record(start.elapsed());
                last = Some(output);
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
                .with_curl_capability(curl_capability.clone())
                .run(script, &invoker);
            tracing::info!(
                shell.exit_code = outcome.exit_code.get(),
                shell.steps = outcome.steps,
                provider.invocations = outcome.capability_calls,
                shell.truncated = outcome.truncated,
                "shell script completed"
            );
            let mut text = outcome.output;
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[exit code: {}]", outcome.exit_code));
            Ok(CommandOutput {
                text,
                exit_code: i32::from(outcome.exit_code.get()),
            })
        }
        Command::Broker { command } => evaluate_broker(command).await,
        Command::Prompt {
            limits,
            providers,
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
            let span = tracing::info_span!(
                "runner.prompt",
                provider.count = providers.provider.len(),
                model = %model,
                model.backend = backend,
                prompt.max_steps = max_steps.get()
            );
            let _entered = span.enter();
            let registry = ProviderRegistry::load(providers.provider.clone(), host_limits(limits))?;
            let timeout = Duration::from_millis(*model_timeout_ms);
            let model: Box<dyn ChatModel> = if *chatgpt_subscription {
                Box::new(ChatGptCodexModel::new(
                    model,
                    chatgpt_auth_file.as_deref(),
                    timeout,
                )?)
            } else {
                let bearer_token = read_optional_secret(api_key_env)?;
                Box::new(OpenAiChatModel::new(
                    endpoint,
                    model,
                    bearer_token,
                    timeout,
                )?)
            };
            let outcome = run_prompt(
                model.as_ref(),
                &registry,
                prompt,
                system.as_deref(),
                max_steps.get(),
            )?;
            tracing::info!(
                model.turns = outcome.model_turns,
                provider.invocations = outcome.provider_invocations,
                "prompt session completed"
            );
            Ok(CommandOutput::success(outcome.answer))
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

fn shell_limits(limits: &ShellLimitArgs) -> ShellLimits {
    ShellLimits {
        max_steps: limits.shell_max_steps,
        max_recursion_depth: limits.shell_max_recursion_depth,
        max_output_bytes: limits.shell_max_output_bytes,
        max_output_lines: limits.shell_max_output_lines,
        timeout: Duration::from_millis(limits.shell_timeout_ms),
        max_capability_calls: limits.shell_max_capability_calls,
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

fn milliseconds(duration: Duration) -> f64 {
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
}
