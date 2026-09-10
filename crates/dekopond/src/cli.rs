//! Gateway serving configuration and isolated model-account commands.
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Parsed `dekopond` invocation.
#[derive(Debug, Parser)]
#[command(
    name = "dekopond",
    version,
    subcommand_negates_reqs = true,
    about = "Run the unprivileged Dekopon chat gateway and agent daemon"
)]
pub struct Cli {
    /// Strict owner-controlled gateway YAML/JSON configuration; unused by auth.
    #[arg(long, value_name = "PATH", required = true)]
    pub config: Option<PathBuf>,
    /// Operator command; absent means serve the gateway.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Isolated operator operations.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage model-account authentication without starting the gateway.
    Auth(AuthOptions),
}

/// Options scoped to authentication, not daemon logging.
#[derive(Debug, Args)]
pub struct AuthOptions {
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

    /// Model account to manage.
    #[command(subcommand)]
    pub account: AuthCommand,
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

/// Forms `dekopond auth chatgpt export` can print a credential in.
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
        AuthCommand, AuthOptions, ChatGptAuthCommand, Cli, Command, ExportFormat, dns_label,
        dns_subdomain,
    };

    #[test]
    fn clap_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_chatgpt_auth_commands() {
        let cli = Cli::try_parse_from([
            "dekopond",
            "auth",
            "chatgpt",
            "status",
            "--auth-file",
            "auth.json",
        ])
        .expect("valid auth command");

        assert!(matches!(
            cli.command,
            Some(Command::Auth(AuthOptions {
                account: AuthCommand::ChatGpt {
                    command: ChatGptAuthCommand::Status { .. }
                },
                ..
            }))
        ));
    }

    #[test]
    fn export_requires_the_credential_acknowledgement() {
        let refused = Cli::try_parse_from(["dekopond", "auth", "chatgpt", "export"])
            .expect_err("export without acknowledgement must not parse");

        assert!(refused.to_string().contains("--expose-credential"));

        let cli = Cli::try_parse_from([
            "dekopond",
            "auth",
            "chatgpt",
            "export",
            "--expose-credential",
        ])
        .expect("acknowledged export parses");

        assert!(matches!(
            cli.command,
            Some(Command::Auth(AuthOptions {
                account: AuthCommand::ChatGpt {
                    command: ChatGptAuthCommand::Export {
                        format: ExportFormat::Secret,
                        expose_credential: true,
                        allow_terminal: false,
                        ..
                    }
                },
                ..
            }))
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
}
