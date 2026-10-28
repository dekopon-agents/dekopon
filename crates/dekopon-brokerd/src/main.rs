#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
#[cfg(unix)]
mod observed;
#[cfg(unix)]
use observed::observed;

#[cfg(unix)]
use std::{io, path::PathBuf, process::ExitCode};

#[cfg(unix)]
use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum, error::ErrorKind};
#[cfg(unix)]
use dekopon_core::error_chain;
#[cfg(unix)]
use dekopon_telemetry::{Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, Install};
#[cfg(unix)]
use serde::Serialize;
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(
    name = "dekopon-brokerd",
    version,
    about = "Run the authenticated broker or manage its provider set"
)]
struct Cli {
    /// Tokio worker threads (1 to 4096); otherwise TOKIO_WORKER_THREADS or the available CPU count.
    #[arg(long, env = "DEKOPON_WORKER_THREADS", value_name = "COUNT")]
    worker_threads: Option<std::num::NonZeroUsize>,
    /// Maximum additional Tokio blocking threads (1 to 4096; default: 512).
    #[arg(long, env = "DEKOPON_BLOCKING_THREADS", value_name = "COUNT")]
    blocking_threads: Option<std::num::NonZeroUsize>,
    /// Retire idle blocking threads after this duration (default: 10s).
    #[arg(long, env = "DEKOPON_BLOCKING_KEEP_ALIVE", value_name = "DURATION")]
    blocking_keep_alive: Option<humantime::Duration>,
    /// Rayon CPU threads (1 to 4096); otherwise RAYON_NUM_THREADS or the available CPU count.
    #[arg(long, env = "DEKOPON_CPU_THREADS", value_name = "COUNT")]
    cpu_threads: Option<std::num::NonZeroUsize>,
    /// Strict owner-controlled broker YAML/JSON configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Maintenance or health-check mode. Omit to serve the broker.
    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Check the broker as its mapped owner UID without loading configuration.
    Probe {
        /// Broker-owned Unix socket; the server must have this process's UID.
        #[arg(long, value_name = "PATH")]
        socket: PathBuf,
    },
    /// Resolve, materialize, and verify a startup-fixed provider set.
    Provider(ProviderArgs),
    /// Run startup validation on a configuration without serving: no socket is bound and no
    /// credentials file, secret map, asset root or storage root is read or created.
    Check(CheckArgs),
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct CheckArgs {
    /// Broker configuration file or configuration directory.
    #[arg(value_name = "CONFIG")]
    config: PathBuf,
    /// Operator-authored provider set to resolve when the configuration names a providerSet.
    #[arg(long, value_name = "PATH", requires = "store")]
    provider_set: Option<PathBuf>,
    /// Private directory that keeps the resolved provider lock and blob store between checks.
    #[arg(long, value_name = "DIR", requires = "provider_set")]
    store: Option<PathBuf>,
    /// Directory of essential workflows, every *.yaml file, run against the candidate providers
    /// and policy.
    #[arg(long, value_name = "DIR", requires = "provider_set")]
    workflows: Option<PathBuf>,
    /// Render problems and warnings as a table or JSON.
    #[arg(long, value_enum, default_value_t)]
    output: OutputFormat,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct ProviderArgs {
    /// Operator-authored exact provider references.
    #[arg(long, value_name = "PATH", global = true)]
    provider_set: Option<PathBuf>,
    /// Generated immutable provider activation lock.
    #[arg(long, value_name = "PATH", global = true)]
    lock_file: Option<PathBuf>,
    /// Content-addressed provider store.
    #[arg(long, value_name = "PATH", global = true)]
    store: Option<PathBuf>,
    /// Permit plain HTTP to this exact literal loopback registry authority.
    #[arg(
        long = "plaintext-loopback-registry",
        value_name = "HOST[:PORT]",
        global = true
    )]
    plaintext_loopback_registries: Vec<String>,
    /// Render command results as a table or JSON.
    #[arg(long, value_enum, default_value_t, global = true)]
    output: OutputFormat,
    #[command(subcommand)]
    command: ProviderCommand,
}

/// How an offline operator command renders its result.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    #[default]
    Table,
    Json,
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum ProviderCommand {
    /// Resolve changed exact references, fetch missing blobs, validate, and activate.
    Sync {
        /// Refuse any resolution change and materialize only the existing lock.
        #[arg(long)]
        locked: bool,
    },
    /// Show locked references and local verification state without network access.
    List,
    /// Compile and repair locked components, then prune obsolete cwasm files.
    Precompile,
    /// Export locked providers' embedded skills without network access.
    Skills {
        /// Empty or absent destination for skill directories.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
    },
    /// Verify locked bytes and the complete provider set without network access.
    Verify,
}

