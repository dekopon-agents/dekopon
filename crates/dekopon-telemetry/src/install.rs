use std::{io, time::Duration};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::{logs::SdkLoggerProvider, trace::SdkTracerProvider};
use thiserror::Error;
use tracing_subscriber::{
    EnvFilter, Layer, Registry,
    fmt::{self, writer::BoxMakeWriter},
    layer::SubscriberExt as _,
    util::{SubscriberInitExt as _, TryInitError},
};

use crate::ExporterSettings;

/// This is appended to every OTLP filter so these SDK-internal failure logs are never re-exported,
/// which would otherwise loop when the receiver is down.
const EXPORTER_DIAGNOSTICS_OFF: &str = "opentelemetry=off";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleWriter {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleFormat {
    Json,
    Text {
        ansi: Option<bool>,
        target: bool,
        timestamps: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsoleFilter {
    Environment(String),
    Directive(String),
}

#[derive(Clone, Debug)]
pub struct Console {
    pub format: ConsoleFormat,
    pub writer: ConsoleWriter,
    pub filter: ConsoleFilter,
}

impl Console {
    fn layer(self) -> Box<dyn Layer<Registry> + Send + Sync> {
        let writer = match self.writer {
            ConsoleWriter::Stdout => BoxMakeWriter::new(io::stdout),
            ConsoleWriter::Stderr => BoxMakeWriter::new(io::stderr),
        };
        let filter = match self.filter {
            ConsoleFilter::Environment(default) => {
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default))
            }
            ConsoleFilter::Directive(directive) => EnvFilter::new(directive),
        };
        match self.format {
            ConsoleFormat::Json => fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .event_format(CorrelatedJson(
                    fmt::format()
                        .json()
                        .flatten_event(true)
                        .with_current_span(true),
                ))
                .with_writer(writer)
                .with_filter(filter)
                .boxed(),
            ConsoleFormat::Text {
                ansi,
                target,
                timestamps,
            } => {
                let layer = fmt::layer().with_target(target).with_writer(writer);
                let layer = match ansi {
                    Some(ansi) => layer.with_ansi(ansi),
                    None => layer,
                };
                if timestamps {
                    layer.with_filter(filter).boxed()
                } else {
                    layer.without_time().with_filter(filter).boxed()
                }
            }
        }
    }
}

struct CorrelatedJson(fmt::format::Format<fmt::format::Json>);

impl<S, N> fmt::FormatEvent<S, N> for CorrelatedJson
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        context: &fmt::FmtContext<'_, S, N>,
        mut writer: fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use opentelemetry::trace::TraceContextExt as _;
        let native = opentelemetry::Context::current();
        let span = native.span();
        let ids = span.span_context();
        if !ids.is_valid() {
            return self.0.format_event(context, writer, event);
        }
        let mut json = String::new();
        self.0
            .format_event(context, fmt::format::Writer::new(&mut json), event)?;
        let object = json.strip_suffix("}\n").ok_or(std::fmt::Error)?;
        writeln!(
            writer,
            "{object},\"trace_id\":\"{}\",\"span_id\":\"{}\"}}",
            ids.trace_id(),
            ids.span_id(),
        )
    }
}

struct TraceExport {
    provider: SdkTracerProvider,
    tracer_name: String,
    filter: String,
}

struct LogExport {
    provider: SdkLoggerProvider,
    filter: String,
}

/// The OTLP span layer installs before the log bridge deliberately, so an entered span has already
/// activated the OpenTelemetry context the log bridge correlates against.
pub struct Install {
    console: Console,
    extra: Option<Box<dyn Layer<Registry> + Send + Sync>>,
    traces: Option<TraceExport>,
    logs: Option<LogExport>,
    shutdown_timeout: Option<Duration>,
}

impl Install {
    #[must_use]
    pub const fn new(console: Console) -> Self {
        Self {
            console,
            extra: None,
            traces: None,
            logs: None,
            shutdown_timeout: None,
        }
    }

    #[must_use]
    pub fn with_traces(
        mut self,
        provider: SdkTracerProvider,
        tracer_name: impl Into<String>,
        filter: impl Into<String>,
    ) -> Self {
        self.traces = Some(TraceExport {
            provider,
            tracer_name: tracer_name.into(),
            filter: filter.into(),
        });
        self
    }

