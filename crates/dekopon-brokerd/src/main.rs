#[cfg(unix)]
use std::{io, net::SocketAddr, path::PathBuf, process::ExitCode};

#[cfg(unix)]
use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum, error::ErrorKind};
#[cfg(unix)]
use opentelemetry::trace::TracerProvider as _;
#[cfg(unix)]
use serde::Serialize;
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
#[cfg(unix)]
use tracing_subscriber::{
    EnvFilter, Layer as _, fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(
    name = "dekopon-brokerd",
    version,
    about = "Run the authenticated broker or manage its provider set"
)]
struct Cli {
    /// Strict owner-controlled broker YAML/JSON configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Bind the unauthenticated, read-only operational web UI.
    #[arg(long, value_name = "ADDRESS")]
    http_bind: Option<SocketAddr>,
    /// Offline operator mode. Omit to serve the broker.
    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Resolve, materialize, and verify a startup-fixed provider set.
    Provider(ProviderArgs),
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
    output: ProviderOutput,
    #[command(subcommand)]
    command: ProviderCommand,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum ProviderOutput {
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
    /// Verify locked bytes and the complete provider set without network access.
    Verify,
}

/// Transport crates are silenced explicitly: an OTLP exporter that logs through `tracing` would
/// feed its own export failures back into itself.
#[cfg(unix)]
const OTEL_TRACE_FILTER: &str = "dekopon_brokerd=trace,dekopon_broker=trace,dekopon_broker_host=trace,dekopon_http_host=trace,hyper=off,h2=off,opentelemetry=off,tonic=off,reqwest=off";

#[cfg(unix)]
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(error) = validate_cli(&cli) {
        if let Err(print_error) = error.print() {
            eprintln!("dekopon-brokerd: could not print command-line error: {print_error}");
        }
        return ExitCode::from(2);
    }
    let provider_mode = cli.command.is_some();

    // Provider management never reads daemon configuration and never installs telemetry. It is an
    // offline operator mode with command output on stdout and diagnostics on stderr.
    let settings = match (&cli.command, &cli.config) {
        (None, Some(config)) => {
            dekopon_brokerd::telemetry_settings(config, dekopon_brokerd::current_uid())
                .await
                .ok()
                .flatten()
        }
        _ => None,
    };

    let tracer_provider = match settings
        .as_ref()
        .map(|telemetry| telemetry.settings.tracer_provider())
    {
        Some(Ok(provider)) => Some(provider),
        // Telemetry must never keep the broker from starting. Authorization and audit are the
        // service's contract; observability is not, and failing closed here would trade a working
        // authority boundary for a missing dashboard.
        Some(Err(error)) => {
            eprintln!("dekopon-brokerd: telemetry disabled: {error}");
            None
        }
        None => None,
    };

    let subscriber_result = if provider_mode {
        tracing_subscriber::registry()
            .with(fmt::layer().with_writer(io::stderr).with_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
            ))
            .try_init()
    } else {
        // Structured JSON on stdout is the daemon log contract; a collector or shipper can pick it
        // up without the broker holding a second credential.
        let stdout_layer = fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_writer(io::stdout)
            .with_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            );
        let otel_layer = tracer_provider.as_ref().map(|provider| {
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("dekopon-brokerd"))
                .with_filter(EnvFilter::new(OTEL_TRACE_FILTER))
        });
        tracing_subscriber::registry()
            .with(stdout_layer)
            .with(otel_layer)
            .try_init()
    };
    if subscriber_result.is_err() {
        eprintln!("dekopon-brokerd: could not install tracing subscriber");
        return ExitCode::FAILURE;
    }

    let code = match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(event = "broker_exit", error = %error_chain(&error));
            ExitCode::FAILURE
        }
    };

    if let Some(provider) = tracer_provider {
        // Flush failures are reported but do not change the exit code: the broker's durable audit,
        // not its telemetry, is the record of what happened.
        if let Err(error) = provider.force_flush() {
            tracing::error!(event = "broker_telemetry_flush_failed", error = %error);
        }
        if let Err(error) = provider.shutdown() {
            tracing::error!(event = "broker_telemetry_shutdown_failed", error = %error);
        }
    }
    code
}

