//! Command-line syntax for `dekopon-run`.

use std::{io, num::NonZeroU32, path::PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand, builder::NonEmptyStringValueParser};
use dekopon_broker_protocol::{DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES};
use dekopon_core::{
    CapabilityId, ExternalSubject, InvocationId, PROVIDER_COMPONENT_EXTENSION, TraceId,
};
use dekopon_provider_host::{
    DEFAULT_FUEL, DEFAULT_MAX_INPUT_BYTES, DEFAULT_MAX_MEMORY_BYTES, DEFAULT_MAX_OUTPUT_BYTES,
    DEFAULT_TIMEOUT, HostOptions,
};
use dekopon_shell::{
    DEFAULT_MAX_CAPABILITY_CALLS, DEFAULT_MAX_OUTPUT_BYTES as DEFAULT_SHELL_MAX_OUTPUT_BYTES,
    DEFAULT_MAX_OUTPUT_LINES, DEFAULT_MAX_RECURSION_DEPTH, DEFAULT_MAX_STEPS,
    DEFAULT_MAX_VALUE_BYTES, DEFAULT_TIMEOUT as DEFAULT_SHELL_TIMEOUT,
};
pub use dekopon_telemetry::Transport;
use thiserror::Error;

/// Immediate-mode Dekopon runner.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "dekopon-run",
    version,
    about = "Run import-free providers directly or call a separate Dekopon broker",
    long_about = None,
    propagate_version = true
)]
pub struct Cli {
    /// Write Chrome/Perfetto-compatible tracing JSON to this path.
    #[arg(long, global = true, value_name = "PATH")]
    pub trace: Option<PathBuf>,

    /// OpenTelemetry export settings.
    #[command(flatten)]
    pub telemetry: TelemetryArgs,

    /// Disable ANSI colors in diagnostics.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Increase diagnostics (`-v` for info, `-vv` for debug details).
    #[arg(short = 'v', global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Operation to perform.
    #[command(subcommand)]
    pub command: Command,
}

/// Direct, prompt, and broker-client operations.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Load providers, validate their manifests, and print them as JSON.
    Inspect {
        /// Bounded immediate-mode Wasm settings.
        #[command(flatten)]
        limits: LimitArgs,

        /// Provider components to load.
        #[command(flatten)]
        providers: ProviderArgs,
    },
    /// Invoke one capability directly without contacting a model.
    Invoke {
        /// Bounded immediate-mode Wasm settings.
        #[command(flatten)]
        limits: LimitArgs,

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
    /// Run one sandboxed shell script whose commands dispatch to provider capabilities.
    Shell {
        /// Bounded immediate-mode Wasm settings.
        #[command(flatten)]
        limits: LimitArgs,

        /// Provider components whose capabilities the script may invoke.
        #[command(flatten)]
        providers: ProviderArgs,

        /// Bounded interpreter settings.
        #[command(flatten)]
        shell: ShellLimitArgs,

        /// Direct-mode capability the `curl` builtin assembles requests for.
        ///
        /// Absent means `curl` reports "command not found". Typed as a capability identifier so a
        /// malformed value is a usage error here rather than a "capability not found" at runtime,
        /// which would tell an operator the wrong thing about what went wrong.
        #[arg(long, value_name = "CAPABILITY")]
        curl_capability: Option<CapabilityId>,

        /// Script source.
        #[arg(value_name = "SCRIPT")]
        script: String,
    },
    /// Use the unprivileged client for a separately running authenticated broker.
    Broker {
        /// Broker operation.
        #[command(subcommand)]
        command: BrokerCommand,
    },
    /// Run a one-shot model prompt/tool loop over one sandboxed scripting tool.
    Prompt {
        /// Bounded immediate-mode Wasm settings.
        #[command(flatten)]
        limits: LimitArgs,

        /// Provider components whose capabilities the model's scripts may invoke directly.
        #[command(flatten)]
        providers: ProviderArgs,

        /// Bounded interpreter settings for the scripts the model writes.
        #[command(flatten)]
        shell: ShellLimitArgs,

        /// Also reach a running broker for capabilities no loaded provider offers.
        ///
        /// Off by default, so prompt mode stays exactly as capable as direct mode with no daemon
        /// running. Turning it on is what makes an HTTP-capable capability reachable at all: the
        /// direct-mode Wasm linker is import-free by construction and cannot perform I/O.
        #[arg(long)]
        broker: bool,

        /// Authenticated local broker connection settings; used only with `--broker`.
        #[command(flatten)]
        connection: BrokerConnectionArgs,

        /// Capability the `curl` builtin assembles requests for.
        ///
        /// Typically reachable only over `--broker`, since direct mode cannot speak HTTP.
        #[arg(long, value_name = "CAPABILITY")]
        curl_capability: Option<CapabilityId>,

        /// Model identifier sent to the selected model backend.
        #[arg(long, value_name = "MODEL")]
        model: String,

        /// Use the ChatGPT/Codex login managed by `dekopon auth chatgpt`.
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
    /// Hold a conversation with a running `dekopond` over its local development transport.
    ///
    /// This runs no model or tool loop of its own and loads no provider: it is a client for a
    /// gateway that already does all of that. Each line of standard input is sent as one request
    /// and its single reply is printed.
    Chat {
        /// Owner-only Unix socket the gateway's `local` transport listens on.
        #[arg(long, value_name = "PATH")]
        gateway: PathBuf,

        /// Canonical external subject this session claims, such as `tel.16034700182`.
        ///
        /// Typed so a malformed subject is a usage error here rather than a line the gateway
        /// discards without answering, which would look like an unresponsive daemon.
        #[arg(long, value_name = "SUBJECT")]
        subject: ExternalSubject,

        /// Conversation identity carried as the `channel` of every request.
        ///
        /// Minted and announced on standard error when omitted, so a session is resumable by
        /// passing the same value back.
        #[arg(
            long,
            value_name = "ID",
            value_parser = NonEmptyStringValueParser::new()
        )]
        conversation: Option<String>,
    },
}

