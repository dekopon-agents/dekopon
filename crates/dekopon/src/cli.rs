//! Command-line syntax, kept separate from execution.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use dekopon_core::{AgentId, CapabilityId, ExternalSubject, ProviderId};

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
    ///
    /// Optional so that a bare `dekopon` on a terminal opens the console. Absent on anything that
    /// is not a terminal, it stays the usage error it has always been: a piped `dekopon` that
    /// opened a full-screen console would hang a script forever waiting on raw-mode input.
    #[command(subcommand)]
    pub command: Option<Command>,
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
    /// Open the interactive console against a running local broker.
    #[cfg(unix)]
    Console(ConsoleArgs),
}

/// Connection, identity, and model settings for the interactive console.
///
/// Every one of these has a resolved default, so `dekopon console` with no flags is the ordinary
/// invocation; each flag exists for the deployment that is not the local single-UID one.
#[cfg(unix)]
#[derive(Clone, Debug, Args, Default)]
pub struct ConsoleArgs {
    /// Broker socket path.
    ///
    /// Resolves as `dekopon-run` documents: this flag, then `$DEKOPON_BROKER_SOCKET`, then
    /// `$XDG_RUNTIME_DIR/dekopon/broker.sock`, then `$HOME/.local/run/dekopon/broker.sock`.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Trusted UID owning the broker process; defaults to the caller's own.
    #[arg(long, value_name = "UID")]
    pub server_uid: Option<u32>,

    /// Canonical external subject sessions propose on behalf of.
    ///
    /// The broker still has to hold an attestor grant covering its namespace and a mapping
    /// resolving it to a principal; declaring one here grants nothing at all.
    ///
    /// Optional only so that a bare `dekopon` can take it from the environment. There is no
    /// default: an identity the console guessed would be an identity nobody chose, and the broker
    /// would refuse it anyway one step later having told you nothing useful.
    #[arg(long, value_name = "SUBJECT", env = "DEKOPON_CONSOLE_SUBJECT")]
    pub subject: Option<ExternalSubject>,

    /// Model name handed to the backend.
    #[arg(long, value_name = "MODEL", default_value = ConsoleArgs::DEFAULT_MODEL)]
    pub model: String,

    /// ChatGPT credential file.
    ///
    /// Defaults to the console's own `chatgpt-auth.console.json` rather than the file every other
    /// surface resolves to, because the refresh token rotates and sharing it would invalidate the
    /// gateway's copy. Passing this explicitly accepts whatever it points at.
    #[arg(long, value_name = "PATH", conflicts_with = "endpoint")]
    pub auth_file: Option<PathBuf>,

    /// OpenAI-compatible endpoint to use instead of the ChatGPT subscription.
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,

    /// Name of the environment variable holding the endpoint's bearer token.
    #[arg(long, value_name = "NAME", requires = "endpoint")]
    pub api_key_env: Option<String>,

    /// Maximum model turns one session may take.
    #[arg(long, value_name = "COUNT", default_value_t = 8)]
    pub max_steps: u32,

    /// Capability invocations one session may drive in total.
    #[arg(long, value_name = "COUNT", default_value_t = 16)]
    pub max_capability_calls: u32,
}

#[cfg(unix)]
impl ConsoleArgs {
    /// Model a console session talks to when nothing names one.
    pub const DEFAULT_MODEL: &'static str = "gpt-5.6-luna";

    /// The settings a bare `dekopon` opens with.
    ///
    /// Everything but the subject has a default; the subject comes from `DEKOPON_CONSOLE_SUBJECT`
    /// or not at all, and `console::execute` reports its absence by naming both ways to supply it.
    #[must_use]
    pub fn interactive_default() -> Self {
        Self {
            socket: None,
            server_uid: None,
            subject: std::env::var("DEKOPON_CONSOLE_SUBJECT")
                .ok()
                .filter(|value| !value.is_empty())
                .and_then(|value| value.parse().ok()),
            model: Self::DEFAULT_MODEL.to_owned(),
            auth_file: None,
            endpoint: None,
            api_key_env: None,
            max_steps: 8,
            max_capability_calls: 16,
        }
    }
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
    /// Print the stored ChatGPT credential so it can be seeded into a secret store.
    ///
    /// This command prints real credential material in the clear: the live access token and the
    /// rotating refresh token. Every other Dekopon surface renders a redaction marker instead. It
    /// exists because device authorization needs a human at a browser, so a pod can only ever run
    /// on a credential an operator carried out of a local login.
    ///
    /// The refresh token rotates. This copy is stale the moment the credential it came from
    /// refreshes, so seed it once into a writable directory, never overwrite a newer credential
    /// file with it, and re-export after a deliberate rotation.
    ///
    /// Standard output is refused when it is a terminal, and `--output` does not apply: the form
    /// is chosen by `--format`.
    Export {
        /// Override Dekopon's ChatGPT credential file.
        #[arg(long, value_name = "PATH")]
        auth_file: Option<PathBuf>,

        /// Form to print the credential in.
        #[arg(
            long,
            value_enum,
            default_value_t = ExportFormat::Secret,
            value_name = "FORM"
        )]
        format: ExportFormat,

        /// Name of the emitted Kubernetes Secret; ignored by `--format raw`.
        #[arg(
            long,
            value_name = "NAME",
            default_value = "dekopon-chatgpt-auth",
            value_parser = dns_subdomain
        )]
        secret_name: String,

        /// Namespace of the emitted Kubernetes Secret; omitted from the manifest when unset.
        #[arg(long, value_name = "NAMESPACE", value_parser = dns_label)]
        namespace: Option<String>,

        /// Acknowledge that this prints a live access token and refresh token in the clear.
        ///
        /// Conflicts with `--quiet`, which would otherwise suppress the whole document and exit
        /// zero — a scripted seeding step would store nothing and believe it had succeeded.
        #[arg(long, required = true, conflicts_with = "quiet")]
        expose_credential: bool,

        /// Print to a terminal anyway, accepting the credential in your scrollback.
        #[arg(long)]
        allow_terminal: bool,
    },
}

