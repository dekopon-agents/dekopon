//! Immediate-mode prompt and provider execution for Dekopon.
//!
//! `dekopon-run` deliberately keeps this path separate from the operator catalog CLI. It loads
//! only read-only, import-free provider components and does not claim broker authorization.

#![forbid(unsafe_code)]

use std::{
    env,
    error::Error as _,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use dekopon_core::{CapabilityId, ProviderId};
use dekopon_provider_host::{HostLimits, ProviderHostError, ProviderManifest, ProviderRegistry};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::{
    cli::{Cli, Command},
    model::{ModelError, OpenAiChatModel},
    prompt::{PromptError, run_prompt},
};

pub mod cli;
pub mod model;
pub mod prompt;
mod trace;

/// Runs a parsed CLI invocation and returns a process exit code.
///
/// Clap handles syntax failures before this function and exits with code `2`.
#[must_use]
pub fn run(cli: Cli) -> i32 {
    let _trace_guard = match trace::initialize(cli.verbose, cli.no_color, cli.trace.as_deref()) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    match evaluate(&cli) {
        Ok(output) => match write_output(&output) {
            Ok(()) => 0,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => 0,
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

fn evaluate(cli: &Cli) -> Result<String, AppError> {
    let limits = HostLimits {
        max_memory_bytes: cli.limits.max_memory_bytes,
        max_input_bytes: cli.limits.max_input_bytes,
        max_output_bytes: cli.limits.max_output_bytes,
        fuel: cli.limits.fuel,
        timeout: Duration::from_millis(cli.limits.timeout_ms),
    };

    match &cli.command {
        Command::Inspect { providers } => {
            let span =
                tracing::info_span!("runner.inspect", provider.count = providers.provider.len());
            let _entered = span.enter();
            let registry = ProviderRegistry::load(providers.provider.clone(), limits)?;
            let manifests = registry.manifests().collect::<Vec<&ProviderManifest>>();
            serde_json::to_string_pretty(&manifests).map_err(AppError::Serialize)
        }
        Command::Invoke {
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
                cli.limits.max_input_bytes,
            )?;
            let registry = ProviderRegistry::load(providers.provider.clone(), limits)?;
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
            serde_json::to_string_pretty(&report).map_err(AppError::Serialize)
        }
        Command::Prompt {
            providers,
            model,
            endpoint,
            api_key_env,
            system,
            max_steps,
            model_timeout_ms,
            prompt,
        } => {
            let span = tracing::info_span!(
                "runner.prompt",
                provider.count = providers.provider.len(),
                model = %model,
                prompt.max_steps = max_steps.get()
            );
            let _entered = span.enter();
            let registry = ProviderRegistry::load(providers.provider.clone(), limits)?;
            let bearer_token = read_optional_secret(api_key_env)?;
            let model = OpenAiChatModel::new(
                endpoint,
                model,
                bearer_token,
                Duration::from_millis(*model_timeout_ms),
            )?;
            let outcome = run_prompt(
                &model,
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
            Ok(outcome.answer)
        }
    }
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
    #[error("could not read provider input from {source_name}")]
    ReadInputStream {
        source_name: String,
        #[source]
        source: io::Error,
    },
    #[error("provider input from {source_name} is not UTF-8")]
    InputUtf8 {
        source_name: String,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("provider input is {length} bytes; the maximum is {maximum}")]
    InputTooLarge { length: usize, maximum: usize },
    #[error("provider input is not valid JSON")]
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
}
