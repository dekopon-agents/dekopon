//! Command-line syntax for `dekopon-run`.

use std::{num::NonZeroU32, path::PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand};
use dekopon_broker_protocol::{DEFAULT_IO_TIMEOUT, DEFAULT_MAX_FRAME_BYTES};
use dekopon_core::{CapabilityId, InvocationId, TraceId};
use dekopon_provider_host::{
    DEFAULT_FUEL, DEFAULT_MAX_INPUT_BYTES, DEFAULT_MAX_MEMORY_BYTES, DEFAULT_MAX_OUTPUT_BYTES,
    DEFAULT_TIMEOUT,
};

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
    /// Use the unprivileged client for a separately running authenticated broker.
    Broker {
        /// Broker operation.
        #[command(subcommand)]
        command: BrokerCommand,
    },
    /// Run a one-shot model prompt/tool loop.
    Prompt {
        /// Bounded immediate-mode Wasm settings.
        #[command(flatten)]
        limits: LimitArgs,

        /// Provider components whose capabilities become model tools.
        #[command(flatten)]
        providers: ProviderArgs,

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

/// Optional OTLP/gRPC export settings for runner traces and audit-safe logs.
#[derive(Clone, Debug, Args)]
pub struct TelemetryArgs {
    /// OTLP/gRPC endpoint. Export is disabled when this and
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` are unset.
    #[arg(
        long,
        global = true,
        env = "OTEL_EXPORTER_OTLP_ENDPOINT",
        value_name = "URL"
    )]
    pub otlp_endpoint: Option<String>,

    /// OpenTelemetry service name attached to logs and traces.
    #[arg(
        long,
        global = true,
        env = "OTEL_SERVICE_NAME",
        default_value = "dekopon-run",
        value_name = "NAME"
    )]
    pub otel_service_name: String,

    /// Quickwit index selected through the `qw-otel-logs-index` OTLP header.
    #[arg(
        long,
        global = true,
        env = "DEKOPON_OTEL_LOGS_INDEX",
        default_value = "otel-logs-v0_9",
        value_name = "INDEX"
    )]
    pub otel_logs_index: String,

    /// Quickwit index selected through the `qw-otel-traces-index` OTLP header.
    #[arg(
        long,
        global = true,
        env = "DEKOPON_OTEL_TRACES_INDEX",
        default_value = "otel-traces-v0_9",
        value_name = "INDEX"
    )]
    pub otel_traces_index: String,

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

    use super::{BrokerCommand, Cli, Command};

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
            "http://quickwit:7281",
            "--otel-service-name",
            "dekopon-run-test",
            "--otel-logs-index",
            "otel-logs-test",
            "--otel-traces-index",
            "otel-traces-test",
            "inspect",
            "--provider",
            "echo.wasm",
        ])
        .expect("valid telemetry settings");

        assert_eq!(
            cli.telemetry.otlp_endpoint.as_deref(),
            Some("http://quickwit:7281")
        );
        assert_eq!(cli.telemetry.otel_service_name, "dekopon-run-test");
        assert_eq!(cli.telemetry.otel_logs_index, "otel-logs-test");
        assert_eq!(cli.telemetry.otel_traces_index, "otel-traces-test");
        assert_eq!(cli.telemetry.otel_export_timeout_ms, 5_000);
    }

    #[test]
    fn telemetry_defaults_to_quickwit_0_9_indexes() {
        let cli = Cli::try_parse_from(["dekopon-run", "inspect", "--provider", "echo.wasm"])
            .expect("valid command line");

        assert_eq!(cli.telemetry.otel_logs_index, "otel-logs-v0_9");
        assert_eq!(cli.telemetry.otel_traces_index, "otel-traces-v0_9");
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