/// Transport crates are silenced explicitly: an HTTP stack logs every connection. The
/// OTLP exporter's own diagnostics are silenced by `dekopon_telemetry`, which appends that
/// directive to every OTLP layer it installs; category targets run at the standard level.
#[cfg(unix)]
const OTEL_TRACE_FILTER: &str = "dekopon_brokerd=trace,dekopon_broker=trace,dekopon_broker_host=trace,dekopon_http_host=trace,gateway=debug,prompt=debug,model=debug,asset=debug,shell=debug,job=debug,broker=debug,provider=debug,http=debug,credential=debug,memory=debug,telemetry=debug,hyper=off,h2=off,reqwest=off";

/// Log records ride the same crate set as spans, because the broker's audit record is one of them.
///
/// A trace that carries every decision span but not the `broker.decision` record it produced is
/// exactly the split the constitution removed, so the two signals select the same targets.
#[cfg(unix)]
const OTEL_LOG_FILTER: &str = OTEL_TRACE_FILTER;

#[cfg(unix)]
const MAX_RUNTIME_THREADS: usize = 4096;

#[cfg(unix)]
#[expect(
    clippy::disallowed_methods,
    reason = "owner: main passes it to observed::finish for shutdown; bound: one runtime, configured workers and blocking cap or Tokio defaults"
)]
fn runtime_builder(cli: &Cli) -> Result<tokio::runtime::Builder, clap::Error> {
    let workers = match cli.worker_threads {
        Some(workers) => workers.get(),
        None => match std::env::var("TOKIO_WORKER_THREADS") {
            Ok(value) => value
                .parse::<std::num::NonZeroUsize>()
                .map_err(|error| {
                    Cli::command().error(
                        ErrorKind::ValueValidation,
                        format!("invalid TOKIO_WORKER_THREADS: {error}"),
                    )
                })?
                .get(),
            Err(std::env::VarError::NotPresent) => {
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
            }
            Err(error) => {
                return Err(Cli::command().error(ErrorKind::ValueValidation, error));
            }
        },
    };
    let blocking = cli
        .blocking_threads
        .map_or(512, std::num::NonZeroUsize::get);
    if workers > MAX_RUNTIME_THREADS || blocking > MAX_RUNTIME_THREADS {
        return Err(Cli::command().error(
            ErrorKind::ValueValidation,
            format!("worker and blocking thread counts must each be at most {MAX_RUNTIME_THREADS}"),
        ));
    }
    if workers.checked_add(blocking).is_none() {
        return Err(Cli::command().error(
            ErrorKind::ValueValidation,
            "worker threads plus blocking threads exceed the platform thread-count limit",
        ));
    }

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    let next = std::sync::atomic::AtomicUsize::new(0);
    builder.enable_all().thread_name_fn(move || {
        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 100_000_000;
        format!("bk-rt-{index}")
    });
    builder
        .worker_threads(workers)
        .max_blocking_threads(blocking);
    if let Some(keep_alive) = cli.blocking_keep_alive {
        builder.thread_keep_alive(keep_alive.into());
    }
    Ok(builder)
}

#[cfg(unix)]
fn cpu_pool_builder(cli: &Cli) -> Result<rayon::ThreadPoolBuilder, clap::Error> {
    let threads = match cli.cpu_threads {
        Some(threads) => threads.get(),
        None => match std::env::var("RAYON_NUM_THREADS") {
            Ok(value) => value
                .parse::<std::num::NonZeroUsize>()
                .map_err(|error| {
                    Cli::command().error(
                        ErrorKind::ValueValidation,
                        format!("invalid RAYON_NUM_THREADS: {error}"),
                    )
                })?
                .get(),
            Err(std::env::VarError::NotPresent) => {
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
            }
            Err(error) => {
                return Err(Cli::command().error(ErrorKind::ValueValidation, error));
            }
        },
    };
    if threads > MAX_RUNTIME_THREADS {
        return Err(Cli::command().error(
            ErrorKind::ValueValidation,
            format!("CPU thread count must be at most {MAX_RUNTIME_THREADS}"),
        ));
    }
    Ok(rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("bk-cpu-{}", index % 100_000_000)))
}

#[cfg(unix)]
fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(error) = validate_cli(&cli) {
        if let Err(print_error) = error.print() {
            eprintln!("dekopon-brokerd: could not print command-line error: {print_error}");
        }
        return ExitCode::from(2);
    }
    let mut builder = runtime_builder(&cli).unwrap_or_else(|error| error.exit());
    let cpu_builder = cpu_pool_builder(&cli).unwrap_or_else(|error| error.exit());
    if let Err(error) = cpu_builder.build_global() {
        eprintln!("dekopon-brokerd: could not start the CPU pool: {error}");
        return ExitCode::FAILURE;
    }
    let runtime = match builder.build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("dekopon-brokerd: could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    observed::finish(runtime, execute(cli))
}

