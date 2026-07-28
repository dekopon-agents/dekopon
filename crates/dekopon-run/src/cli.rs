//! Command-line syntax for `dekopon-run`.

use std::{num::NonZeroU32, path::PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand};
use dekopon_core::CapabilityId;
use dekopon_provider_host::{
    DEFAULT_FUEL, DEFAULT_MAX_INPUT_BYTES, DEFAULT_MAX_MEMORY_BYTES, DEFAULT_MAX_OUTPUT_BYTES,
    DEFAULT_TIMEOUT,
};

/// Immediate-mode Dekopon runner.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "dekopon-run",
    version,
    about = "Run read-only Dekopon providers in bounded WebAssembly components",
    long_about = None,
    propagate_version = true
)]
pub struct Cli {
    /// Write Chrome/Perfetto-compatible tracing JSON to this path.
    #[arg(long, global = true, value_name = "PATH")]
    pub trace: Option<PathBuf>,

    /// Disable ANSI colors in diagnostics.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Increase diagnostics (`-v` for info, `-vv` for debug details).
    #[arg(short = 'v', global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Wasm execution limits shared by all loaded providers.
    #[command(flatten)]
    pub limits: LimitArgs,

    /// Operation to perform.
    #[command(subcommand)]
    pub command: Command,
}

/// Immediate-mode operations.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Load providers, validate their manifests, and print them as JSON.
    Inspect {
        /// Provider components to load.
        #[command(flatten)]
        providers: ProviderArgs,
    },
    /// Invoke one capability directly without contacting a model.
    Invoke {
        /// Provider components to load.
        #[command(flatten)]
        providers: ProviderArgs,

        /// Capability to invoke.
        capability: CapabilityId,

        /// JSON object supplied to the capability.
        #[arg(long, conflicts_with = "input_file", value_name = "JSON")]
        input: Option<String>,

        /// Read the capability input as JSON from a file, or `-` for stdin.
        #[arg(long, conflicts_with = "input", value_name = "PATH")]
        input_file: Option<PathBuf>,

        /// Number of warm invocations after providers have been compiled.
        #[arg(long, default_value = "1", value_name = "COUNT")]
        repeat: NonZeroU32,
    },
    /// Manage Dekopon's isolated ChatGPT/Codex subscription login.
    Chatgpt {
        /// Authentication operation.
        #[command(subcommand)]
        command: ChatGptCommand,
    },
    /// Run a one-shot model prompt/tool loop.
    Prompt {
        /// Provider components whose capabilities become model tools.
        #[command(flatten)]
        providers: ProviderArgs,

        /// Model identifier sent to the selected model backend.
        #[arg(long, value_name = "MODEL")]
        model: String,

        /// Use Dekopon's ChatGPT/Codex subscription login instead of an API endpoint.
        #[arg(long)]
        chatgpt_subscription: bool,

        /// Override Dekopon's ChatGPT credential file.
        #[arg(long, requires = "chatgpt_subscription", value_name = "PATH")]
        chatgpt_auth_file: Option<PathBuf>,

        /// OpenAI-compatible API base URL; ignored with `--chatgpt-subscription`.
        #[arg(long, default_value = "http://127.0.0.1:11434/v1", value_name = "URL")]
        endpoint: String,

        /// Environment variable containing an optional bearer token.
        #[arg(long, default_value = "OPENAI_API_KEY", value_name = "NAME")]
        api_key_env: String,

        /// Optional system instruction prepended to the conversation.
        #[arg(long, value_name = "TEXT")]
        system: Option<String>,

        /// Maximum model turns, including the final answer.
        #[arg(long, default_value = "8", value_name = "COUNT")]
        max_steps: NonZeroU32,

        /// Timeout for each model HTTP request.
        #[arg(long, default_value = "120000", value_name = "MILLISECONDS")]
        model_timeout_ms: u64,

        /// User prompt.
        #[arg(value_name = "PROMPT")]
        prompt: String,
    },
}

