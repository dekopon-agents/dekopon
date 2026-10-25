//! Console serialization mirrors the daemon keys; validation/export remain shared core APIs.
//! Error sources are deliberately not retained: parser/exporter errors can echo credential-bearing
//! input, and verbose CLI diagnostics walk sources. Only safe, actionable categories cross here.
use dekopon_telemetry::{
    Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, ExporterSettings, Install,
    TelemetryGuard, Transport,
};
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde::Deserialize;
use std::{
    io::{self, IsTerminal as _},
    path::Path,
    time::Duration,
};

// Network-client diagnostics are intentionally absent: headers/model credentials must not export.
const FILTER: &str = "off,dekopon_console=trace,dekopon_tui=trace,dekopon_agent=trace,dekopon_process=trace,dekopon_shell=trace,dekopon_model=trace";

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("could not read telemetry configuration")]
    Read,
    #[error(
        "invalid telemetry YAML: expected endpoint, transport, serviceName, exportTimeoutMs; credentials belong only in OTEL_EXPORTER_OTLP_HEADERS"
    )]
    Parse,
    #[error(
        "invalid telemetry settings: use a credential-free base endpoint, nonempty serviceName and positive exportTimeoutMs"
    )]
    Settings,
    #[error("could not initialize telemetry exporter or subscriber")]
    Install,
    #[error("telemetry flush/shutdown failed")]
    Shutdown,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Config {
    endpoint: String,
    #[serde(default = "transport")]
    transport: Transport,
    #[serde(default = "service_name")]
    service_name: String,
    #[serde(default = "timeout_ms")]
    export_timeout_ms: u64,
}
const fn transport() -> Transport {
    Transport::Grpc
}
fn service_name() -> String {
    "dekopon-console".into()
}
const fn timeout_ms() -> u64 {
    5000
}
fn settings(text: &str) -> Result<ExporterSettings, TelemetryError> {
    // Never include serde's diagnostic: it can contain the rejected value, including userinfo.
    let config: Config =
        serde_yaml::from_str(text).map_err(|_sensitive_error| TelemetryError::Parse)?;
    ExporterSettings::new(
        &config.endpoint,
        config.transport,
        &config.service_name,
        "dekopon-console",
        env!("CARGO_PKG_VERSION"),
        Duration::from_millis(config.export_timeout_ms),
    )
    .map_err(|_sensitive_error| TelemetryError::Settings)
}

pub fn initialize(
    path: Option<&Path>,
    verbosity: u8,
    no_color: bool,
) -> Result<TelemetryGuard, TelemetryError> {
    let settings = path
        .map(|path| {
            let text =
                std::fs::read_to_string(path).map_err(|_sensitive_error| TelemetryError::Read)?;
            settings(&text)
        })
        .transpose()?;
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let mut install = Install::new(Console {
        format: ConsoleFormat::Text {
            ansi: Some(!no_color),
            target: verbosity > 1,
            timestamps: false,
        },
        writer: ConsoleWriter::Stderr,
        // Never write diagnostics into the TUI. OTLP layers have independent filters.
        filter: ConsoleFilter::Directive(
            if io::stderr().is_terminal() {
                "off"
            } else {
                level
            }
            .into(),
        ),
    });
    let provider = if let Some(settings) = settings {
        let provider = settings
            .tracer_provider()
            .map_err(|_sensitive_error| TelemetryError::Install)?;
        let logs = match settings.logger_provider() {
            Ok(logs) => logs,
            Err(_) => {
                let _shutdown = provider.shutdown();
                return Err(TelemetryError::Install);
            }
        };
        install = install
            .with_logs(logs, FILTER)
            .with_shutdown_timeout(settings.timeout());
        provider
    } else {
        // No exporter, no network: IDs still exist and broker proposals adopt the real turn.
        SdkTracerProvider::builder().build()
    };
    install
        .with_traces(provider, "dekopon-console", FILTER)
        .install()
        .map_err(|_sensitive_error| TelemetryError::Install)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn telemetry_defaults_and_explicit_values_use_shared_validation() {
        let default = settings("endpoint: http://127.0.0.1:4317").unwrap();
        assert_eq!(default.transport(), Transport::Grpc);
        assert_eq!(default.service_name(), "dekopon-console");
        assert_eq!(default.timeout(), Duration::from_millis(5000));
        let explicit = settings("endpoint: http://127.0.0.1:4318\ntransport: http\nserviceName: fixture\nexportTimeoutMs: 1000").unwrap();
        assert_eq!(explicit.transport(), Transport::Http);
        assert_eq!(explicit.service_name(), "fixture");
        assert_eq!(explicit.timeout(), Duration::from_millis(1000));
    }
    #[test]
    fn telemetry_refuses_invalid_and_inline_credentials_without_echoing_input() {
        for text in [
            "{}",
            "endpoint: x\nheaders: synthetic-secret",
            "endpoint: http://user:synthetic-secret@localhost",
            "endpoint: ''",
            "endpoint: http://localhost?synthetic-secret",
            "endpoint: http://localhost\nexportTimeoutMs: 0",
            "endpoint: http://localhost\nserviceName: ''",
            "endpoint: http://localhost\ntransport: synthetic-secret",
        ] {
            let error = settings(text).unwrap_err();
            assert!(!error.to_string().contains("synthetic-secret"));
            assert!(std::error::Error::source(&error).is_none());
        }
    }
}

#[cfg(test)]
mod local_context_tests {
    use super::*;
    use tracing::Instrument as _;

    #[test]
    fn no_exporter_still_correlates_async_and_blocking_work() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let guard = initialize(None, 0, true).unwrap();
        runtime.block_on(async {
            let root = dekopon_tui::run::console_turn_span("model");
            async {
                let context = dekopon_telemetry::current_trace_context().unwrap();
                let trace = context.trace_id;
                tokio::spawn(
                    async move {
                        let span = tracing::Span::current();
                        tokio::task::spawn_blocking(move || {
                            let _entered = span.enter();
                            assert_eq!(
                                dekopon_telemetry::current_trace_context().unwrap().trace_id,
                                trace
                            );
                        })
                        .await
                        .unwrap();
                    }
                    .in_current_span(),
                )
                .await
                .unwrap();
            }
            .instrument(root)
            .await;
        });
        guard.shutdown().unwrap();
    }
}