/// Unprivileged broker operations.
#[derive(Clone, Debug, Subcommand)]
pub enum BrokerCommand {
    /// List capabilities exact policy exposes to this authenticated Unix peer.
    Capabilities {
        /// Authenticated Unix connection settings.
        #[command(flatten)]
        connection: BrokerConnectionArgs,
    },
    /// Submit one untrusted invocation proposal to the broker.
    Invoke {
        /// Authenticated Unix connection settings.
        #[command(flatten)]
        connection: BrokerConnectionArgs,

        /// Capability to propose.
        capability: CapabilityId,

        /// Caller-generated unique invocation identifier used for durable replay rejection.
        #[arg(long, value_name = "ID")]
        invocation_id: InvocationId,

        /// Caller-generated trace correlation identifier.
        #[arg(long, value_name = "ID")]
        trace_id: TraceId,

        /// JSON object supplied to the capability.
        #[arg(long, conflicts_with = "input_file", value_name = "JSON")]
        input: Option<String>,

        /// Read capability input as JSON from a file, or `-` for stdin.
        #[arg(long, conflicts_with = "input", value_name = "PATH")]
        input_file: Option<PathBuf>,
    },
}

/// Authenticated local broker connection settings.
#[derive(Clone, Debug, Args)]
pub struct BrokerConnectionArgs {
    /// Owner-only Unix socket created by `dekopon-brokerd`; defaults to
    /// `$DEKOPON_BROKER_SOCKET`, then `$XDG_RUNTIME_DIR/dekopon/broker.sock`,
    /// then `$HOME/.local/run/dekopon/broker.sock`.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Trusted operating-system UID expected for the broker server process;
    /// defaults to the caller's own effective UID.
    #[arg(long, value_name = "UID")]
    pub server_uid: Option<u32>,

    /// Maximum JSON frame bytes, excluding the four-byte prefix.
    #[arg(long, default_value_t = DEFAULT_MAX_FRAME_BYTES, value_name = "BYTES")]
    pub max_frame_bytes: usize,

    /// Deadline for connect and each complete frame operation.
    #[arg(
        long,
        default_value_t = DEFAULT_IO_TIMEOUT.as_millis() as u64,
        value_name = "MILLISECONDS"
    )]
    pub io_timeout_ms: u64,
}

