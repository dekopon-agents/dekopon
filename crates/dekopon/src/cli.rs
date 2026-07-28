//! Command-line syntax, kept separate from execution.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use dekopon_core::{AgentId, CapabilityId, ProviderId};

/// Dekopon operator command line.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "dekopon",
    version,
    about = "Capability-oriented control plane for self-hosted AI agents",
    long_about = None,
    propagate_version = true
)]
pub struct Cli {
    /// Path to a YAML or JSON catalog; unused by version and auth commands.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Output format.
    #[arg(
        short = 'o',
        long = "output",
        global = true,
        value_enum,
        default_value_t = OutputFormat::Table,
        value_name = "FORMAT"
    )]
    pub output: OutputFormat,

    /// Disable ANSI colors in diagnostics.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Suppress successful command output.
    #[arg(long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Increase diagnostics (`-v` for info, `-vv` for debug details).
    #[arg(
        short = 'v',
        global = true,
        action = ArgAction::Count,
        conflicts_with = "quiet"
    )]
    pub verbose: u8,

    /// Operation to perform.
    #[command(subcommand)]
    pub command: Command,
}

/// Supported top-level operations.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Print CLI version information.
    Version,
    /// Manage model-account authentication.
    Auth {
        /// Model account to manage.
        #[command(subcommand)]
        account: AuthCommand,
    },
    /// Get one resource or list resources.
    Get {
        /// Resource selector.
        #[command(subcommand)]
        resource: GetCommand,
    },
    /// Show detailed resource information.
    Describe {
        /// Resource selector.
        #[command(subcommand)]
        resource: DescribeCommand,
    },
    /// Parse and validate the resolved local catalog.
    Validate,
    /// Inspect resolved configuration.
    Config {
        /// Configuration operation.
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

/// Model-account authentication namespaces.
#[derive(Clone, Debug, Subcommand)]
pub enum AuthCommand {
    /// Manage Dekopon's isolated ChatGPT/Codex subscription login.
    #[command(name = "chatgpt")]
    ChatGpt {
        /// Authentication operation.
        #[command(subcommand)]
        command: ChatGptAuthCommand,
    },
}

/// ChatGPT/Codex subscription authentication operations.
#[derive(Clone, Debug, Subcommand)]
pub enum ChatGptAuthCommand {
    /// Sign in through OpenAI's Codex device authorization flow.
    Login {
        /// Override Dekopon's ChatGPT credential file.
        #[arg(long, value_name = "PATH")]
        auth_file: Option<PathBuf>,
    },
    /// Report whether Dekopon has a ChatGPT login.
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

/// Resource selectors accepted by `get`.
#[derive(Clone, Debug, Subcommand)]
pub enum GetCommand {
    /// Get one agent.
    Agent {
        /// Agent name.
        name: AgentId,
    },
    /// List agents.
    Agents,
    /// Get one capability.
    Capability {
        /// Capability name.
        name: CapabilityId,
    },
    /// List capabilities.
    Capabilities,
    /// Get one provider.
    Provider {
        /// Provider name.
        name: ProviderId,
    },
    /// List providers.
    Providers,
}

/// Resource selectors accepted by `describe`.
#[derive(Clone, Debug, Subcommand)]
pub enum DescribeCommand {
    /// Describe one agent and its declared authority references.
    Agent {
        /// Agent name.
        name: AgentId,
    },
}

/// Configuration subcommands.
#[derive(Clone, Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the canonical, validated local catalog.
    View,
}

/// Stable output formats supported by resource commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OutputFormat {
    /// Compact human-readable table.
    Table,
    /// Human-readable table with additional fields.
    Wide,
    /// Pretty-printed JSON.
    Json,
    /// YAML.
    Yaml,
    /// Qualified resource names only.
    Name,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{AuthCommand, ChatGptAuthCommand, Cli, Command, GetCommand, OutputFormat};

    #[test]
    fn clap_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_chatgpt_auth_commands() {
        let cli = Cli::try_parse_from([
            "dekopon",
            "auth",
            "chatgpt",
            "status",
            "--auth-file",
            "auth.json",
        ])
        .expect("valid auth command");

        assert!(matches!(
            cli.command,
            Command::Auth {
                account: AuthCommand::ChatGpt {
                    command: ChatGptAuthCommand::Status { .. }
                }
            }
        ));
    }

    #[test]
    fn parses_global_output_after_nested_command() {
        let cli = Cli::try_parse_from(["dekopon", "get", "agents", "-o", "wide"])
            .expect("valid command line");

        assert_eq!(cli.output, OutputFormat::Wide);
        assert!(matches!(
            cli.command,
            Command::Get {
                resource: GetCommand::Agents
            }
        ));
    }
}