/// Forms `dekopon auth chatgpt export` can print a credential in.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ExportFormat {
    /// A `v1` `Secret` manifest, for `kubectl apply -f -`.
    Secret,
    /// The credential document itself, for a password-manager field.
    Raw,
}

/// Validates a Kubernetes object name as an RFC 1123 DNS subdomain.
///
/// The API server would reject a bad name anyway, but only after the manifest — and the credential
/// inside it — had already been printed and piped somewhere.
fn dns_subdomain(value: &str) -> Result<String, String> {
    dns_name(value, 253, true)
}

/// Validates a namespace as an RFC 1123 DNS label, which is a subdomain without dots.
fn dns_label(value: &str) -> Result<String, String> {
    dns_name(value, 63, false)
}

fn dns_name(value: &str, limit: usize, dots: bool) -> Result<String, String> {
    let shape = if dots {
        "lowercase letters, digits, '-' and '.'"
    } else {
        "lowercase letters, digits and '-'"
    };
    let requirement = format!(
        "must be at most {limit} characters of {shape}, starting and ending with a letter or digit"
    );

    if value.is_empty() || value.len() > limit {
        return Err(requirement);
    }
    let alphanumeric =
        |character: char| character.is_ascii_lowercase() || character.is_ascii_digit();
    let label = |label: &str| {
        !label.is_empty()
            && label.starts_with(alphanumeric)
            && label.ends_with(alphanumeric)
            && label
                .chars()
                .all(|character| alphanumeric(character) || character == '-')
    };
    // A subdomain is validated per dot-separated label, not as one string: the API server's
    // regex applies to each label, so `a.-b.c` is rejected there and must be rejected here,
    // before the credential this name labels has been printed.
    let valid = if dots {
        value.split('.').all(label)
    } else {
        label(value)
    };
    if !valid {
        return Err(requirement);
    }

    Ok(value.to_owned())
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

    use super::{
        AuthCommand, ChatGptAuthCommand, Cli, Command, ExportFormat, GetCommand, OutputFormat,
        dns_label, dns_subdomain,
    };

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
            Some(Command::Auth {
                account: AuthCommand::ChatGpt {
                    command: ChatGptAuthCommand::Status { .. }
                }
            })
        ));
    }

    #[test]
    fn export_requires_the_credential_acknowledgement() {
        let refused = Cli::try_parse_from(["dekopon", "auth", "chatgpt", "export"])
            .expect_err("export without acknowledgement must not parse");

        assert!(refused.to_string().contains("--expose-credential"));

        let cli = Cli::try_parse_from([
            "dekopon",
            "auth",
            "chatgpt",
            "export",
            "--expose-credential",
        ])
        .expect("acknowledged export parses");

        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                account: AuthCommand::ChatGpt {
                    command: ChatGptAuthCommand::Export {
                        format: ExportFormat::Secret,
                        expose_credential: true,
                        allow_terminal: false,
                        ..
                    }
                }
            })
        ));
    }

    #[test]
    fn export_rejects_a_secret_name_the_api_server_would_reject() {
        assert!(dns_subdomain("dekopon-chatgpt-auth").is_ok());
        assert!(dns_subdomain("dekopon.chatgpt.auth").is_ok());
        assert!(dns_subdomain("").is_err());
        assert!(dns_subdomain("-leading").is_err());
        assert!(dns_subdomain("trailing-").is_err());
        assert!(dns_subdomain("Upper").is_err());
        assert!(dns_subdomain("double..dot").is_err());
        assert!(dns_subdomain(&"a".repeat(254)).is_err());

        // A namespace is a label, so dots are not a valid separator there.
        assert!(dns_label("dekopon").is_ok());
        assert!(dns_label("dekopon.agents").is_err());
        assert!(dns_label(&"a".repeat(64)).is_err());
    }

    #[test]
    fn parses_global_output_after_nested_command() {
        let cli = Cli::try_parse_from(["dekopon", "get", "agents", "-o", "wide"])
            .expect("valid command line");

        assert_eq!(cli.output, OutputFormat::Wide);
        assert!(matches!(
            cli.command,
            Some(Command::Get {
                resource: GetCommand::Agents
            })
        ));
    }
}
