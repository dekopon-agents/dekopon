//! `dekopon-console` — an interactive terminal view over a running Dekopon broker.
//!
//! This binary owns exactly one decision the library cannot make for it: what a command line means.
//! Everything after that — connecting, authenticating, drawing, running turns — is `dekopon-tui`.
//!
//! It is an ordinary unprivileged client of `dekopon-brokerd`'s Unix socket. It holds a model
//! credential and nothing else: no policy, no provider credential, no authorization. Every
//! capability call it makes is a proposal the broker alone decides.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]

use std::{
    collections::HashSet,
    io::{self, IsTerminal as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use dekopon_config::load_discovered;
use dekopon_core::{AgentId, ExternalSubject};
use dekopon_tui::OperatorProfile;
use dekopon_tui::{
    App, ConsoleOptions, ModelChoice,
    session::{connect, resolve_console_credential},
};
use serde::Deserialize;
use thiserror::Error;
mod telemetry;

/// Connection, identity, and model settings for the console.
///
/// Every one of these has a resolved default except the subject, so `dekopon-console` with no flags
/// is the ordinary invocation; each flag exists for the deployment that is not the local
/// single-UID one.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "dekopon-console",
    version,
    about = "Interactive terminal console for a running Dekopon broker",
    long_about = None
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent CLI switches preserve the existing command-line contract"
)]
struct Cli {
    /// Path to a YAML or JSON agent catalog.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Broker socket path.
    ///
    /// Resolves as `dekopon-run` documents: this flag, then `$DEKOPON_BROKER_SOCKET`, then
    /// `$XDG_RUNTIME_DIR/dekopon/broker.sock`, then `$HOME/.local/run/dekopon/broker.sock`.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Trusted UID owning the broker process; defaults to the caller's own.
    #[arg(long, value_name = "UID", env = "DEKOPON_BROKER_UID")]
    server_uid: Option<u32>,

    /// Canonical external subject sessions propose on behalf of.
    ///
    /// The broker still has to hold an attestor grant covering its namespace and a mapping
    /// resolving it to a principal; declaring one here grants nothing at all.
    ///
    /// Optional only so it can come from the environment. There is no default: an identity the
    /// console guessed would be an identity nobody chose, and the broker would refuse it anyway one
    /// step later having told you nothing useful.
    #[arg(long, value_name = "SUBJECT", env = "DEKOPON_CONSOLE_SUBJECT")]
    subject: Option<ExternalSubject>,

    /// Exact catalog agent to enter directly; otherwise use the picker.
    #[arg(long, value_name = "AGENT")]
    agent: Option<AgentId>,

    /// Use broker-derived synthetic scope; never replay a real conversation.
    #[arg(long)]
    smoke_conversation: bool,

    /// Operator-authored, read-only profile document.
    #[arg(long, value_name = "PATH")]
    profiles: Option<PathBuf>,

    /// Name of one authored profile, bound to its agent.
    #[arg(
        long,
        value_name = "NAME",
        requires = "profiles",
        conflicts_with = "subject"
    )]
    profile: Option<String>,

    /// Model override; otherwise profile model, then the console default.
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,

    /// Optional OTLP YAML configuration; ignored by --idle.
    #[arg(long, value_name = "PATH")]
    telemetry: Option<PathBuf>,

    /// Idle container PID1: wait for termination; no catalog, broker or model setup.
    ///
    /// Settings that read the environment are not in this list: a container sets them once for
    /// `kubectl exec` sessions, and PID1 inherits the same environment.
    #[arg(long, conflicts_with_all = ["config", "socket", "agent", "profile", "profiles", "model", "endpoint", "api_key_env", "auth_file", "max_steps", "max_capability_calls", "smoke_conversation"])]
    idle: bool,

    /// ChatGPT credential file.
    ///
    /// Defaults to the console's own `chatgpt-auth.console.json` rather than the file every other
    /// surface resolves to, because the refresh token rotates and sharing it would invalidate the
    /// gateway's copy. Passing this explicitly accepts whatever it points at.
    #[arg(long, value_name = "PATH", conflicts_with = "endpoint")]
    auth_file: Option<PathBuf>,

    /// OpenAI-compatible endpoint to use instead of the ChatGPT subscription.
    #[arg(long, value_name = "URL")]
    endpoint: Option<String>,

    /// Name of the environment variable holding the endpoint's bearer token.
    #[arg(long, value_name = "NAME", requires = "endpoint")]
    api_key_env: Option<String>,

    /// Maximum model turns one session may take.
    #[arg(long, value_name = "COUNT", value_parser = clap::value_parser!(u32).range(1..=64))]
    max_steps: Option<u32>,

    /// Capability invocations one session may drive in total.
    #[arg(long, value_name = "COUNT", value_parser = clap::value_parser!(u32).range(1..=256))]
    max_capability_calls: Option<u32>,

    /// Bounded non-TTY command/result transport (requires both stdin and stdout non-TTY).
    #[arg(long, conflicts_with = "idle")]
    structured: bool,

    /// Disable ANSI colors in diagnostics.
    #[arg(long)]
    no_color: bool,

    /// Increase diagnostics (`-v` for info, `-vv` for debug details).
    #[arg(short = 'v', action = clap::ArgAction::Count)]
    verbose: u8,
}