#[cfg(unix)]
fn validate_cli(cli: &Cli) -> Result<(), clap::Error> {
    match (&cli.command, &cli.config) {
        (None, None) => Err(Cli::command().error(
            ErrorKind::MissingRequiredArgument,
            "--config <PATH> is required when serving the broker",
        )),
        (Some(_), Some(_)) => Err(Cli::command().error(
            ErrorKind::ArgumentConflict,
            "--config cannot be used with an offline operator command",
        )),
        (Some(Command::Provider(provider)), _)
            if matches!(provider.command, ProviderCommand::Precompile)
                && provider.provider_set.is_some() =>
        {
            Err(Cli::command().error(
                ErrorKind::ArgumentConflict,
                "provider precompile reads the lock and does not take --provider-set",
            ))
        }
        (Some(Command::Provider(provider)), _)
            if provider.lock_file.is_none()
                || provider.store.is_none()
                || (matches!(provider.command, ProviderCommand::Sync { .. })
                    && provider.provider_set.is_none()) =>
        {
            Err(Cli::command().error(
                ErrorKind::MissingRequiredArgument,
                "provider mode requires --lock-file and --store; sync also requires --provider-set",
            ))
        }
        _ => Ok(()),
    }
}

/// The one dispatch over the parsed command line.
///
/// Each arm brings the process state its mode needs, which is why the setup lives here rather than
/// ahead of the match: a health check must discover no credentials and initialize no telemetry or
/// provider machinery, and provider management must read no daemon configuration.
#[cfg(unix)]
async fn execute(cli: Cli) -> ExitCode {
    match cli.command {
        Some(Command::Probe { socket }) => match probe(&socket).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("dekopon-brokerd: probe failed: {}", error_chain(&error));
                ExitCode::FAILURE
            }
        },
        Some(Command::Provider(provider)) => {
            let install = Install::new(Console {
                format: ConsoleFormat::Text {
                    ansi: None,
                    target: true,
                    timestamps: true,
                },
                writer: ConsoleWriter::Stderr,
                filter: ConsoleFilter::Environment("warn".to_owned()),
            });
            observed(install, execute_provider(provider), None).await
        }
        Some(Command::Check(check)) => {
            let install = Install::new(Console {
                format: ConsoleFormat::Text {
                    ansi: None,
                    target: false,
                    timestamps: false,
                },
                writer: ConsoleWriter::Stderr,
                filter: ConsoleFilter::Environment("error".to_owned()),
            });
            observed(install, execute_check(check), None).await
        }
        None => {
            let config = cli
                .config
                .expect("validate_cli requires daemon configuration");
            let settings =
                dekopon_brokerd::telemetry_settings(&config, dekopon_brokerd::current_uid())
                    .await
                    .ok()
                    .flatten();
            // Telemetry setup must never block broker startup: authorization and audit are the
            // service's contract, observability is not, so a telemetry failure costs a missing
            // dashboard, not a working authority boundary.
            let exporter = settings.as_ref().map(|telemetry| &telemetry.settings);
            let tracer_provider =
                dekopon_telemetry::optional_tracer_provider(exporter, "dekopon-brokerd");
            let logger_provider =
                dekopon_telemetry::optional_logger_provider(exporter, "dekopon-brokerd");
            let mut install = Install::new(Console {
                format: ConsoleFormat::Json,
                writer: ConsoleWriter::Stdout("bk-stdout"),
                filter: ConsoleFilter::Environment("info".to_owned()),
            });
            if let Some(provider) = tracer_provider {
                install = install.with_traces(provider, "dekopon-brokerd", OTEL_TRACE_FILTER);
            }
            if let Some(provider) = logger_provider {
                install = install.with_logs(provider, OTEL_LOG_FILTER);
            }
            let deadline = dekopon_brokerd::ShutdownDeadline::default();
            observed(
                install,
                execute_server(config, deadline.clone()),
                Some(deadline),
            )
            .await
        }
    }
}

#[cfg(unix)]
async fn probe(socket: &std::path::Path) -> Result<(), AppError> {
    use dekopon_broker_protocol::{BrokerClient, FrameLimits};
    let client = BrokerClient::new(
        socket,
        dekopon_brokerd::current_uid(),
        FrameLimits::default(),
    )
    .map_err(AppError::Probe)?;
    tokio::time::timeout(std::time::Duration::from_secs(2), client.capabilities())
        .await
        .map_err(AppError::ProbeTimeout)?
        .map_err(AppError::Probe)?;
    Ok(())
}

