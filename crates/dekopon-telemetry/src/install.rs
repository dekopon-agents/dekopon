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
            "dekopon_run=trace",
            // A caller that silenced the exporter itself; the guarantee is idempotent.
            "dekopon_run=trace,opentelemetry=off",
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
                tracing::info!(target: "dekopon_run", "runner event");
            });

            assert_eq!(
                *recorded.0.lock().expect("target log"),
                vec!["dekopon_run".to_owned()],
                "{directive}"
            );
        }
    }

    /// A process that configured no exporter still runs this on the way out — the runner without
    /// an OTLP endpoint, the broker's offline provider mode — and must not report a failure for
    /// having nothing to flush.
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