    #[must_use]
    pub fn with_logs(mut self, provider: SdkLoggerProvider, filter: impl Into<String>) -> Self {
        self.logs = Some(LogExport {
            provider,
            filter: filter.into(),
        });
        self
    }

    #[must_use]
    pub fn with_layer<L>(mut self, layer: L) -> Self
    where
        L: Layer<Registry> + Send + Sync + 'static,
    {
        self.extra = Some(layer.boxed());
        self
    }

    #[must_use]
    pub const fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = Some(timeout);
        self
    }

    pub fn install(self) -> Result<TelemetryGuard, InstallError> {
        let Self {
            console,
            extra,
            traces,
            logs,
            shutdown_timeout,
        } = self;

        let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = vec![console.layer()];
        layers.extend(extra);
        let tracer_provider = traces.map(|traces| {
            let tracer = traces.provider.tracer(traces.tracer_name);
            layers.push(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    .with_filter(otlp_filter(&traces.filter))
                    .boxed(),
            );
            traces.provider
        });
        let logger_provider = logs.map(|logs| {
            layers.push(
                OpenTelemetryTracingBridge::new(&logs.provider)
                    .with_filter(otlp_filter(&logs.filter))
                    .boxed(),
            );
            logs.provider
        });

        let guard = TelemetryGuard {
            tracer_provider,
            logger_provider,
            shutdown_timeout,
        };
        if let Err(error) = tracing_subscriber::registry().with(layers).try_init() {
            drop(guard.shutdown());
            return Err(InstallError::from(error));
        }
        Ok(guard)
    }
}

#[derive(Debug)]
#[must_use = "an exporter that is never flushed drops its last batch"]
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    shutdown_timeout: Option<Duration>,
}

impl TelemetryGuard {
    pub fn shutdown(self) -> Result<(), ShutdownError> {
        let mut failures = Vec::new();
        if let Some(provider) = self.logger_provider {
            if let Err(error) = provider.force_flush() {
                failures.push(format!("logs flush: {error}"));
            }
            let stopped = match self.shutdown_timeout {
                Some(timeout) => provider.shutdown_with_timeout(timeout),
                None => provider.shutdown(),
            };
            if let Err(error) = stopped {
                failures.push(format!("logs shutdown: {error}"));
            }
        }
        if let Some(provider) = self.tracer_provider {
            if let Err(error) = provider.force_flush() {
                failures.push(format!("traces flush: {error}"));
            }
            let stopped = match self.shutdown_timeout {
                Some(timeout) => provider.shutdown_with_timeout(timeout),
                None => provider.shutdown(),
            };
            if let Err(error) = stopped {
                failures.push(format!("traces shutdown: {error}"));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(ShutdownError(failures.join("; ")))
        }
    }
}

#[must_use]
pub fn optional_tracer_provider(
    settings: Option<&ExporterSettings>,
    program: &str,
) -> Option<SdkTracerProvider> {
    match settings?.tracer_provider() {
        Ok(provider) => Some(provider),
        Err(error) => {
            eprintln!("{program}: telemetry disabled: {error}");
            None
        }
    }
}

#[must_use]
pub fn optional_logger_provider(
    settings: Option<&ExporterSettings>,
    program: &str,
) -> Option<SdkLoggerProvider> {
    match settings?.logger_provider() {
        Ok(provider) => Some(provider),
        Err(error) => {
            eprintln!("{program}: log export disabled: {error}");
            None
        }
    }
}

fn otlp_filter(directive: &str) -> EnvFilter {
    EnvFilter::new(format!("{directive},{EXPORTER_DIAGNOSTICS_OFF}"))
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct InstallError(#[from] TryInitError);

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ShutdownError(String);

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::{Layer as _, layer::Context, layer::SubscriberExt as _, registry};

    use super::{TelemetryGuard, otlp_filter};

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("output").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn json_ids_match_native_context_and_are_absent_without_it() {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::fmt;

        for enabled in [false, true] {
            let output = Output::default();
            let writer = output.clone();
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
            let console = fmt::layer()
                .json()
                .flatten_event(true)
                .event_format(super::CorrelatedJson(
                    fmt::format().json().flatten_event(true),
                ))
                .with_writer(move || writer.clone());
            let subscriber = registry().with(console).with(
                enabled
                    .then(|| tracing_opentelemetry::layer().with_tracer(provider.tracer("test"))),
            );
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(retained = 7, "outside");
                let span = tracing::info_span!("native", retained_span = 8);
                let _entered = span.enter();
                let native = crate::current_trace_context();
                tracing::info!(retained = 9, "inside");
                let text = String::from_utf8(output.0.lock().expect("output").clone())
                    .expect("UTF-8 JSON");
                let lines: Vec<_> = text.lines().collect();
                assert_eq!(lines.len(), 2);
                assert!(!lines[0].contains("trace_id"));
                assert!(!lines[0].contains("span_id"));
                assert!(lines[0].contains("\"retained\":7"));
                assert!(lines[1].contains("\"retained\":9"));
                assert!(lines[1].contains("\"retained_span\":8"));
                if let Some(parts) = native {
                    assert!(enabled);
                    assert!(lines[1].contains(&format!(
                        "\"trace_id\":\"{}\"",
                        opentelemetry::trace::TraceId::from_bytes(parts.trace_id)
                    )));
                    assert!(lines[1].contains(&format!(
                        "\"span_id\":\"{}\"",
                        opentelemetry::trace::SpanId::from_bytes(parts.span_id)
                    )));
                } else {
                    assert!(!enabled);
                    assert!(!lines[1].contains("trace_id"));
                    assert!(!lines[1].contains("span_id"));
                }
            });
            provider.shutdown().expect("shutdown");
        }
    }

