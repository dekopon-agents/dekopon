use std::{
    fs::File,
    io::{self, BufWriter},
    path::{Path, PathBuf},
    time::Duration,
};

use opentelemetry::{KeyValue, trace::TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{
    ExporterBuildError, LogExporter, SpanExporter, WithExportConfig, WithTonicConfig,
};
use opentelemetry_sdk::{Resource, logs::SdkLoggerProvider, trace::SdkTracerProvider};
use thiserror::Error;
use tonic::metadata::{MetadataMap, MetadataValue};
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{
    EnvFilter, Layer, fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use crate::cli::TelemetryArgs;

const OTEL_TRACE_FILTER: &str = "dekopon_run=trace,dekopon_model=trace,dekopon_provider_host=trace";
const OTEL_LOG_FILTER: &str = "dekopon_run=info,dekopon_model=info,dekopon_provider_host=info,hyper=off,h2=off,opentelemetry=off,tonic=off";

pub(crate) struct TraceGuard {
    chrome: Option<FlushGuard>,
    logger_provider: Option<SdkLoggerProvider>,
    tracer_provider: Option<SdkTracerProvider>,
    shutdown_timeout: Duration,
}

impl TraceGuard {
    /// Flushes every configured exporter before the short-lived runner exits.
    ///
    /// A configured OTLP endpoint is treated as part of the command contract: a flush failure is
    /// returned to the caller so a successful guest execution is not silently reported as fully
    /// observed.
    pub(crate) fn shutdown(mut self) -> Result<(), TraceError> {
        // Dropping the Chrome guard flushes its buffered writer. Do this while the tracing
        // subscriber and the OpenTelemetry providers are still alive.
        drop(self.chrome.take());

        let mut failures = Vec::new();
        if let Some(provider) = self.logger_provider.take() {
            if let Err(error) = provider.force_flush() {
                failures.push(format!("logs flush: {error}"));
            }
            if let Err(error) = provider.shutdown_with_timeout(self.shutdown_timeout) {
                failures.push(format!("logs shutdown: {error}"));
            }
        }
        if let Some(provider) = self.tracer_provider.take() {
            if let Err(error) = provider.force_flush() {
                failures.push(format!("traces flush: {error}"));
            }
            if let Err(error) = provider.shutdown_with_timeout(self.shutdown_timeout) {
                failures.push(format!("traces shutdown: {error}"));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(TraceError::Shutdown(failures.join("; ")))
        }
    }
}

pub(crate) fn initialize(
    verbosity: u8,
    no_color: bool,
    chrome_trace: Option<&Path>,
    telemetry: &TelemetryArgs,
) -> Result<TraceGuard, TraceError> {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let stderr_layer = fmt::layer()
        .with_ansi(!no_color)
        .with_target(verbosity > 1)
        .with_writer(io::stderr)
        .with_filter(EnvFilter::new(level));

    let (chrome_layer, chrome_guard) = if let Some(path) = chrome_trace {
        let file = File::create(path).map_err(|source| TraceError::Create {
            path: path.to_path_buf(),
            source,
        })?;
        let (layer, guard) = ChromeLayerBuilder::new()
            .writer(BufWriter::new(file))
            .include_args(true)
            .include_locations(true)
            .build();
        (
            Some(layer.with_filter(EnvFilter::new(OTEL_TRACE_FILTER))),
            Some(guard),
        )
    } else {
        (None, None)
    };

    let shutdown_timeout = Duration::from_millis(telemetry.otel_export_timeout_ms);
    if shutdown_timeout.is_zero() {
        return Err(TraceError::Configuration(
            "OTLP export timeout must be greater than zero".to_owned(),
        ));
    }

    let Some(endpoint) = telemetry.otlp_endpoint.as_deref() else {
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(chrome_layer)
            .try_init()
            .map_err(|error| TraceError::Subscriber(error.to_string()))?;
        return Ok(TraceGuard {
            chrome: chrome_guard,
            logger_provider: None,
            tracer_provider: None,
            shutdown_timeout,
        });
    };

    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(TraceError::Configuration(
            "OTLP endpoint must not be empty".to_owned(),
        ));
    }
    let service_name = telemetry.otel_service_name.trim();
    if service_name.is_empty() {
        return Err(TraceError::Configuration(
            "OpenTelemetry service name must not be empty".to_owned(),
        ));
    }

    let resource = Resource::builder()
        .with_service_name(service_name.to_owned())
        .with_attributes([
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("process.executable.name", "dekopon-run"),
        ])
        .build();

    let span_exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.to_owned())
        .with_timeout(shutdown_timeout)
        .with_metadata(index_metadata(
            "qw-otel-traces-index",
            &telemetry.otel_traces_index,
        )?)
        .build()
        .map_err(|source| TraceError::Exporter {
            signal: "trace",
            source,
        })?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(span_exporter)
        .build();
    let tracer = tracer_provider.tracer("dekopon-run");
    let otel_trace_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(EnvFilter::new(OTEL_TRACE_FILTER));

    let log_exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.to_owned())
        .with_timeout(shutdown_timeout)
        .with_metadata(index_metadata(
            "qw-otel-logs-index",
            &telemetry.otel_logs_index,
        )?)
        .build()
        .map_err(|source| TraceError::Exporter {
            signal: "log",
            source,
        })?;
    let logger_provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(log_exporter)
        .build();
    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider)
        .with_filter(EnvFilter::new(OTEL_LOG_FILTER));

    if let Err(error) = tracing_subscriber::registry()
        .with(stderr_layer)
        .with(chrome_layer)
        // Install the span layer before the log bridge so entered tracing spans activate an
        // OpenTelemetry context that the log SDK can use for trace/span correlation.
        .with(otel_trace_layer)
        .with(otel_log_layer)
        .try_init()
    {
        let _ = logger_provider.shutdown_with_timeout(shutdown_timeout);
        let _ = tracer_provider.shutdown_with_timeout(shutdown_timeout);
        return Err(TraceError::Subscriber(error.to_string()));
    }

    Ok(TraceGuard {
        chrome: chrome_guard,
        logger_provider: Some(logger_provider),
        tracer_provider: Some(tracer_provider),
        shutdown_timeout,
    })
}

