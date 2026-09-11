//! One subscriber installation for every exporting Dekopon process.
//!
//! The exporting binaries each used to hand-roll the same sequence — a registry, a console layer,
//! an OTLP span layer, sometimes an OTLP log bridge, then a flush and shutdown on the way out —
//! differing only in the writer, the rendering, and their own crate filters. The sequence lives
//! here so that a change to it happens once: a newly silenced target, a second signal, or a
//! different flush order is then true of every process rather than of whichever `main` was edited.

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

/// The `tracing` target prefix the OpenTelemetry SDK reports its own failures under.
///
/// `internal-logs` is enabled workspace-wide, so an OTLP layer that accepted these records would
/// export the failures of its own export — and a receiver that is down produces exactly the
/// records it cannot accept. Appended to every OTLP filter built here rather than written into
/// each binary's directive, so a new exporting process cannot forget it. It is a prefix rather
/// than an exact target because the SDK crates log under their package names, hyphens and all:
/// `opentelemetry`, `opentelemetry-sdk`, `opentelemetry-otlp`.
const EXPORTER_DIAGNOSTICS_OFF: &str = "opentelemetry=off";

/// Where a process writes its own records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleWriter {
    /// Standard output, which is the daemons' structured log contract.
    Stdout,
    /// Standard error, leaving standard output for command results.
    Stderr,
}

/// How a process renders its own records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleFormat {
    /// One flattened JSON object per event, carrying the current span.
    Json,
    /// Human-readable lines for an operator's terminal.
    Text {
        /// `None` keeps `tracing-subscriber`'s own default, which honors `NO_COLOR`.
        ansi: Option<bool>,
        /// Whether each line names the emitting target.
        target: bool,
        /// Whether each line carries a timestamp.
        timestamps: bool,
    },
}

/// Which records reach the console.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsoleFilter {
    /// `RUST_LOG` when it is set and parses, and this directive otherwise.
    Environment(String),
    /// This directive, whatever the environment says.
    Directive(String),
}

/// One process's console layer: what it renders, where, and for which records.
#[derive(Clone, Debug)]
pub struct Console {
    /// How records are rendered.
    pub format: ConsoleFormat,
    /// Where rendered records are written.
    pub writer: ConsoleWriter,
    /// Which records are rendered at all.
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

/// Delegate all existing JSON fields to tracing-subscriber; only native context adds IDs.
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
        // Subscriber callbacks cannot re-enter the tracing dispatcher. The OTel layer
        // activates this same native context on span entry, independently of callbacks.
        let native = opentelemetry::Context::current();
        let span = native.span();
        let ids = span.span_context();
        if !ids.is_valid() {
            return self.0.format_event(context, writer, event);
        }
        let mut json = String::new();
        self.0
            .format_event(context, fmt::format::Writer::new(&mut json), event)?;
        // The delegated formatter always emits one JSON object and a newline.
        let object = json.strip_suffix("}\n").ok_or(std::fmt::Error)?;
        writeln!(
            writer,
            "{object},\"trace_id\":\"{}\",\"span_id\":\"{}\"}}",
            ids.trace_id(),
            ids.span_id(),
        )
    }
}

/// A tracer provider and the layer settings that feed it.
struct TraceExport {
    provider: SdkTracerProvider,
    tracer_name: String,
    filter: String,
}

/// A logger provider and the layer settings that feed it.
struct LogExport {
    provider: SdkLoggerProvider,
    filter: String,
}

/// Builds one process's subscriber, and the guard that stops its exporters.
///
/// Layers are installed in the order they are configured here: console, then any extra layer, then
/// the OTLP span layer, then the OTLP log bridge. The span layer precedes the bridge deliberately,
/// so an entered span has already activated an OpenTelemetry context the log SDK can correlate
/// against.
pub struct Install {
    console: Console,
    extra: Option<Box<dyn Layer<Registry> + Send + Sync>>,
    traces: Option<TraceExport>,
    logs: Option<LogExport>,
    shutdown_timeout: Option<Duration>,
}

impl Install {
    /// Starts an installation that writes only to the console.
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

    /// Exports spans from `filter`'s targets through `provider`, under a tracer named for the
    /// calling executable.
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

    /// Exports log records from `filter`'s targets through `provider`.
    #[must_use]
    pub fn with_logs(mut self, provider: SdkLoggerProvider, filter: impl Into<String>) -> Self {
        self.logs = Some(LogExport {
            provider,
            filter: filter.into(),
        });
        self
    }

    /// Adds one process-specific layer, such as a local Chrome trace writer.
    #[must_use]
    pub fn with_layer<L>(mut self, layer: L) -> Self
    where
        L: Layer<Registry> + Send + Sync + 'static,
    {
        self.extra = Some(layer.boxed());
        self
    }