    #[test]
    fn an_active_span_has_no_trace_context_until_the_opentelemetry_layer_is_installed() {
        use opentelemetry::trace::TracerProvider as _;

        tracing::subscriber::with_default(registry(), || {
            let outer = tracing::info_span!("gateway.session");
            let _outer = outer.enter();
            let inner = tracing::info_span!("broker.leg");
            let _inner = inner.enter();
            assert!(
                crate::current_trace_context().is_none(),
                "no layer means no context to read"
            );
        });

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let subscriber =
            registry().with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("gateway.session");
            let _entered = span.enter();
            assert!(
                crate::current_trace_context().is_some(),
                "the layer mints identifiers whether or not anything exports them"
            );
        });
        provider.shutdown().expect("shutdown");
    }

    #[test]
    fn an_unsampled_remote_parent_makes_every_span_beneath_it_non_recording() {
        use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        for (flags, recorded) in [(0x00_u8, false), (0x01_u8, true)] {
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
            let subscriber = registry()
                .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
            tracing::subscriber::with_default(subscriber, || {
                let span = tracing::info_span!("broker.invocation");
                span.set_parent(crate::remote_context(crate::TraceContextParts {
                    trace_id: [0x11; 16],
                    span_id: [0x22; 8],
                    flags,
                }))
                .expect("the parent context is well formed");

                let context = span.context();
                let child = context.span();
                let child_context = child.span_context();
                assert_eq!(
                    child_context.trace_id().to_bytes(),
                    [0x11; 16],
                    "the child joins the parent's trace either way"
                );
                assert_eq!(child_context.is_sampled(), recorded, "flags {flags:#04x}");
                assert_eq!(child.is_recording(), recorded, "flags {flags:#04x}");
            });
            provider.shutdown().expect("shutdown");
        }
    }

    #[derive(Clone, Default)]
    struct RecordTargets(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecordTargets {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            self.0
                .lock()
                .expect("target log")
                .push(event.metadata().target().to_owned());
        }
    }

    #[test]
    fn an_otlp_layer_never_sees_the_exporters_own_records() {
        for directive in [
            "dekopond=trace",
            "dekopond=trace,opentelemetry=off",
            "trace",
        ] {
            let recorded = RecordTargets::default();
            let subscriber = registry().with(recorded.clone().with_filter(otlp_filter(directive)));

            tracing::subscriber::with_default(subscriber, || {
                tracing::error!(target: "opentelemetry", "api diagnostic");
                tracing::error!(target: "opentelemetry-sdk", "sdk diagnostic");
                tracing::error!(target: "opentelemetry-otlp", "exporter diagnostic");
                tracing::info!(target: "dekopond", "gateway event");
            });

            assert_eq!(
                *recorded.0.lock().expect("target log"),
                vec!["dekopond".to_owned()],
                "{directive}"
            );
        }
    }

    #[test]
    fn a_guard_without_exporters_shuts_down_cleanly() {
        let guard = TelemetryGuard {
            tracer_provider: None,
            logger_provider: None,
            shutdown_timeout: None,
        };
        assert!(guard.shutdown().is_ok());
    }
}
