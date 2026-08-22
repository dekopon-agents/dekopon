//! Launching the interactive console.
//!
//! This is the one command in this binary that contacts another process. It stays here rather than
//! in `dekopon-tui` because the decision it makes is a CLI decision: whether this invocation is a
//! person at a terminal or a script reading a pipe.

use std::io::{self, IsTerminal as _};

use dekopon_config::load_discovered;
use dekopon_tui::{
    App, ConsoleOptions, ModelChoice,
    session::{TRACE_PREFIX, connect, resolve_console_credential},
};
use thiserror::Error;

use crate::cli::{Cli, ConsoleArgs};

/// Failure opening or running the console.
#[derive(Debug, Error)]
pub enum ConsoleError {
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
    /// No subject was supplied.
    #[error(
        "no console subject: pass --subject <SUBJECT> or set DEKOPON_CONSOLE_SUBJECT, for example \
         tel.15550100000. The broker must also hold an attestor grant covering that namespace and \
         a mapping resolving it to a principal"
    )]
    NoSubject,
}

/// Whether a bare `dekopon` should open the console.
///
/// Both halves are required. Standard output being a terminal is what makes drawing meaningful;
/// standard input being one is what makes the console able to read a key. A process with either
/// redirected asked for a command, not a screen.
#[must_use]
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Opens the console.
///
/// # Errors
///
/// Returns before drawing anything when the catalog will not load, no broker is listening, or the
/// credential guard refuses. Once the console is drawing, only a terminal failure comes back here:
/// a refused hop or a failed session is something the console shows and stays open after.
pub fn execute(cli: &Cli, args: &ConsoleArgs) -> Result<(), ConsoleError> {
    // Command-line facts first, filesystem facts second. A missing subject is not something an
    // operator fixes by finding a catalog, so reporting the catalog's absence ahead of it would
    // send them to the wrong problem.
    let subject = args.subject.clone().ok_or(ConsoleError::NoSubject)?;
    let catalog = load_discovered(cli.config.clone())
        .map_err(|error| ConsoleError::Config(Box::new(error)))?;
    let agents = catalog.agents().cloned().collect();

    let mut options = ConsoleOptions::new(subject.clone(), args.model.clone());
    options.catalog = cli.config.clone();
    options.socket = args.socket.clone();
    options.server_uid = args.server_uid;
    options.prompt_limits.max_steps = args.max_steps;
    options.prompt_limits.max_capability_calls = args.max_capability_calls;
    options.model_choice = match &args.endpoint {
        Some(endpoint) => ModelChoice::OpenAiCompatible {
            endpoint: endpoint.clone(),
            api_key_env: args.api_key_env.clone(),
        },
        None => ModelChoice::ChatGptSubscription {
            auth_file: args.auth_file.clone(),
        },
    };

    // Resolved before the screen opens, so the refusal an operator has to act on arrives as a line
    // on their terminal rather than inside a full-screen frame they then have to quit out of.
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(ConsoleError::Runtime)?;

    let (client, socket) = runtime.block_on(connect(&options))?;
    tracing::debug!(
        trace_prefix = TRACE_PREFIX,
        socket_tier = socket.tier().label(),
        "opening the console",
    );

    let app = App::new(
        agents,
        subject.to_string(),
        socket.path().display().to_string(),
        credential,
    );
    runtime.block_on(dekopon_tui::run(app, client, options))?;
    Ok(())
}
