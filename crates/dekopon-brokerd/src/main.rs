#[cfg(unix)]
use std::{io, net::SocketAddr, path::PathBuf, process::ExitCode};

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
    /// Inspect a durable audit log without starting the broker.
    Audit(AuditArgs),
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
    /// Verify locked bytes and the complete provider set without network access.
    Verify,
}

#[cfg(unix)]
#[derive(Debug, Args)]
struct AuditArgs {
    /// Durable JSONL audit log to read.
    #[arg(long, value_name = "PATH", global = true)]
    audit_path: Option<PathBuf>,
    /// Render command results as a table or JSON.
    #[arg(long, value_enum, default_value_t, global = true)]
    output: OutputFormat,
    #[command(subcommand)]
    command: AuditCommand,
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum AuditCommand {
    /// Verify every retained record's sequence, previous-hash link, and record hash.
    Verify,
}

/// Transport crates are silenced explicitly: an HTTP or gRPC stack logs every connection. The
/// OTLP exporter's own diagnostics are silenced by `dekopon_telemetry`, which appends that
/// directive to every OTLP layer it installs.
#[cfg(unix)]
const OTEL_TRACE_FILTER: &str = "dekopon_brokerd=trace,dekopon_broker=trace,dekopon_broker_host=trace,dekopon_http_host=trace,hyper=off,h2=off,tonic=off,reqwest=off";

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

    // Telemetry must never keep the broker from starting. Authorization and audit are the
    // service's contract; observability is not, and failing closed here would trade a working
    // authority boundary for a missing dashboard.
    let tracer_provider = dekopon_telemetry::optional_tracer_provider(
        settings.as_ref().map(|telemetry| &telemetry.settings),
        "dekopon-brokerd",
    );

    let console = if provider_mode {
        Console {
            format: ConsoleFormat::Text {
                ansi: None,
                target: true,
                timestamps: true,
            },
            writer: ConsoleWriter::Stderr,
            filter: ConsoleFilter::Environment("warn".to_owned()),
        }
    } else {
        // Structured JSON on stdout is the daemon log contract; a collector or shipper can pick it
        // up without the broker holding a second credential.
        Console {
            format: ConsoleFormat::Json,
            writer: ConsoleWriter::Stdout,
            filter: ConsoleFilter::Environment("info".to_owned()),
        }
    };
    let mut install = Install::new(console);
    if let Some(provider) = tracer_provider {
        install = install.with_traces(provider, "dekopon-brokerd", OTEL_TRACE_FILTER);
    }
    let telemetry = match install.install() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("dekopon-brokerd: could not install tracing subscriber: {error}");
            return ExitCode::FAILURE;
        }
    };

    let code = match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(event = "broker_exit", error = %error_chain(&error));
            ExitCode::FAILURE
        }
    };

    // Flush failures are reported but do not change the exit code: the broker's durable audit,
    // not its telemetry, is the record of what happened.
    if let Err(error) = telemetry.shutdown() {
        tracing::error!(event = "broker_telemetry_shutdown_failed", error = %error);
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
            "--config cannot be used with an offline operator command",
        )),
        (Some(_), _, Some(_)) => Err(Cli::command().error(
            ErrorKind::ArgumentConflict,
            "--http-bind cannot be used with an offline operator command",
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
        (Some(Command::Audit(audit)), _, _) if audit.audit_path.is_none() => Err(Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "audit mode requires --audit-path",
            )),
        _ => Ok(()),
    }
}

#[cfg(unix)]
async fn execute(cli: Cli) -> Result<(), AppError> {
    match cli.command {
        Some(Command::Provider(provider)) => execute_provider(provider).await,
        Some(Command::Audit(audit)) => execute_audit(audit),
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
fn execute_audit(audit: AuditArgs) -> Result<(), AppError> {
    let AuditCommand::Verify = audit.command;
    let path = audit
        .audit_path
        .expect("validate_cli requires --audit-path");
    let verification = dekopon_brokerd::verify_audit_file(path).map_err(AppError::Audit)?;
    render(audit.output, &verification, || {
        format!(
            "RECORDS\tHEAD\n{}\t{}",
            verification.records,
            verification.head.as_deref().unwrap_or("-")
        )
    })
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
    #[error("could not install termination signal handler")]
    Signal(#[source] io::Error),
    #[error("broker service failed")]
    Broker(#[source] dekopon_brokerd::BrokerdError),
    #[error("provider manager failed")]
    Provider(#[source] dekopon_brokerd::ProviderManagerError),
    #[error("audit verification failed")]
    Audit(#[source] dekopon_brokerd::AuditVerificationError),
    #[error("could not render provider-manager output")]
    Output(#[source] serde_json::Error),
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::Path,
    };

    use clap::{CommandFactory as _, Parser as _};

    use super::{AuditCommand, Cli, Command, OutputFormat, ProviderCommand, validate_cli};

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

    /// `audit verify` is the only operator path to the audit-chain integrity check, so it must
    /// refuse to run against nothing rather than silently verify an empty default.
    #[test]
    fn audit_mode_requires_a_log_and_rejects_daemon_arguments() {
        let cli = Cli::try_parse_from([
            "dekopon-brokerd",
            "audit",
            "verify",
            "--audit-path",
            "audit.jsonl",
            "--output",
            "json",
        ])
        .expect("audit command parses");
        assert!(validate_cli(&cli).is_ok());
        let Some(Command::Audit(audit)) = cli.command else {
            panic!("audit command");
        };
        assert_eq!(audit.output, OutputFormat::Json);
        assert_eq!(audit.audit_path.as_deref(), Some(Path::new("audit.jsonl")));
        assert!(matches!(audit.command, AuditCommand::Verify));

        let without_path = Cli::try_parse_from(["dekopon-brokerd", "audit", "verify"])
            .expect("shape parses before validation");
        assert!(validate_cli(&without_path).is_err());

        let with_config = Cli::try_parse_from([
            "dekopon-brokerd",
            "--config=broker.yaml",
            "audit",
            "verify",
            "--audit-path=audit.jsonl",
        ])
        .expect("shape parses before validation");
        assert!(validate_cli(&with_config).is_err());
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("dekopon-brokerd requires Unix peer credentials and Unix-domain sockets");
    std::process::ExitCode::FAILURE
}