/// Optional OTLP/HTTP export settings for runner traces and audit-safe logs.
#[derive(Clone, Debug, Args)]
pub struct TelemetryArgs {
    /// Base OTLP/HTTP endpoint. `/v1/traces` and `/v1/logs` are appended.
    /// Export is disabled when this and `OTEL_EXPORTER_OTLP_ENDPOINT` are unset.
    #[arg(
        long,
        global = true,
        env = "OTEL_EXPORTER_OTLP_ENDPOINT",
        value_name = "URL"
    )]
    pub otlp_endpoint: Option<String>,

    /// OTLP wire transport: `grpc` or `http`.
    #[arg(
        long,
        global = true,
        env = "OTEL_EXPORTER_OTLP_PROTOCOL_KIND",
        default_value = "http",
        value_name = "TRANSPORT"
    )]
    pub otlp_transport: Transport,

    /// Include provider payloads and HTTP URLs in span fields.
    ///
    /// Declares the telemetry sink in scope for the data this runner handles. Credentials are
    /// unaffected: redacted values render their marker in either mode.
    #[arg(
        long,
        global = true,
        env = "DEKOPON_OTEL_TELEMETRY_PAYLOADS",
        value_name = "BOOL",
        default_value_t = false,
        action = ArgAction::Set
    )]
    pub otel_telemetry_payloads: bool,

    /// OpenTelemetry service name attached to logs and traces.
    #[arg(
        long,
        global = true,
        env = "OTEL_SERVICE_NAME",
        default_value = "dekopon-run",
        value_name = "NAME"
    )]
    pub otel_service_name: String,

    /// Timeout for each OTLP export and final shutdown flush.
    #[arg(
        long,
        global = true,
        env = "DEKOPON_OTEL_EXPORT_TIMEOUT_MS",
        default_value = "5000",
        value_name = "MILLISECONDS"
    )]
    pub otel_export_timeout_ms: u64,
}

/// Failure to expand a `--provider` argument into component files.
#[derive(Debug, Error)]
#[error("could not read provider directory {}", path.display())]
pub struct ProviderArgsError {
    /// The directory argument that could not be read.
    pub path: PathBuf,
    /// The underlying filesystem failure.
    #[source]
    pub source: io::Error,
}

/// Repeatable provider component arguments and where their compiled code is kept.
#[derive(Clone, Debug, Args)]
pub struct ProviderArgs {
    /// Wasm component or directory of them; repeat for multiple providers.
    #[arg(long, required = true, action = ArgAction::Append, value_name = "COMPONENT")]
    pub provider: Vec<PathBuf>,

    /// Directory holding Wasmtime's compiled-code cache, reused across runs.
    ///
    /// Absent, every process Cranelift-compiles every selected component again, which is the
    /// dominant cost of a short command. The directory holds code this process executes, so name
    /// one only the invoking user can write.
    #[arg(long, env = "DEKOPON_RUN_COMPILE_CACHE", value_name = "DIRECTORY")]
    pub compile_cache: Option<PathBuf>,
}

impl ProviderArgs {
    /// Expands each argument into the component files it names, in load order.
    ///
    /// A file is itself; a directory is every `*.wasm` directly inside it, in filename order. The
    /// selection rule is [`PROVIDER_COMPONENT_EXTENSION`] and the sort is deliberate: the registry
    /// builds its capability route table in load order, so readdir order would make two runs over
    /// one directory disagree about which provider claimed a duplicate capability.
    ///
    /// Unlike the broker, this runner is unprivileged and loads components the invoking user
    /// already owns, so there is no ownership or permission check here. `dekopon-brokerd` applies
    /// those to its own provider directories, where the components run under broker authority.
    ///
    /// # Errors
    ///
    /// Returns any error from reading a directory argument.
    pub fn components(&self) -> Result<Vec<PathBuf>, ProviderArgsError> {
        let mut components = Vec::with_capacity(self.provider.len());
        for entry in &self.provider {
            if !entry.is_dir() {
                components.push(entry.clone());
                continue;
            }
            let read = |source| ProviderArgsError {
                path: entry.clone(),
                source,
            };
            let mut found = Vec::new();
            for candidate in std::fs::read_dir(entry).map_err(read)? {
                let candidate = candidate.map_err(read)?;
                if candidate
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == PROVIDER_COMPONENT_EXTENSION)
                    && candidate.file_type().map_err(read)?.is_file()
                {
                    found.push(candidate.path());
                }
            }
            found.sort();
            components.extend(found);
        }
        Ok(components)
    }

    /// Returns the operational host settings these arguments select.
    #[must_use]
    pub fn host_options(&self) -> HostOptions {
        HostOptions {
            compile_cache_dir: self.compile_cache.clone(),
        }
    }
}