/// ChatGPT/Codex subscription authentication operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ChatGptCommand {
    /// Sign in through OpenAI's Codex device authorization flow.
    Login {
        /// Override Dekopon's ChatGPT credential file.
        #[arg(long, value_name = "PATH")]
        auth_file: Option<PathBuf>,
    },
    /// Report whether Dekopon has a usable ChatGPT login.
    Status {
        /// Override Dekopon's ChatGPT credential file.
        #[arg(long, value_name = "PATH")]
        auth_file: Option<PathBuf>,
    },
    /// Delete Dekopon's ChatGPT login without touching other clients.
    Logout {
        /// Override Dekopon's ChatGPT credential file.
        #[arg(long, value_name = "PATH")]
        auth_file: Option<PathBuf>,
    },
}

/// Repeatable provider component arguments.
#[derive(Clone, Debug, Args)]
pub struct ProviderArgs {
    /// Wasm component implementing the Dekopon provider world; repeat for multiple providers.
    #[arg(long, required = true, action = ArgAction::Append, value_name = "COMPONENT")]
    pub provider: Vec<PathBuf>,
}

/// Bounded Wasmtime store settings.
#[derive(Clone, Debug, Args)]
pub struct LimitArgs {
    /// Maximum linear memory per provider call.
    #[arg(
        long,
        global = true,
        default_value_t = DEFAULT_MAX_MEMORY_BYTES,
        value_name = "BYTES"
    )]
    pub max_memory_bytes: usize,

    /// Maximum serialized invocation input.
    #[arg(
        long,
        global = true,
        default_value_t = DEFAULT_MAX_INPUT_BYTES,
        value_name = "BYTES"
    )]
    pub max_input_bytes: usize,

    /// Maximum serialized provider manifest or output.
    #[arg(
        long,
        global = true,
        default_value_t = DEFAULT_MAX_OUTPUT_BYTES,
        value_name = "BYTES"
    )]
    pub max_output_bytes: usize,

    /// Wasm instruction fuel supplied to each fresh store.
    #[arg(long, global = true, default_value_t = DEFAULT_FUEL, value_name = "UNITS")]
    pub fuel: u64,

    /// Wall-clock limit for each provider instantiation and call.
    #[arg(
        long,
        global = true,
        default_value_t = DEFAULT_TIMEOUT.as_millis() as u64,
        value_name = "MILLISECONDS"
    )]
    pub timeout_ms: u64,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{ChatGptCommand, Cli, Command};

    #[test]
    fn clap_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_multiple_provider_components() {
        let cli = Cli::try_parse_from([
            "dekopon-run",
            "invoke",
            "--provider",
            "echo.wasm",
            "--provider",
            "clock.wasm",
            "echo.echo",
            "--input",
            "{}",
        ])
        .expect("valid command line");

        let Command::Invoke { providers, .. } = cli.command else {
            panic!("expected invoke command");
        };
        assert_eq!(providers.provider.len(), 2);
    }

    #[test]
    fn parses_chatgpt_subscription_prompt_and_login() {
        let cli = Cli::try_parse_from([
            "dekopon-run",
            "prompt",
            "--provider",
            "echo.wasm",
            "--chatgpt-subscription",
            "--model",
            "gpt-test",
            "echo hello",
        ])
        .expect("valid subscription prompt");
        assert!(matches!(
            cli.command,
            Command::Prompt {
                chatgpt_subscription: true,
                ..
            }
        ));

        let cli =
            Cli::try_parse_from(["dekopon-run", "chatgpt", "login"]).expect("valid login command");
        assert!(matches!(
            cli.command,
            Command::Chatgpt {
                command: ChatGptCommand::Login { .. }
            }
        ));
    }

    #[test]
    fn rejects_conflicting_input_sources() {
        let error = Cli::try_parse_from([
            "dekopon-run",
            "invoke",
            "--provider",
            "echo.wasm",
            "echo.echo",
            "--input",
            "{}",
            "--input-file",
            "input.json",
        ])
        .expect_err("input sources conflict");

        assert_eq!(error.exit_code(), 2);
    }
}