#[cfg(unix)]
fn validate_cli(cli: &Cli) -> Result<(), clap::Error> {
    match (&cli.command, &cli.config, &cli.http_bind) {
        (None, None, _) => Err(Cli::command().error(
            ErrorKind::MissingRequiredArgument,
            "--config <PATH> is required when serving the broker",
        )),
        (Some(_), Some(_), _) => Err(Cli::command().error(
            ErrorKind::ArgumentConflict,
            "--config cannot be used with provider operator mode",
        )),
        (Some(_), _, Some(_)) => Err(Cli::command().error(
            ErrorKind::ArgumentConflict,
            "--http-bind cannot be used with provider operator mode",
        )),
        (Some(Command::Provider(provider)), _, _)
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

#[cfg(unix)]
async fn execute(cli: Cli) -> Result<(), AppError> {
    match cli.command {
        Some(Command::Provider(provider)) => execute_provider(provider).await,
        None => {
            execute_server(
                cli.config
                    .expect("validate_cli requires daemon configuration"),
                cli.http_bind,
            )
            .await
        }
    }
}

#[cfg(unix)]
async fn execute_server(config: PathBuf, http_bind: Option<SocketAddr>) -> Result<(), AppError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(AppError::Signal)?;
    let shutdown = async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_err() {
                    tracing::error!(event = "broker_signal_failed", signal = "interrupt");
                }
            }
            _ = terminate.recv() => {}
        }
    };
    dekopon_brokerd::run_with_http(config, http_bind, shutdown)
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
                eprintln!("provider changes apply on the next broker restart");
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
        ProviderCommand::Verify => {
            let report = manager.verify().await.map_err(AppError::Provider)?;
            render(output, &report, || {
                format!("verified {} locked provider(s)", report.providers)
            })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn render<T: Serialize>(
    output: ProviderOutput,
    value: &T,
    table: impl FnOnce() -> String,
) -> Result<(), AppError> {
    match output {
        ProviderOutput::Table => println!("{}", table()),
        ProviderOutput::Json => println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(AppError::Output)?
        ),
    }
    Ok(())
}

#[cfg(unix)]
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(current) = source {
        rendered.push_str(": ");
        rendered.push_str(&current.to_string());
        source = current.source();
    }
    rendered
}

#[cfg(unix)]
#[derive(Debug, Error)]
enum AppError {
    #[error("could not install termination signal handler")]
    Signal(#[source] io::Error),
    #[error("broker service failed")]
    Broker(#[source] dekopon_brokerd::BrokerdError),
    #[error("provider manager failed")]
    Provider(#[source] dekopon_brokerd::ProviderManagerError),
    #[error("could not render provider-manager output")]
    Output(#[source] serde_json::Error),
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, Mutex},
    };

    use clap::{CommandFactory as _, Parser as _};
    use tracing_subscriber::{
        EnvFilter, Layer as _, layer::Context, layer::SubscriberExt as _, registry,
    };

    use super::{Cli, Command, OTEL_TRACE_FILTER, ProviderCommand, ProviderOutput, validate_cli};

    /// Records the target of every event a layer is actually asked to handle.
    #[derive(Clone, Default)]
    struct RecordTargets(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecordTargets {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            self.0
                .lock()
                .expect("target log")
                .push(event.metadata().target().to_owned());
        }
    }

    /// The OTLP layer must never see the exporter's own diagnostics. `internal-logs` is enabled
    /// workspace-wide, so a layer that accepted them would export the failures of its own export.
    /// The SDK crates log under their package names, hyphens and all, which is why the directive
    /// has to be the `opentelemetry` prefix rather than an exact target.
    #[test]
    fn the_otlp_layer_never_sees_the_exporters_own_records() {
        let recorded = RecordTargets::default();
        let subscriber = registry().with(
            recorded
                .clone()
                .with_filter(EnvFilter::new(OTEL_TRACE_FILTER)),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "opentelemetry", "api diagnostic");
            tracing::error!(target: "opentelemetry-sdk", "sdk diagnostic");
            tracing::error!(target: "opentelemetry-otlp", "exporter diagnostic");
            tracing::info!(target: "dekopon_brokerd", "broker event");
        });

        assert_eq!(
            *recorded.0.lock().expect("target log"),
            vec!["dekopon_brokerd".to_owned()]
        );
    }

    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn http_listener_is_explicit_and_accepts_the_documented_spelling() {
        let disabled = Cli::try_parse_from(["dekopon-brokerd", "--config", "broker.yaml"])
            .expect("HTTP is optional");
        assert!(disabled.http_bind.is_none());
        assert!(validate_cli(&disabled).is_ok());

        let enabled = Cli::try_parse_from([
            "dekopon-brokerd",
            "--config=broker.yaml",
            "--http-bind=0.0.0.0:8080",
        ])
        .expect("documented HTTP bind parses");
        assert_eq!(
            enabled.http_bind,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080))
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
        assert_eq!(provider.output, ProviderOutput::Json);
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
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("dekopon-brokerd requires Unix peer credentials and Unix-domain sockets");
    std::process::ExitCode::FAILURE
}