/// Bounded interpreter settings for `shell`.
///
/// These are separate from [`LimitArgs`]: Wasm fuel and memory bound one component call, while
/// these bound the native tree-walking interpreter that decides how many such calls happen.
#[derive(Clone, Debug, Args)]
pub struct ShellLimitArgs {
    /// Statements, loop iterations, and function calls one script may execute.
    #[arg(long, default_value_t = DEFAULT_MAX_STEPS, value_name = "COUNT")]
    pub shell_max_steps: u64,

    /// Maximum nested shell-function calls.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_RECURSION_DEPTH,
        value_name = "DEPTH"
    )]
    pub shell_max_recursion_depth: u32,

    /// Maximum accumulated script output.
    #[arg(
        long,
        default_value_t = DEFAULT_SHELL_MAX_OUTPUT_BYTES,
        value_name = "BYTES"
    )]
    pub shell_max_output_bytes: usize,

    /// Maximum accumulated script output lines.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_OUTPUT_LINES,
        value_name = "LINES"
    )]
    pub shell_max_output_lines: usize,

    /// Wall-clock deadline for the whole script.
    #[arg(
        long,
        default_value_t = DEFAULT_SHELL_TIMEOUT.as_millis() as u64,
        value_name = "MILLISECONDS"
    )]
    pub shell_timeout_ms: u64,

    /// Maximum capability invocations.
    ///
    /// `shell` runs exactly one script, so this bounds that script. `prompt` may run one script
    /// per model turn, so there it bounds the whole session: without that, a model widens its own
    /// budget just by writing more scripts, and the model-turn limit multiplies the ceiling
    /// instead of bounding it.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_CAPABILITY_CALLS,
        value_name = "COUNT"
    )]
    pub shell_max_capability_calls: u32,

    /// Maximum value bytes one script may materialize in variables, buffers, and substitutions.
    ///
    /// This is cumulative across the run rather than a snapshot of what is held, so it is an upper
    /// bound on the interpreter's peak value memory. Without it, doubling a string in a loop
    /// reaches gigabytes in a few hundred steps.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_VALUE_BYTES,
        value_name = "BYTES"
    )]
    pub shell_max_value_bytes: u64,

    /// Let the `date` builtin read the host wall clock.
    ///
    /// Off by default, and the only ambient authority the interpreter has to grant: unlike `curl`,
    /// which is bound to one operator-configured capability, there is no provider to authorize
    /// "what time is it". With this unset, `date` reports "command not found" like any capability
    /// this session was not granted, rather than returning a fabricated time a script would trust.
    #[arg(long)]
    pub shell_allow_clock: bool,
}