    /// Bounds the final flush of each provider.
    ///
    /// Without this the SDK's own default deadline applies, which is what a long-lived daemon
    /// wants; a short-lived command that has already produced its output sets its export timeout
    /// here so exit cannot stall on an unreachable receiver.
    #[must_use]
    pub const fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = Some(timeout);
        self
    }

    /// Installs the process-wide subscriber and returns the guard that stops its exporters.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError`] when a subscriber is already installed in this process. Any
    /// provider built for this installation is shut down before returning, because nothing was
    /// exported through it and nothing else holds it.
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
            // Best-effort rollback of providers that never received a span: `try_init` just
            // failed, so nothing was exported, and there is no subscriber of ours for a shutdown
            // diagnostic to reach. The install failure is the one an operator has to act on.
            drop(guard.shutdown());
            return Err(InstallError::from(error));
        }
        Ok(guard)
    }
}

/// Stops the exporters an [`Install`] built.
///
/// Batch exporters hold records that have not left the process, so a run that ends without this
/// loses whatever the last batch window was still holding.
#[derive(Debug)]
#[must_use = "an exporter that is never flushed drops its last batch"]
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    shutdown_timeout: Option<Duration>,
}

impl TelemetryGuard {
    /// Flushes and stops every configured provider, reporting every failure rather than the first.
    ///
    /// Logs are stopped before traces, and a process that configured neither succeeds without
    /// doing anything. What a caller does with a failure is its own policy: a short-lived command
    /// fails, because a successful run reported as fully observed when it was not is a lie; a
    /// daemon logs and carries on, because the broker's durable audit rather than telemetry is the
    /// record of what happened.
    ///
    /// # Errors
    ///
    /// Returns [`ShutdownError`] naming each signal and stage that failed.
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

/// Builds a tracer provider for a process that must start even when export cannot.
///
/// Returns `None` after naming the cause on stderr: no subscriber is installed yet, so stderr is
/// the only channel there is, and telemetry must never keep a service from starting. Answering
/// authorized work is the contract; a dashboard is not.
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

/// Builds an OTLP layer's filter from a caller's crate directive.
fn otlp_filter(directive: &str) -> EnvFilter {
    EnvFilter::new(format!("{directive},{EXPORTER_DIAGNOSTICS_OFF}"))
}

/// A `tracing` subscriber was already installed in this process.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct InstallError(#[from] TryInitError);

/// Every flush and shutdown failure raised while stopping one process's exporters.
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

    /// Why a broker request must carry a trace the caller minted rather than one it read.
    ///
    /// [`TelemetryBuilder::install`] attaches the `tracing-opentelemetry` layer only when an OTLP
    /// trace exporter is configured. Without one there is no OpenTelemetry context behind an open
    /// span at all, so `current_trace_context` answers `None` however deep the span stack is — the
    /// identifiers are absent, not invalid, and no amount of span nesting produces them. With the
    /// layer attached the context is valid even though this provider exports nowhere, which is
    /// what separates the two cases: the exporter decides where spans go, the layer decides
    /// whether they have identifiers at all.
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

    /// The `sampled` bit on an adopted remote parent decides whether the child records at all.
    ///
    /// `dekopon-brokerd` joins a client's trace by handing [`remote_context`] to `set_parent`, and
    /// no Dekopon process configures a sampler, so the SDK default `ParentBased(AlwaysOn)` applies:
    /// beneath an unsampled parent every span is created non-recording and never exported. That is
    /// why a client that exports nothing still mints its `traceparent` with the flag *set* — the
    /// bit instructs the receiver rather than describing the sender, and clearing it would silence
    /// an exporting broker sitting behind a non-exporting gateway.
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

    /// Records the target of every event a layer is actually asked to handle.
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

    /// No OTLP layer may see the exporter's own diagnostics, whatever directive the calling
    /// binary supplies. `internal-logs` is enabled workspace-wide, so a layer that accepted them
    /// would export the failures of its own export — and the failing receiver is exactly what
    /// generates them. Each binary used to carry its own copy of this test over its own constant,
    /// which proved the property for three strings rather than for the mechanism; the permissive
    /// directive below is the case those copies could not have caught.
    #[test]
    fn an_otlp_layer_never_sees_the_exporters_own_records() {
        for directive in [
            // A caller that named only its own crates.
            "dekopond=trace",
            // A caller that silenced the exporter itself; the guarantee is idempotent.
            "dekopond=trace,opentelemetry=off",
            // A caller that admitted everything. Without the appended directive this layer would
            // export every diagnostic the export itself produced.
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

    /// A process that configured no exporter still runs this on the way out — a daemon started
    /// without an OTLP endpoint, the broker's offline provider mode — and must not report a failure
    /// for having nothing to flush.
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