/// Failure opening or running the console.
#[derive(Debug, Error)]
enum ConsoleError {
    /// The catalog would not load.
    #[error(transparent)]
    Config(Box<dekopon_config::ConfigError>),
    /// Connecting, authenticating, or choosing a model failed.
    #[error(transparent)]
    Session(#[from] dekopon_tui::SessionError),
    /// The console ran and the terminal failed under it.
    #[error(transparent)]
    Exit(#[from] dekopon_tui::ConsoleExit),
    /// The async runtime could not be built.
    #[error("could not start the console runtime")]
    Runtime(#[source] io::Error),
    #[error(transparent)]
    Telemetry(#[from] telemetry::TelemetryError),
    /// No subject was supplied.
    #[error(
        "no console identity: pass --subject <SUBJECT>, set DEKOPON_CONSOLE_SUBJECT, or select an authored --profile / defaultProfile; the broker must map the subject and authorize this console UID"
    )]
    NoSubject,
    #[error("profile configuration: {0}")]
    Profile(String),
    #[error("unknown agent {0} in the catalog")]
    UnknownAgent(AgentId),
    #[error("the interactive console needs a TTY on stdin and stdout (use kubectl exec -it)")]
    NoTerminal,
    #[error("--structured requires both stdin and stdout to be non-TTY")]
    StructuredTerminal,
    #[error("--structured requires --agent or a selected --profile")]
    StructuredAgent,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match if cli.idle { idle() } else { run_console(&cli) } {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", dekopon_tui::report::chain(&error));
            ExitCode::FAILURE
        }
    }
}

/// Opens the console.
///
/// Returns before drawing anything when the catalog will not load, no broker is listening, or the
/// credential guard refuses. Once the console is drawing, only a terminal failure comes back here:
/// a refused hop or a failed session is something the console shows and stays open after.
#[expect(
    clippy::too_many_lines,
    reason = "the existing startup sequence validates CLI modes before creating any broker or model client"
)]
fn execute(cli: &Cli, runtime: &tokio::runtime::Runtime) -> Result<(), ConsoleError> {
    let document = match &cli.profiles {
        Some(path) => read_profiles(path)?,
        None => ProfilesDocument::default(),
    };
    let chosen = cli.profile.as_ref().or_else(|| {
        if cli.agent.is_none() {
            document.default_profile.as_ref()
        } else {
            None
        }
    });
    if cli.profile.is_some()
        && !document
            .profiles
            .iter()
            .any(|p| Some(&p.name) == cli.profile.as_ref())
    {
        return Err(ConsoleError::Profile(format!(
            "unknown profile {}",
            cli.profile.as_deref().unwrap_or_default()
        )));
    }
    let matching: Vec<_> = cli
        .agent
        .as_ref()
        .map(|agent| {
            document
                .profiles
                .iter()
                .filter(|p| &p.agent == agent)
                .collect()
        })
        .unwrap_or_default();
    if cli.agent.is_some() && cli.profile.is_none() && matching.len() > 1 {
        return Err(ConsoleError::Profile(
            "multiple profiles for --agent; select --profile".into(),
        ));
    }
    let initial = chosen
        .and_then(|name| document.profiles.iter().find(|p| &p.name == name))
        .or_else(|| {
            if cli.profile.is_none() {
                matching.first().copied()
            } else {
                None
            }
        });
    if cli.subject.is_some() && !document.profiles.is_empty() {
        return Err(ConsoleError::Profile(
            "--subject cannot be combined with authored --profiles; remove --subject or --profiles before opening the picker".into(),
        ));
    }
    if let (Some(agent), Some(profile)) = (&cli.agent, initial)
        && agent != &profile.agent
    {
        return Err(ConsoleError::Profile(format!(
            "--agent {agent} conflicts with profile {} bound to {}",
            profile.name, profile.agent
        )));
    }
    if cli.subject.is_none() && initial.is_none() && cli.agent.is_none() {
        return Err(ConsoleError::NoSubject);
    }
    let catalog = load_discovered(cli.config.clone())
        .map_err(|error| ConsoleError::Config(Box::new(error)))?;
    if let Some(agent) = &cli.agent
        && catalog.agent(agent).is_none()
    {
        return Err(ConsoleError::UnknownAgent(agent.clone()));
    }
    for profile in &document.profiles {
        if catalog.agent(&profile.agent).is_none() {
            return Err(ConsoleError::UnknownAgent(profile.agent.clone()));
        }
    }
    let subject = initial
        .map(|p| p.subject.clone())
        .or_else(|| cli.subject.clone())
        .ok_or(ConsoleError::NoSubject)?;
    validate_terminal(cli, io::stdin().is_terminal(), io::stdout().is_terminal())?;
    if cli.structured && cli.agent.is_none() && cli.profile.is_none() {
        return Err(ConsoleError::StructuredAgent);
    }
    let agents = catalog.agents().cloned().collect();

    let (model, model_source) = dekopon_tui::session::selected_model(
        cli.model.as_deref(),
        initial.and_then(|p| p.model.as_deref()),
    );
    let mut options = ConsoleOptions::new(subject.clone(), model);
    options.model_source = model_source;
    options.scope = cli
        .smoke_conversation
        .then(dekopon_tui::session::smoke_claim);
    options.smoke_conversation = cli.smoke_conversation;
    options.profiles = document.profiles.clone();
    options.skills = catalog
        .agents()
        .filter_map(|agent| {
            let id = agent.metadata.name.parse::<AgentId>().ok()?;
            Some((id.clone(), catalog.agent_skills(&id).to_vec()))
        })
        .collect();
    options.fixed_profile = cli.profile.clone();
    options.initial_agent = cli.agent.clone().or_else(|| {
        cli.profile
            .as_ref()
            .and_then(|_| initial.map(|p| p.agent.clone()))
    });
    options.model_override = cli.model.clone();
    options.catalog = cli.config.clone();
    options.socket = cli.socket.clone();
    options.server_uid = cli.server_uid;
    options.prompt_limits.max_steps = cli
        .max_steps
        .or_else(|| initial.and_then(|p| p.max_steps))
        .unwrap_or(8);
    options.prompt_limits.max_capability_calls = cli
        .max_capability_calls
        .or_else(|| initial.and_then(|p| p.max_capability_calls))
        .unwrap_or(16);
    options.steps_override = cli.max_steps;
    options.calls_override = cli.max_capability_calls;
    options.model_choice = match &cli.endpoint {
        Some(endpoint) => ModelChoice::OpenAiCompatible {
            endpoint: endpoint.clone(),
            api_key_env: cli.api_key_env.clone(),
        },
        None => ModelChoice::ChatGptSubscription {
            auth_file: cli.auth_file.clone(),
        },
    };
    let credential = match &options.model_choice {
        ModelChoice::ChatGptSubscription { auth_file } => {
            resolve_console_credential(auth_file.as_deref())?
                .display()
                .to_string()
        }
        ModelChoice::OpenAiCompatible { endpoint, .. } => endpoint.clone(),
    };

    // The runtime comes up before the screen does, because connecting proves a broker is actually
    // answering and that refusal has to reach a plain terminal rather than a frame the operator
    // then has to quit out of.
    let (client, socket) = runtime.block_on(connect(&options))?;
    tracing::debug!(
        component = "dekopon-console",
        socket_tier = socket.tier().label(),
        "opening the console",
    );

    let mut app = App::new(
        agents,
        subject.to_string(),
        socket.path().display().to_string(),
        credential,
    );
    if let Some(agent) = &options.initial_agent {
        app.selected_agent = app
            .agents
            .iter()
            .position(|candidate| candidate.metadata.name == agent.as_str())
            .expect("validated catalog agent");
    }
    app.profile = initial.map(|p| p.name.clone());
    app.scope_label = options
        .scope
        .as_ref()
        .map(|s| serde_json::to_string(s).expect("typed scope serializes"))
        .unwrap_or_else(|| "subject-only".into());
    app.model = options.model.clone();
    app.model_source = options.model_source;
    if cli.structured {
        runtime.block_on(dekopon_tui::structured::run(app, client, options))?;
    } else {
        runtime.block_on(dekopon_tui::run(app, client, options))?;
    }
    Ok(())
}