/// Bounded Wasmtime store settings.
#[derive(Clone, Debug, Args)]
pub struct LimitArgs {
    /// Maximum linear memory per provider call.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_MEMORY_BYTES,
        value_name = "BYTES"
    )]
    pub max_memory_bytes: usize,

    /// Maximum serialized invocation input.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_INPUT_BYTES,
        value_name = "BYTES"
    )]
    pub max_input_bytes: usize,

    /// Maximum serialized provider manifest or output.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_OUTPUT_BYTES,
        value_name = "BYTES"
    )]
    pub max_output_bytes: usize,

    /// Wasm instruction fuel supplied to each fresh store.
    #[arg(long, default_value_t = DEFAULT_FUEL, value_name = "UNITS")]
    pub fuel: u64,

    /// Wall-clock limit for each provider instantiation and call.
    #[arg(
        long,
        default_value_t = DEFAULT_TIMEOUT.as_millis() as u64,
        value_name = "MILLISECONDS"
    )]
    pub timeout_ms: u64,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::{CommandFactory, Parser};

    use super::{BrokerCommand, Cli, Command, ProviderArgs};

    /// A `--provider` directory expands to the components inside it, in filename order, and a
    /// plain file argument still means itself.
    ///
    /// The runner applies no ownership check where `dekopon-brokerd` does: this loads components
    /// the invoking user already owns, under their own authority.
    #[test]
    fn a_provider_directory_argument_expands_in_filename_order() {
        let directory = tempfile::tempdir().expect("create provider fixture");
        let nested = directory.path().join("bundled");
        std::fs::create_dir(&nested).expect("create bundled directory");
        for name in ["middle.wasm", "alpha.wasm"] {
            std::fs::write(nested.join(name), b"component fixture").expect("write component");
        }
        std::fs::write(nested.join("notes.txt"), b"not a component").expect("write decoy");
        let solo = directory.path().join("solo.wasm");
        std::fs::write(&solo, b"component fixture").expect("write solo component");

        let arguments = ProviderArgs {
            provider: vec![solo.clone(), nested.clone()],
            compile_cache: None,
        };
        assert_eq!(
            arguments.components().expect("expansion succeeds"),
            [solo, nested.join("alpha.wasm"), nested.join("middle.wasm")]
        );
    }

    #[test]
    fn an_unreadable_provider_directory_names_itself() {
        let directory = tempfile::tempdir().expect("create provider fixture");
        let missing = directory.path().join("absent");
        std::fs::create_dir(&missing).expect("create directory");
        std::fs::remove_dir(&missing).expect("remove directory");
        // A path that is neither a file nor a readable directory is passed through as a component
        // path; the registry reports it, not the argument parser.
        let arguments = ProviderArgs {
            provider: vec![missing.clone()],
            compile_cache: None,
        };
        assert_eq!(
            arguments
                .components()
                .expect("a missing path is not expanded"),
            [missing]
        );
    }

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
    fn parses_global_otlp_settings() {
        let cli = Cli::try_parse_from([
            "dekopon-run",
            "--otlp-endpoint",
            "http://openobserve:5080/api/default",
            "--otel-service-name",
            "dekopon-run-test",
            "--otel-export-timeout-ms",
            "2500",
            "inspect",
            "--provider",
            "echo.wasm",
        ])
        .expect("valid telemetry settings");

        assert_eq!(
            cli.telemetry.otlp_endpoint.as_deref(),
            Some("http://openobserve:5080/api/default")
        );
        assert_eq!(cli.telemetry.otel_service_name, "dekopon-run-test");
        assert_eq!(cli.telemetry.otel_export_timeout_ms, 2_500);
    }

    /// Telemetry is opt-in, and every subcommand accepts the same global flags.
    ///
    /// `shell` is the case worth pinning: it arrived after this feature was written, so a
    /// per-subcommand argument group would have silently skipped it.
    #[test]
    fn telemetry_defaults_to_disabled_on_every_subcommand() {
        // Defaults are only observable with the env fallbacks unset, and scrubbing the process
        // environment is off the table (`std::env::remove_var` is unsafe in edition 2024 and this
        // workspace forbids unsafe code), so ambient OpenTelemetry configuration skips the test
        // instead of failing it. CI runs with a clean environment and always pins the defaults.
        if std::env::vars_os().any(|(name, _)| {
            name.to_str()
                .is_some_and(|name| name.starts_with("OTEL_") || name.starts_with("DEKOPON_OTEL_"))
        }) {
            eprintln!("skipping: ambient OTEL_*/DEKOPON_OTEL_* environment overrides the defaults");
            return;
        }
        for command in [
            vec!["inspect", "--provider", "echo.wasm"],
            vec!["shell", "--provider", "echo.wasm", "echo hi"],
            vec![
                "chat",
                "--gateway",
                "/run/dekopon/dekopond-dev.sock",
                "--subject",
                "tel.16034700182",
            ],
        ] {
            let mut arguments = vec!["dekopon-run"];
            arguments.extend(command);
            let cli = Cli::try_parse_from(&arguments).expect("valid command line");

            assert!(cli.telemetry.otlp_endpoint.is_none(), "{arguments:?}");
            assert_eq!(cli.telemetry.otel_service_name, "dekopon-run");
        }
    }

    #[test]
    fn parses_chatgpt_subscription_prompt() {
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
    }

    #[test]
    fn parses_identity_free_broker_invocation() {
        let cli = Cli::try_parse_from([
            "dekopon-run",
            "broker",
            "invoke",
            "--socket",
            "/run/dekopon/broker.sock",
            "--server-uid",
            "1000",
            "--invocation-id",
            "invoke-client-test",
            "--trace-id",
            "trace-client-test",
            "jsonplaceholder.posts.get",
            "--input",
            r#"{"postId":7}"#,
        ])
        .expect("valid broker invocation");
        let Command::Broker {
            command:
                BrokerCommand::Invoke {
                    connection,
                    invocation_id,
                    trace_id,
                    ..
                },
        } = cli.command
        else {
            panic!("expected broker invoke command");
        };
        assert_eq!(
            connection.socket.as_deref(),
            Some(Path::new("/run/dekopon/broker.sock"))
        );
        assert_eq!(connection.server_uid, Some(1000));
        assert_eq!(invocation_id.as_str(), "invoke-client-test");
        assert_eq!(trace_id.as_str(), "trace-client-test");
    }

    #[test]
    fn defaults_broker_socket_and_server_uid_when_omitted() {
        let cli = Cli::try_parse_from(["dekopon-run", "broker", "capabilities"])
            .expect("broker capabilities without connection flags");
        let Command::Broker {
            command: BrokerCommand::Capabilities { connection },
        } = cli.command
        else {
            panic!("expected broker capabilities command");
        };
        assert!(connection.socket.is_none());
        assert!(connection.server_uid.is_none());
    }

    #[test]
    fn rejects_broker_payload_identity_claims() {
        let error = Cli::try_parse_from([
            "dekopon-run",
            "broker",
            "capabilities",
            "--socket",
            "/run/dekopon/broker.sock",
            "--server-uid",
            "1000",
            "--principal",
            "forged",
        ])
        .expect_err("broker client has no payload principal argument");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn parses_a_gateway_chat_session() {
        let cli = Cli::try_parse_from([
            "dekopon-run",
            "chat",
            "--gateway",
            "/run/dekopon/dekopond-dev.sock",
            "--subject",
            "slack.t0123abc.u9xyz",
            "--conversation",
            "morning-standup",
        ])
        .expect("valid chat session");
        let Command::Chat {
            gateway,
            subject,
            conversation,
        } = cli.command
        else {
            panic!("expected chat command");
        };
        assert_eq!(gateway, Path::new("/run/dekopon/dekopond-dev.sock"));
        assert_eq!(subject.canonical(), "slack.t0123abc.u9xyz");
        assert_eq!(conversation.as_deref(), Some("morning-standup"));
    }

    /// A subject the broker could never map is a usage error, not a silently discarded line.
    ///
    /// The transport drops a request it cannot deserialize without answering it, so a raw `String`
    /// here would turn a typo into a session that simply never replies.
    #[test]
    fn rejects_a_subject_that_is_not_canonical() {
        for subject in ["U9XYZ", "slack.T0123ABC", "tel.+16034700182"] {
            let error = Cli::try_parse_from([
                "dekopon-run",
                "chat",
                "--gateway",
                "/run/dekopon/dekopond-dev.sock",
                "--subject",
                subject,
            ])
            .expect_err("subject must be canonical");
            assert_eq!(error.exit_code(), 2, "{subject}");
        }
    }

    /// An empty conversation identifier would silently merge unrelated sessions on the gateway.
    #[test]
    fn rejects_an_empty_conversation_identifier() {
        let error = Cli::try_parse_from([
            "dekopon-run",
            "chat",
            "--gateway",
            "/run/dekopon/dekopond-dev.sock",
            "--subject",
            "tel.16034700182",
            "--conversation",
            "",
        ])
        .expect_err("conversation identifiers must not be empty");
        assert_eq!(error.exit_code(), 2);
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