fn index_metadata(header: &'static str, index: &str) -> Result<MetadataMap, TraceError> {
    let index = index.trim();
    if index.is_empty() {
        return Err(TraceError::Configuration(format!(
            "OpenTelemetry index for {header} must not be empty"
        )));
    }
    let value = MetadataValue::try_from(index).map_err(|error| {
        TraceError::Configuration(format!(
            "OpenTelemetry index {index:?} is not a valid gRPC metadata value: {error}"
        ))
    })?;
    let mut metadata = MetadataMap::new();
    metadata.insert(header, value);
    Ok(metadata)
}

#[derive(Debug, Error)]
pub(crate) enum TraceError {
    #[error("could not create trace file {}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid telemetry configuration: {0}")]
    Configuration(String),
    #[error("could not build OTLP {signal} exporter")]
    Exporter {
        signal: &'static str,
        #[source]
        source: ExporterBuildError,
    },
    #[error("could not install tracing subscriber: {0}")]
    Subscriber(String),
    #[error("could not flush OTLP telemetry: {0}")]
    Shutdown(String),
}

#[cfg(test)]
mod tests {
    use super::index_metadata;

    #[test]
    fn quickwit_index_metadata_is_signal_specific() {
        let metadata = index_metadata("qw-otel-traces-index", "otel-traces-v0_9")
            .expect("valid Quickwit index metadata");

        assert_eq!(
            metadata
                .get("qw-otel-traces-index")
                .expect("trace index header")
                .to_str()
                .expect("ASCII index"),
            "otel-traces-v0_9"
        );
        assert!(metadata.get("qw-otel-logs-index").is_none());
    }

    #[test]
    fn quickwit_index_metadata_rejects_empty_or_non_ascii_values() {
        assert!(index_metadata("qw-otel-logs-index", "  ").is_err());
        assert!(index_metadata("qw-otel-logs-index", "logs-\n-forged").is_err());
    }
}