fn validate_terminal(cli: &Cli, stdin_tty: bool, stdout_tty: bool) -> Result<(), ConsoleError> {
    if cli.structured {
        if stdin_tty || stdout_tty {
            return Err(ConsoleError::StructuredTerminal);
        }
    } else if !stdin_tty || !stdout_tty {
        return Err(ConsoleError::NoTerminal);
    }
    Ok(())
}

#[cfg(test)]
mod mode_tests {
    use super::*;
    #[test]
    fn blocking_worker_cannot_hold_structured_process_open() {
        use std::time::{Duration, Instant};
        const CHILD: &str = "DEKOPON_SHUTDOWN_TIMEOUT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (started, ready) = tokio::sync::oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    assert!(started.send(()).is_ok());
                    std::thread::sleep(Duration::from_secs(2)); // deliberately ignores cancel
                });
                ready.await.unwrap();
            });
            // The same shutdown path run_console invokes after flushing END and telemetry.
            shutdown_console_runtime(runtime, true, Duration::from_millis(20));
            return;
        }
        let start = Instant::now();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mode_tests::blocking_worker_cannot_hold_structured_process_open",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            start.elapsed() < Duration::from_millis(700),
            "blocking worker held process open: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn all_tty_combinations_are_explicit() {
        let default = Cli::parse_from(["console"]);
        let structured = Cli::parse_from(["console", "--structured"]);
        for (stdin, stdout) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(
                validate_terminal(&default, stdin, stdout).is_ok(),
                stdin && stdout
            );
            assert_eq!(
                validate_terminal(&structured, stdin, stdout).is_ok(),
                !stdin && !stdout
            );
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ProfilesDocument {
    default_profile: Option<String>,
    profiles: Vec<OperatorProfile>,
}