#[cfg(unix)]
async fn execute_server(
    config: PathBuf,
    deadline: dekopon_brokerd::ShutdownDeadline,
) -> Result<(), AppError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(AppError::Signal)?;
    let shutdown = async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    tracing::error!(
                        event = "broker_signal_failed",
                        signal = "interrupt",
                        "broker.signal.failed"
                    );
                }
            }
            _ = terminate.recv() => {}
        }
    };
    dekopon_brokerd::run(config, shutdown, deadline)
        .await
        .map_err(AppError::Broker)?;
    Ok(())
}

#[cfg(unix)]
async fn execute_provider(provider: ProviderArgs) -> Result<(), AppError> {
    let output = provider.output;
    let manager = dekopon_brokerd::ProviderManager::new(dekopon_brokerd::ProviderManagerOptions {
        paths: dekopon_brokerd::ProviderManagerPaths {
            provider_set: provider.provider_set,
            lock_file: provider
                .lock_file
                .expect("validate_cli requires --lock-file"),
            store: provider.store.expect("validate_cli requires --store"),
        },
        plaintext_loopback_registries: provider.plaintext_loopback_registries,
    })
    .await
    .map_err(AppError::Provider)?;
    match provider.command {
        ProviderCommand::Sync { locked } => {
            let report = if locked {
                manager.sync_locked().await
            } else {
                manager.sync().await
            }
            .map_err(AppError::Provider)?;
            render(output, &report, || {
                format!(
                    "PROVIDERS\tFETCHED\tLOCK-CHANGED\tRESTART-REQUIRED\n{}\t{}\t{}\t{}",
                    report.providers, report.fetched, report.lock_changed, report.restart_required
                )
            })?;
            if report.restart_required {
                eprintln!("a running broker drains and restarts once it reads the changed lock");
            }
        }
        ProviderCommand::List => {
            let statuses = manager.list().await.map_err(AppError::Provider)?;
            render(output, &statuses, || {
                let mut table =
                    String::from("PROVIDER\tSOURCE\tMANIFEST\tCOMPONENT\tLOCAL\tREASON\n");
                for status in &statuses {
                    use std::fmt::Write as _;
                    writeln!(
                        &mut table,
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        status.provider_id,
                        status.source,
                        status.manifest_digest,
                        status.component_digest,
                        status.local_status,
                        status.local_reason.as_deref().unwrap_or("-")
                    )
                    .expect("writing to a String cannot fail");
                }
                table.pop();
                table
            })?;
        }
        ProviderCommand::Skills { out } => {
            let rows = manager.skills(&out).await.map_err(AppError::Provider)?;
            render(output, &rows, || {
                use std::fmt::Write as _;
                let mut table = String::from("PROVIDER\tSKILL\tFILES\tBYTES\tSHA256\n");
                for row in &rows {
                    writeln!(
                        &mut table,
                        "{}\t{}\t{}\t{}\t{}",
                        row.provider, row.skill, row.files, row.bytes, row.sha256
                    )
                    .expect("writing to a String cannot fail");
                }
                table.pop();
                table
            })?;
        }
        ProviderCommand::Verify => {
            let report = manager.verify().await.map_err(AppError::Provider)?;
            render(output, &report, || {
                format!("verified {} locked provider(s)", report.providers)
            })?;
        }
        ProviderCommand::Precompile => {
            let report = manager.precompile().await.map_err(AppError::Provider)?;
            render(output, &report, || {
                format!(
                    "PROVIDERS\tCOMPILED\tREPAIRED\tREMOVED-FILES\tREMOVED-BYTES\n{}\t{}\t{}\t{}\t{}",
                    report.providers,
                    report.compiled,
                    report.repaired,
                    report.removed_files,
                    report.removed_bytes
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_digest: Option<String>,
    ok: bool,
    problems: Vec<String>,
    warnings: Vec<String>,
    #[serde(rename = "peakRssBytes")]
    peak_rss_bytes: Option<u64>,
}

#[cfg(unix)]
async fn execute_check(check: CheckArgs) -> Result<(), AppError> {
    let providers = check
        .provider_set
        .zip(check.store)
        .map(|(provider_set, store)| dekopon_brokerd::CheckProviders {
            provider_set,
            store,
        });
    let report = dekopon_brokerd::check(&check.config, providers, check.workflows.as_deref()).await;
    let output = CheckOutput {
        policy_digest: report.policy_digest,
        identity_digest: report.identity_digest,
        ok: report.problems.is_empty(),
        problems: report
            .problems
            .iter()
            .map(|problem| error_chain(problem))
            .collect(),
        warnings: report.warnings.iter().map(ToString::to_string).collect(),
        peak_rss_bytes: report.peak_rss_bytes,
    };
    render(check.output, &output, || {
        let lines = output
            .problems
            .iter()
            .map(|problem| format!("problem\t{problem}"))
            .chain(
                output
                    .warnings
                    .iter()
                    .map(|warning| format!("warning\t{warning}")),
            )
            .chain(
                output
                    .policy_digest
                    .as_ref()
                    .map(|digest| format!("policy.digest\t{digest}")),
            )
            .chain(
                output
                    .identity_digest
                    .as_ref()
                    .map(|digest| format!("identity.digest\t{digest}")),
            )
            .chain(output.ok.then(|| "ok".to_owned()))
            .collect::<Vec<_>>();
        lines.join("\n")
    })?;
    if output.ok {
        Ok(())
    } else {
        Err(AppError::CheckFailed {
            problems: output.problems.len(),
        })
    }
}

#[cfg(unix)]
fn render<T: Serialize>(
    output: OutputFormat,
    value: &T,
    table: impl FnOnce() -> String,
) -> Result<(), AppError> {
    match output {
        OutputFormat::Table => println!("{}", table()),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(AppError::Output)?
        ),
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Error)]
enum AppError {
    #[error("broker probe exceeded two-second deadline")]
    ProbeTimeout(#[source] tokio::time::error::Elapsed),
    #[error("broker probe failed")]
    Probe(#[source] dekopon_broker_protocol::ClientError),
    #[error("could not install termination signal handler")]
    Signal(#[source] io::Error),
    #[error("broker service failed")]
    Broker(#[source] dekopon_brokerd::BrokerdError),
    #[error("provider manager failed")]
    Provider(#[source] dekopon_brokerd::ProviderManagerError),
    #[error("could not render command output")]
    Output(#[source] serde_json::Error),
    #[error("configuration check found {problems} problem(s)")]
    CheckFailed { problems: usize },
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

    use clap::{CommandFactory as _, Parser as _};

    use super::{Cli, Command, OutputFormat, ProviderCommand, validate_cli};

    #[test]
    fn probe_requires_only_a_socket_and_rejects_authority_arguments() {
        let cli = Cli::try_parse_from(["dekopon-brokerd", "probe", "--socket", "/run/broker.sock"])
            .unwrap();
        assert!(validate_cli(&cli).is_ok());
        assert!(
            matches!(cli.command, Some(Command::Probe { socket }) if socket == Path::new("/run/broker.sock"))
        );
        assert!(Cli::try_parse_from(["dekopon-brokerd", "probe"]).is_err());
        for flag in ["--server-uid", "--principal", "--provider", "--credential"] {
            assert!(
                Cli::try_parse_from(["dekopon-brokerd", "probe", "--socket", "x", flag, "x"])
                    .is_err()
            );
        }
        let cli = Cli::try_parse_from(["dekopon-brokerd", "--config=x", "probe", "--socket", "x"])
            .unwrap();
        assert!(validate_cli(&cli).is_err());
    }

    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn retired_http_listener_flag_is_refused() {
        assert!(
            Cli::try_parse_from([
                "dekopon-brokerd",
                "--config=broker.yaml",
                "--http-bind=0.0.0.0:8080"
            ])
            .is_err()
        );
    }

    #[test]
    fn daemon_mode_still_requires_config() {
        let cli = Cli::try_parse_from(["dekopon-brokerd"]).expect("shape parses before validation");
        assert!(validate_cli(&cli).is_err());
    }

    #[test]
    fn provider_mode_is_independent_of_daemon_configuration() {
        let cli = Cli::try_parse_from([
            "dekopon-brokerd",
            "provider",
            "sync",
            "--locked",
            "--provider-set",
            "providers.yaml",
            "--lock-file",
            "providers.lock.yaml",
            "--store",
            "store",
            "--output",
            "json",
        ])
        .expect("provider command parses");
        assert!(validate_cli(&cli).is_ok());
        let Some(Command::Provider(provider)) = cli.command else {
            panic!("provider command");
        };
        assert_eq!(provider.output, OutputFormat::Json);
        assert!(matches!(
            provider.command,
            ProviderCommand::Sync { locked: true }
        ));

        let list = Cli::try_parse_from([
            "dekopon-brokerd",
            "provider",
            "list",
            "--lock-file",
            "providers.lock.yaml",
            "--store",
            "store",
        ])
        .expect("offline list parses without desired state");
        assert!(validate_cli(&list).is_ok());
    }

    #[test]
    fn skills_has_its_own_required_destination_and_accepts_global_flags() {
        let cli = Cli::try_parse_from([
            "dekopon-brokerd",
            "provider",
            "--lock-file=/store/providers.lock.yaml",
            "--store=/store/store",
            "skills",
            "--out",
            "/config/provider-skills",
            "--output",
            "json",
        ])
        .expect("skills parses");
        assert!(validate_cli(&cli).is_ok());
        let Some(Command::Provider(provider)) = cli.command else {
            panic!("provider command")
        };
        assert_eq!(provider.output, OutputFormat::Json);
        assert!(
            matches!(provider.command, ProviderCommand::Skills { out } if out == std::path::Path::new("/config/provider-skills"))
        );
        let error = Cli::try_parse_from(["dekopon-brokerd", "provider", "skills"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(error.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn check_output_serializes_peak_rss_as_number_or_null() {
        for peak in [Some(1234_u64), None] {
            let output = super::CheckOutput {
                policy_digest: None,
                identity_digest: None,
                ok: true,
                problems: Vec::new(),
                warnings: Vec::new(),
                peak_rss_bytes: peak,
            };
            assert_eq!(
                serde_json::to_value(output).expect("JSON output"),
                serde_json::json!({
                    "ok": true, "problems": [], "warnings": [], "peakRssBytes": peak
                })
            );
        }
    }

    #[test]
    fn precompile_uses_only_global_provider_flags() {
        let cli = Cli::try_parse_from([
            "dekopon-brokerd",
            "provider",
            "--lock-file=providers.lock.yaml",
            "--store=store",
            "--output=json",
            "precompile",
        ])
        .expect("precompile parses without a provider set");
        assert!(validate_cli(&cli).is_ok());
        let Some(Command::Provider(provider)) = cli.command else {
            panic!("provider command");
        };
        assert!(matches!(provider.command, ProviderCommand::Precompile));
        assert_eq!(provider.output, OutputFormat::Json);
        assert!(
            Cli::try_parse_from([
                "dekopon-brokerd",
                "provider",
                "--lock-file=lock",
                "--store=store",
                "precompile",
                "--compile-threads=2",
            ])
            .is_err()
        );
        let with_set = Cli::try_parse_from([
            "dekopon-brokerd",
            "provider",
            "--lock-file=lock",
            "--store=store",
            "--provider-set=providers.yaml",
            "precompile",
        ])
        .expect("global syntax parses");
        assert!(validate_cli(&with_set).is_err());
    }

    #[test]
    fn check_takes_a_positional_configuration_and_pairs_the_provider_set_with_a_store() {
        let cli = Cli::try_parse_from([
            "dekopon-brokerd",
            "check",
            "broker.d",
            "--provider-set",
            "providers.yaml",
            "--store",
            "cache",
            "--output",
            "json",
        ])
        .expect("check parses");
        assert!(validate_cli(&cli).is_ok());
        let Some(Command::Check(check)) = cli.command else {
            panic!("check command");
        };
        assert_eq!(check.config, Path::new("broker.d"));
        assert_eq!(check.output, OutputFormat::Json);

        for unpaired in [
            [
                "dekopon-brokerd",
                "check",
                "broker.d",
                "--provider-set",
                "p.yaml",
            ],
            ["dekopon-brokerd", "check", "broker.d", "--store", "cache"],
        ] {
            assert!(Cli::try_parse_from(unpaired).is_err());
        }
        assert!(Cli::try_parse_from(["dekopon-brokerd", "check"]).is_err());
        let daemon = Cli::try_parse_from(["dekopon-brokerd", "--config=x", "check", "broker.d"])
            .expect("shape parses before validation");
        assert!(validate_cli(&daemon).is_err());
    }

    #[test]
    fn check_workflows_requires_a_provider_set() {
        let cli = Cli::try_parse_from([
            "dekopon-brokerd",
            "check",
            "broker.d",
            "--provider-set=providers.yaml",
            "--store=cache",
            "--workflows=workflows.d",
        ])
        .expect("workflows beside a provider set parses");
        let Some(Command::Check(check)) = cli.command else {
            panic!("check command");
        };
        assert_eq!(check.workflows.as_deref(), Some(Path::new("workflows.d")));

        let error = Cli::try_parse_from([
            "dekopon-brokerd",
            "check",
            "broker.d",
            "--workflows=workflows.d",
        ])
        .expect_err("workflows without a provider set is a usage error");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn retired_audit_command_is_rejected() {
        let error = Cli::try_parse_from([
            "dekopon-brokerd",
            "audit",
            "verify",
            "--audit-path=audit.jsonl",
        ])
        .expect_err("retired command is not executable");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("dekopon-brokerd requires Unix peer credentials and Unix-domain sockets");
    std::process::ExitCode::FAILURE
}

#[cfg(all(test, unix))]
mod runtime_sizing_tests {
    use super::*;
    use std::{collections::BTreeSet, process::Command, time::Duration};

    #[test]
    fn flags_and_environment() {
        const CHILD: &str = "DEKOPON_RUNTIME_SIZING_TEST";
        let variables = [
            "DEKOPON_WORKER_THREADS",
            "DEKOPON_BLOCKING_THREADS",
            "DEKOPON_BLOCKING_KEEP_ALIVE",
            "DEKOPON_CPU_THREADS",
        ];
        let mode = std::env::var(CHILD).ok();
        let Some(mode) = mode else {
            for case in ["defaults", "environment", "0", "1", "2", "3"] {
                let mut child = Command::new(std::env::current_exe().unwrap());
                child
                    .args(["--exact", "runtime_sizing_tests::flags_and_environment"])
                    .env(CHILD, case);
                for (variable, value) in variables.iter().zip(["2", "3", "250ms", "4"]) {
                    if case == "defaults" {
                        child.env_remove(variable);
                    } else {
                        child.env(variable, value);
                    }
                }
                if let Ok(index) = case.parse::<usize>() {
                    child.env(variables[index], if index == 2 { "invalid" } else { "0" });
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        if mode.parse::<usize>().is_ok() {
            let error = Cli::try_parse_from(["daemon"]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        } else {
            let cli = Cli::try_parse_from(["daemon"]).unwrap();
            if mode == "defaults" {
                assert_eq!(cli.worker_threads, None);
                assert_eq!(cli.blocking_threads, None);
                assert_eq!(cli.blocking_keep_alive, None);
                assert_eq!(cli.cpu_threads.map(std::num::NonZeroUsize::get), None);
            } else {
                assert_eq!(cli.worker_threads.unwrap().get(), 2);
                assert_eq!(cli.blocking_threads.unwrap().get(), 3);
                assert_eq!(
                    Duration::from(cli.blocking_keep_alive.unwrap()),
                    Duration::from_millis(250)
                );
                assert_eq!(cli.cpu_threads.map(std::num::NonZeroUsize::get), Some(4));
            }
            for argument in [
                "--worker-threads=0",
                "--blocking-threads=0",
                "--blocking-keep-alive=invalid",
                "--cpu-threads=0",
            ] {
                let error = Cli::try_parse_from(["daemon", argument]).unwrap_err();
                assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
            }
        }
        let cli = Cli::try_parse_from([
            "daemon",
            "--worker-threads=3",
            "--blocking-threads=4",
            "--blocking-keep-alive=500ms",
            "--cpu-threads=5",
        ])
        .unwrap();
        assert_eq!(cli.worker_threads.unwrap().get(), 3);
        assert_eq!(cli.blocking_threads.unwrap().get(), 4);
        assert_eq!(
            Duration::from(cli.blocking_keep_alive.unwrap()),
            Duration::from_millis(500)
        );
        assert_eq!(cli.cpu_threads.map(std::num::NonZeroUsize::get), Some(5));
    }

    #[test]
    fn runtime_thread_limits_are_cli_errors() {
        const CHILD: &str = "DEKOPON_RUNTIME_LIMIT_TEST";
        let arguments = ["daemon"];
        if std::env::var_os(CHILD).is_some() {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let error = runtime_builder(&cli).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation);
            assert_eq!(error.exit_code(), 2);
            return;
        }
        let invalid_counts = [usize::MAX, usize::MAX - 512, MAX_RUNTIME_THREADS + 1];
        for count in invalid_counts {
            for flag in ["--worker-threads", "--blocking-threads"] {
                let cli = Cli::try_parse_from(
                    arguments
                        .into_iter()
                        .map(str::to_owned)
                        .chain([format!("{flag}={count}")]),
                )
                .unwrap();
                let error = runtime_builder(&cli).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::ValueValidation);
                assert_eq!(error.exit_code(), 2);
            }
        }
        let variables = [
            "DEKOPON_WORKER_THREADS",
            "DEKOPON_BLOCKING_THREADS",
            "TOKIO_WORKER_THREADS",
        ];
        for count in invalid_counts {
            for variable in variables {
                let mut child = Command::new(std::env::current_exe().unwrap());
                child.args([
                    "--exact",
                    "runtime_sizing_tests::runtime_thread_limits_are_cli_errors",
                ]);
                for variable in variables {
                    child.env_remove(variable);
                }
                let output = child
                    .env(CHILD, "1")
                    .env(variable, count.to_string())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{variable}={count}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    #[test]
    fn cpu_thread_limits_are_cli_errors() {
        const CHILD: &str = "DEKOPON_CPU_LIMIT_TEST";
        if std::env::var_os(CHILD).is_some() {
            let error = Cli::try_parse_from(["daemon"])
                .and_then(|cli| cpu_pool_builder(&cli))
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation);
            assert_eq!(error.exit_code(), 2);
            return;
        }
        let invalid_counts = [usize::MAX, usize::MAX - 512, MAX_RUNTIME_THREADS + 1];
        for count in invalid_counts {
            let cli = Cli::try_parse_from(["daemon".to_owned(), format!("--cpu-threads={count}")])
                .unwrap();
            let error = cpu_pool_builder(&cli).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ValueValidation);
            assert_eq!(error.exit_code(), 2);
        }
        for variable in ["DEKOPON_CPU_THREADS", "RAYON_NUM_THREADS"] {
            let mut values: Vec<_> = invalid_counts.into_iter().map(|n| n.to_string()).collect();
            if variable == "RAYON_NUM_THREADS" {
                values.extend(["0".to_owned(), "invalid".to_owned()]);
            }
            for value in values {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "runtime_sizing_tests::cpu_thread_limits_are_cli_errors",
                    ])
                    .env_remove("DEKOPON_CPU_THREADS")
                    .env_remove("RAYON_NUM_THREADS")
                    .env(CHILD, "1")
                    .env(variable, &value)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{variable}={value}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    #[test]
    fn configured_runtime_names_workers_and_blocking_threads() {
        let cli = Cli::parse_from(["daemon", "--worker-threads=2", "--blocking-threads=1"]);
        let runtime = runtime_builder(&cli).unwrap().build().unwrap();
        assert_eq!(runtime.metrics().num_workers(), 2);
        let workers = runtime.block_on(async {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..2 {
                let barrier = std::sync::Arc::clone(&barrier);
                tasks.spawn(async move {
                    let name = std::thread::current().name().unwrap().to_owned();
                    barrier.wait();
                    name
                });
            }
            let mut names = BTreeSet::new();
            while let Some(name) = tasks.join_next().await {
                names.insert(name.unwrap());
            }
            names
        });
        assert_eq!(
            workers,
            BTreeSet::from(["bk-rt-0".to_owned(), "bk-rt-1".to_owned()])
        );
        let blocking = runtime.block_on(async {
            tokio::task::spawn_blocking(|| std::thread::current().name().unwrap().to_owned())
                .await
                .unwrap()
        });
        assert!(blocking.starts_with("bk-rt-"));
        assert!(blocking.len() <= 15);
        runtime.shutdown_timeout(Duration::from_secs(1));
    }
    #[test]
    fn global_cpu_pool_has_the_selected_size_and_names() {
        const CHILD: &str = "DEKOPON_CPU_POOL_TEST";
        let mode = std::env::var(CHILD).ok();
        let Some(mode) = mode else {
            for (mode, dekopon, rayon) in [
                ("flag", Some("3"), Some("1000000")),
                ("environment", Some("2"), Some("1000000")),
                ("legacy", None, Some("2")),
                ("default", None, None),
            ] {
                let mut child = Command::new(std::env::current_exe().unwrap());
                child
                    .args([
                        "--exact",
                        "runtime_sizing_tests::global_cpu_pool_has_the_selected_size_and_names",
                    ])
                    .env(CHILD, mode);
                for (variable, value) in [
                    ("DEKOPON_CPU_THREADS", dekopon),
                    ("RAYON_NUM_THREADS", rayon),
                ] {
                    if let Some(value) = value {
                        child.env(variable, value);
                    } else {
                        child.env_remove(variable);
                    }
                }
                let output = child.output().unwrap();
                assert!(
                    output.status.success(),
                    "{mode}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        let arguments = if mode == "flag" {
            vec!["broker", "--cpu-threads=2"]
        } else {
            vec!["broker"]
        };
        let expected = if mode == "default" {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        } else {
            2
        };
        let cli = Cli::parse_from(arguments);
        cpu_pool_builder(&cli).unwrap().build_global().unwrap();
        assert_eq!(rayon::current_num_threads(), expected);
        let names: BTreeSet<_> =
            rayon::broadcast(|_| std::thread::current().name().unwrap().to_owned())
                .into_iter()
                .collect();
        assert_eq!(
            names,
            (0..expected).map(|n| format!("bk-cpu-{n}")).collect()
        );
    }
}