fn read_profiles(path: &Path) -> Result<ProfilesDocument, ConsoleError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| ConsoleError::Profile(format!("{}: {error}", path.display())))?;
    let document: ProfilesDocument = serde_yaml::from_str(&text)
        .map_err(|error| ConsoleError::Profile(format!("{}: {error}", path.display())))?;
    let mut names = HashSet::new();
    for profile in &document.profiles {
        if profile.name.is_empty()
            || !profile
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || !names.insert(profile.name.as_str())
        {
            return Err(ConsoleError::Profile(format!(
                "invalid or duplicate profile name: {}",
                profile.name
            )));
        }
        if profile
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
            || profile.max_steps.is_some_and(|n| !(1..=64).contains(&n))
            || profile
                .max_capability_calls
                .is_some_and(|n| !(1..=256).contains(&n))
        {
            return Err(ConsoleError::Profile(format!(
                "invalid model or bounds in profile {}",
                profile.name
            )));
        }
    }
    if let Some(default) = &document.default_profile
        && !names.contains(default.as_str())
    {
        return Err(ConsoleError::Profile(format!(
            "defaultProfile {default} has no matching profile"
        )));
    }
    Ok(document)
}

fn idle() -> Result<(), ConsoleError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ConsoleError::Runtime)?;
    runtime.block_on(async {
        #[cfg(unix)] {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(ConsoleError::Runtime)?;
            tokio::select! { _ = term.recv() => {}, result = tokio::signal::ctrl_c() => result.map_err(ConsoleError::Runtime)?, }
        }
        Ok(())
    })
}

/// Exporters are created and drained while their runtime is alive, including startup refusals.
fn run_console(cli: &Cli) -> Result<(), ConsoleError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(ConsoleError::Runtime)?;
    let _entered = runtime.enter();
    let telemetry = telemetry::initialize(cli.telemetry.as_deref(), cli.verbose, cli.no_color)?;
    let result = execute(cli, &runtime);
    let shutdown = telemetry
        .shutdown()
        .map_err(|_sensitive_error| telemetry::TelemetryError::Shutdown);
    drop(_entered);
    // A timed-out structured shell may leave unabortable spawn_blocking work behind. The
    // RESULT/END has already been flushed; do not let runtime Drop wait for that worker forever.
    shutdown_console_runtime(runtime, cli.structured, std::time::Duration::from_secs(5));
    result?;
    shutdown?;
    Ok(())
}

fn shutdown_console_runtime(
    runtime: tokio::runtime::Runtime,
    structured: bool,
    grace: std::time::Duration,
) {
    if structured {
        runtime.shutdown_timeout(grace);
    } else {
        drop(runtime);
    }
}
