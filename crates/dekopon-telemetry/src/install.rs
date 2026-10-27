use std::{
    cell::Cell,
    io::{self, Write as _},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::Duration,
};

use parking_lot::Mutex;

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
    fn layer(self, writer: BoxMakeWriter) -> Box<dyn Layer<Registry> + Send + Sync> {
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

type ConsoleOutput = Box<dyn io::Write + Send>;

thread_local! {
    static CONSOLE_WRITER_THREAD: Cell<bool> = const { Cell::new(false) };
    static NOTICE_DROPPED: Cell<bool> = const { Cell::new(false) };
}

enum ConsoleState {
    Queued(SyncSender<Vec<u8>>),
    Direct(ConsoleOutput),
    Closed,
}

struct ConsoleSink {
    state: Arc<Mutex<ConsoleState>>,
    bytes: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
}

impl ConsoleSink {
    const MAX_LINES: usize = 1024;
    const MAX_BYTES: usize = 4 * 1024 * 1024;

    fn new(mut output: ConsoleOutput) -> Result<(Self, ConsoleWorker), InstallError> {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(Self::MAX_LINES);
        let (completed, completion) = mpsc::sync_channel(1);
        let sink = Self {
            state: Arc::new(Mutex::new(ConsoleState::Queued(sender))),
            bytes: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        };
        let bytes = Arc::clone(&sink.bytes);
        let dropped = Arc::clone(&sink.dropped);
        #[expect(
            clippy::disallowed_methods,
            reason = "owner: TelemetryGuard joins it at shutdown; bound: one writer thread per install, 1024 queued lines and 4 MiB"
        )]
        let handle = std::thread::Builder::new()
            .name("dekopon-console-stdout".into())
            .spawn(move || {
                CONSOLE_WRITER_THREAD.set(true);
                let mut reported = 0;
                while let Ok(line) = receiver.recv() {
                    let total = dropped.load(Ordering::Relaxed);
                    if total != reported {
                        NOTICE_DROPPED.set(false);
                        tracing::warn!(
                            audit.event = "telemetry.console.dropped",
                            console.dropped.lines = total,
                            "telemetry.console.dropped"
                        );
                        if !NOTICE_DROPPED.get() {
                            reported = total;
                        }
                    }
                    if output.write_all(&line).is_err() {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    bytes.fetch_sub(line.len(), Ordering::Relaxed);
                }
                match completed.send(()) {
                    Ok(()) | Err(mpsc::SendError(())) => (output, reported),
                }
            })
            .map_err(|error| InstallError::ConsoleSpawn { kind: error.kind() })?;
        let worker = ConsoleWorker {
            state: Arc::clone(&sink.state),
            dropped: Arc::clone(&sink.dropped),
            completion,
            handle,
        };
        Ok((sink, worker))
    }
}

impl<'a> fmt::MakeWriter<'a> for ConsoleSink {
    type Writer = &'a Self;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

impl io::Write for &ConsoleSink {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        match &mut *self.state.lock() {
            ConsoleState::Queued(sender) => {
                if self
                    .bytes
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bytes| {
                        bytes
                            .checked_add(line.len())
                            .filter(|total| *total <= ConsoleSink::MAX_BYTES)
                    })
                    .is_ok()
                {
                    match sender.try_send(line.to_vec()) {
                        Ok(()) => return Ok(line.len()),
                        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                            self.bytes.fetch_sub(line.len(), Ordering::Relaxed);
                        }
                    }
                }
            }
            ConsoleState::Direct(output) => {
                output.write_all(line)?;
                return Ok(line.len());
            }
            ConsoleState::Closed => {}
        }
        if CONSOLE_WRITER_THREAD.get() {
            NOTICE_DROPPED.set(true);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ConsoleWorker {
    state: Arc<Mutex<ConsoleState>>,
    dropped: Arc<AtomicU64>,
    completion: mpsc::Receiver<()>,
    handle: JoinHandle<(ConsoleOutput, u64)>,
}

impl std::fmt::Debug for ConsoleWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsoleWorker")
            .finish_non_exhaustive()
    }
}

impl ConsoleWorker {
    fn shutdown(self, timeout: Duration) -> Result<(), ShutdownError> {
        *self.state.lock() = ConsoleState::Closed;
        match self.completion.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(ShutdownError(format!(
                    "console writer did not finish within the timeout: {timeout:?}"
                )));
            }
        }
        let (output, reported) = self.handle.join().map_err(|payload| {
            ShutdownError(format!(
                "console writer shutdown: writer panicked: {}",
                crate::exporter::panic_message(&*payload)
            ))
        })?;
        *self.state.lock() = ConsoleState::Direct(output);
        let total = self.dropped.load(Ordering::Relaxed);
        if total != reported {
            tracing::warn!(
                audit.event = "telemetry.console.dropped",
                console.dropped.lines = total,
                "telemetry.console.dropped"
            );
        }
        Ok(())
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
        let array = native.get::<crate::EventStringArray>();
        if !ids.is_valid() && array.is_none() {
            return self.0.format_event(context, writer, event);
        }
        let mut json = String::new();
        self.0
            .format_event(context, fmt::format::Writer::new(&mut json), event)?;
        let object = json.strip_suffix("}\n").ok_or(std::fmt::Error)?;
        writer.write_str(object)?;
        if ids.is_valid() {
            write!(
                writer,
                ",\"trace_id\":\"{}\",\"span_id\":\"{}\"",
                ids.trace_id(),
                ids.span_id()
            )?;
        }
        if let Some(array) = array {
            let name =
                serde_json::to_string(array.name).map_err(|_invalid_json| std::fmt::Error)?;
            let values =
                serde_json::to_string(&array.values).map_err(|_invalid_json| std::fmt::Error)?;
            write!(writer, ",{name}:{values}")?;
        }
        writeln!(writer, "}}")
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

        let (writer, console_worker) = match console.writer {
            ConsoleWriter::Stdout => {
                let (sink, worker) = ConsoleSink::new(Box::new(io::stdout()))?;
                (BoxMakeWriter::new(sink), Some(worker))
            }
            ConsoleWriter::Stderr => (BoxMakeWriter::new(io::stderr), None),
        };
        let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = vec![console.layer(writer)];
        layers.extend(extra);
        let tracer_provider = traces.map(|traces| {
            let tracer = traces.provider.tracer(traces.tracer_name);
            layers.push(
                trace_layer(tracer)
                    .with_filter(otlp_filter(&traces.filter))
                    .boxed(),
            );
            traces.provider
        });
        let logger_provider = logs.map(|logs| {
            layers.push(
                OpenTelemetryTracingBridge::new(&logs.provider)
                    .with_filter(log_filter(&logs.filter))
                    .boxed(),
            );
            logs.provider
        });

        let guard = TelemetryGuard {
            console_worker,
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
    console_worker: Option<ConsoleWorker>,
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    shutdown_timeout: Option<Duration>,
}

impl TelemetryGuard {
    pub fn shutdown(self) -> Result<(), ShutdownError> {
        let mut failures = Vec::new();
        if let Some(provider) = self.logger_provider {
            let stopped = match self.shutdown_timeout {
                Some(timeout) => provider.shutdown_with_timeout(timeout),
                None => provider.shutdown(),
            };
            if let Err(error) = stopped {
                failures.push(format!("logs shutdown: {error}"));
            }
        }
        if let Some(provider) = self.tracer_provider {
            let stopped = match self.shutdown_timeout {
                Some(timeout) => provider.shutdown_with_timeout(timeout),
                None => provider.shutdown(),
            };
            if let Err(error) = stopped {
                failures.push(format!("traces shutdown: {error}"));
            }
        }

        if let Some(worker) = self.console_worker
            && let Err(error) =
                worker.shutdown(self.shutdown_timeout.unwrap_or(Duration::from_secs(1)))
        {
            failures.push(error.to_string());
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
pub enum InstallError {
    #[error(transparent)]
    Subscriber(#[from] TryInitError),
    #[error("could not spawn console writer: {kind}")]
    ConsoleSpawn { kind: io::ErrorKind },
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ShutdownError(String);

fn log_filter<S>(directive: &str) -> impl tracing_subscriber::layer::Filter<S> + use<S>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    use tracing::subscriber::Interest;
    use tracing_subscriber::filter::{FilterExt as _, dynamic_filter_fn};

    otlp_filter(directive)
        .or(dynamic_filter_fn(|metadata, _| {
            metadata.is_event()
                && metadata.fields().field("audit.event").is_some()
                && opentelemetry::Context::current()
                    .get::<crate::EventStringArray>()
                    .is_some()
        })
        .with_callsite_filter(|metadata| {
            if metadata.is_event() && metadata.fields().field("audit.event").is_some() {
                Interest::sometimes()
            } else {
                Interest::never()
            }
        }))
        .and(EnvFilter::new(format!("trace,{EXPORTER_DIAGNOSTICS_OFF}")))
}

fn trace_layer<S>(
    tracer: opentelemetry_sdk::trace::Tracer,
) -> tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_error_events_to_status(false)
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::sync::Arc;

    use tracing_subscriber::{Layer as _, layer::Context, layer::SubscriberExt as _, registry};

    use super::{TelemetryGuard, otlp_filter};

    #[test]
    fn an_error_field_on_an_event_does_not_set_its_span_status() {
        use opentelemetry::trace::{Status, TracerProvider as _};
        use opentelemetry_sdk::{
            error::OTelSdkResult,
            trace::{SdkTracerProvider, SpanData, SpanExporter},
        };

        #[derive(Clone, Debug, Default)]
        struct Exported(Arc<Mutex<Vec<SpanData>>>);

        impl SpanExporter for Exported {
            async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
                self.0.lock().extend(batch);
                Ok(())
            }
        }

        let exported = Exported::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exported.clone())
            .build();
        tracing::subscriber::with_default(
            registry().with(super::trace_layer(provider.tracer("status-test"))),
            || {
                tracing::info_span!("model.complete").in_scope(|| {
                    tracing::warn!(error = "upstream said something private");
                });
            },
        );
        let spans = exported.0.lock().clone();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].status, Status::Unset);
    }

    thread_local! {
        static WRITER_DISPATCH: std::cell::RefCell<Option<tracing::dispatcher::DefaultGuard>> = const {
            std::cell::RefCell::new(None)
        };
    }

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct StalledOutput {
        output: Output,
        started: Option<std::sync::mpsc::SyncSender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        written: std::sync::mpsc::SyncSender<()>,
        dispatch: Option<std::sync::mpsc::Receiver<tracing::Dispatch>>,
    }

    impl std::io::Write for StalledOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(dispatch) = self.dispatch.take() {
                let dispatch = dispatch.recv().expect("writer dispatch supplied");
                WRITER_DISPATCH.with(|guard| {
                    *guard.borrow_mut() = Some(tracing::dispatcher::set_default(&dispatch));
                });
            }
            if let Some(started) = self.started.take() {
                started.send(()).expect("first write started");
                self.release.recv().expect("reader released");
            }
            let size = self.output.write(bytes)?;
            self.written.send(()).expect("write observed");
            Ok(size)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn console_guard(worker: super::ConsoleWorker) -> TelemetryGuard {
        TelemetryGuard {
            console_worker: Some(worker),
            tracer_provider: None,
            logger_provider: None,
            shutdown_timeout: None,
        }
    }

    fn json_lines(output: &Output) -> Vec<serde_json::Value> {
        let bytes = output.0.lock();
        std::str::from_utf8(&bytes)
            .expect("UTF-8 JSON")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON record"))
            .collect()
    }

    #[test]
    fn a_stalled_console_reader_drops_and_counts_instead_of_blocking() {
        let output = Output::default();
        let (started, start) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let (written, writes) = std::sync::mpsc::sync_channel(super::ConsoleSink::MAX_LINES + 2);
        let (sink, worker) = super::ConsoleSink::new(Box::new(StalledOutput {
            output: output.clone(),
            started: Some(started),
            release: released,
            written,
            dispatch: None,
        }))
        .expect("console writer spawned");
        let subscriber = registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_writer(sink)
                .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                    metadata.target() == "stalled_console_test"
                        || metadata.target() == module_path!().trim_end_matches("::tests")
                })),
        );
        tracing::subscriber::set_global_default(subscriber).expect("sole global test subscriber");
        let guard = console_guard(worker);
        tracing::info!(target: "stalled_console_test", sequence = 0);
        start.recv().expect("writer stalled");
        let queued = super::ConsoleSink::MAX_LINES;
        for sequence in 1..=queued + 7 {
            tracing::info!(target: "stalled_console_test", sequence);
        }
        release.send(()).expect("release writer");
        for _ in 0..queued + 2 {
            writes.recv().expect("line written");
        }
        guard.shutdown().expect("console drained");
        let lines = json_lines(&output);
        assert_eq!(lines.len(), queued + 2);
        for (sequence, line) in lines[..=queued].iter().enumerate() {
            assert_eq!(line["sequence"], sequence);
        }
        assert_eq!(
            lines[queued + 1]["audit.event"],
            "telemetry.console.dropped"
        );
        assert_eq!(lines[queued + 1]["console.dropped.lines"], 7);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line["audit.event"] == "telemetry.console.dropped")
                .count(),
            1
        );
    }

    #[test]
    fn shutdown_drains_queued_console_lines_and_then_writes_directly() {
        let output = Output::default();
        let (started, start) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let (written, _writes) = std::sync::mpsc::sync_channel(4);
        let (sink, worker) = super::ConsoleSink::new(Box::new(StalledOutput {
            output: output.clone(),
            started: Some(started),
            release: released,
            written,
            dispatch: None,
        }))
        .expect("console writer spawned");
        let state = Arc::clone(&sink.state);
        let subscriber = registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_writer(sink),
        );
        tracing::subscriber::with_default(subscriber, || {
            let guard = console_guard(worker);
            tracing::info!(sequence = 0);
            start.recv().expect("writer stalled");
            for sequence in 1..3 {
                tracing::info!(sequence);
            }
            std::thread::scope(|scope| {
                let releaser = scope.spawn(move || {
                    wait_until_console_is_closed(&state);
                    release.send(()).expect("release writer");
                });
                guard.shutdown().expect("console drained");
                releaser.join().expect("writer released");
            });
            tracing::info!(sequence = 3);
        });
        let lines = json_lines(&output);
        assert_eq!(lines.len(), 4);
        for (sequence, line) in lines.iter().enumerate() {
            assert_eq!(line["sequence"], sequence);
        }
    }

    #[test]
    fn a_drop_count_pending_at_shutdown_is_reported_after_it() {
        let output = Output::default();
        let (started, start) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let queued = super::ConsoleSink::MAX_LINES;
        let (written, _writes) = std::sync::mpsc::sync_channel(queued + 2);
        let (dispatch_sender, dispatch_receiver) = std::sync::mpsc::sync_channel(1);
        let (sink, worker) = super::ConsoleSink::new(Box::new(StalledOutput {
            output: output.clone(),
            started: Some(started),
            release: released,
            written,
            dispatch: Some(dispatch_receiver),
        }))
        .expect("console writer spawned");
        let state = Arc::clone(&sink.state);
        let dropped = Arc::clone(&sink.dropped);
        let dispatch = tracing::Dispatch::new(
            registry().with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_writer(sink),
            ),
        );
        dispatch_sender
            .send(dispatch.clone())
            .expect("supply writer dispatch");
        tracing::dispatcher::with_default(&dispatch, || {
            let guard = console_guard(worker);
            tracing::info!(sequence = 0);
            start.recv().expect("writer stalled");
            for sequence in 1..=queued + 7 {
                tracing::info!(sequence);
            }
            assert_eq!(dropped.load(super::Ordering::Relaxed), 7);
            std::thread::scope(|scope| {
                let releaser = scope.spawn(move || {
                    wait_until_console_is_closed(&state);
                    release.send(()).expect("release writer");
                });
                guard.shutdown().expect("console drained");
                releaser.join().expect("writer released");
            });
        });
        let lines = json_lines(&output);
        assert_eq!(lines.len(), queued + 2);
        for (sequence, line) in lines[..=queued].iter().enumerate() {
            assert_eq!(line["sequence"], sequence);
        }
        assert_eq!(
            lines[queued + 1]["audit.event"],
            "telemetry.console.dropped"
        );
        assert_eq!(lines[queued + 1]["console.dropped.lines"], 7);
        assert_eq!(dropped.load(super::Ordering::Relaxed), 7);
    }

    #[test]
    fn console_event_arrays_are_json_arrays_without_a_trace_exporter() {
        use tracing_subscriber::{layer::SubscriberExt as _, registry};
        let output = Output::default();
        let writer = output.clone();
        let console = tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .event_format(super::CorrelatedJson(
                tracing_subscriber::fmt::format().json().flatten_event(true),
            ))
            .with_writer(move || writer.clone());
        tracing::subscriber::with_default(registry().with(console), || {
            crate::with_event_string_array("github.deliveries", &["first", "second"], || {
                tracing::info!(outcome = "queued", "admission")
            });
            tracing::info!(outcome = "ignored", "next");
        });
        let lines = json_lines(&output);
        assert_eq!(
            lines[0]["github.deliveries"],
            serde_json::json!(["first", "second"])
        );
        assert!(lines[1].get("github.deliveries").is_none());
    }

    fn wait_until_console_is_closed(state: &Mutex<super::ConsoleState>) {
        loop {
            let closed = match &*state.lock() {
                super::ConsoleState::Queued(_) => false,
                super::ConsoleState::Closed => true,
                super::ConsoleState::Direct(_) => panic!("writer released before shutdown"),
            };
            if closed {
                return;
            }
            std::thread::yield_now();
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
                let text = String::from_utf8(output.0.lock().clone()).expect("UTF-8 JSON");
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
            self.0.lock().push(event.metadata().target().to_owned());
        }
    }

    #[test]
    fn native_array_audits_pass_the_log_filter_without_exporting_other_payloads() {
        let recorded = RecordTargets::default();
        let subscriber = registry().with(
            recorded
                .clone()
                .with_filter(super::log_filter("job=debug,meter=info")),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "dekopon_gatewayd::audit", {audit.event = "gateway.message.received"}, "inbound");
            crate::with_event_string_array("github.deliveries", &["first"], || {
                tracing::info!(target: "dekopon_gatewayd::audit", {audit.event = "gateway.admission"}, "admission");
                tracing::error!(target: "opentelemetry", {audit.event = "gateway.admission"}, "must not reexport");
            });
        });
        assert_eq!(*recorded.0.lock(), vec!["dekopon_gatewayd::audit"]);
    }

    #[test]
    fn an_otlp_layer_never_sees_the_exporters_own_records() {
        for directive in [
            "dekopon_gatewayd=trace",
            "dekopon_gatewayd=trace,opentelemetry=off",
            "trace",
        ] {
            let recorded = RecordTargets::default();
            let subscriber = registry().with(recorded.clone().with_filter(otlp_filter(directive)));

            tracing::subscriber::with_default(subscriber, || {
                tracing::error!(target: "opentelemetry", "api diagnostic");
                tracing::error!(target: "opentelemetry-sdk", "sdk diagnostic");
                tracing::error!(target: "opentelemetry-otlp", "exporter diagnostic");
                tracing::info!(target: "dekopon_gatewayd::audit", "gateway event");
            });

            assert_eq!(
                *recorded.0.lock(),
                vec!["dekopon_gatewayd::audit".to_owned()],
                "{directive}"
            );
        }
    }

    #[test]
    fn a_guard_without_exporters_shuts_down_cleanly() {
        let guard = TelemetryGuard {
            console_worker: None,
            tracer_provider: None,
            logger_provider: None,
            shutdown_timeout: None,
        };
        assert!(guard.shutdown().is_ok());
    }

    #[test]
    fn a_stalled_exporter_times_out_at_the_configured_shutdown_timeout() {
        use opentelemetry::trace::{Tracer as _, TracerProvider as _};
        use opentelemetry_sdk::{
            error::{OTelSdkError, OTelSdkResult},
            trace::{
                BatchSpanProcessor, SdkTracerProvider, Span, SpanData, SpanExporter, SpanProcessor,
            },
        };
        use std::{sync::mpsc, time::Duration};

        #[derive(Debug)]
        struct Stalled(Mutex<mpsc::Receiver<()>>);

        impl SpanExporter for Stalled {
            async fn export(&self, _: Vec<SpanData>) -> OTelSdkResult {
                self.0.lock().recv().ok();
                Ok(())
            }
        }

        #[derive(Debug)]
        struct Observed {
            batch: BatchSpanProcessor,
            timed_out: Arc<Mutex<Option<Duration>>>,
        }

        impl SpanProcessor for Observed {
            fn on_start(&self, span: &mut Span, context: &opentelemetry::Context) {
                self.batch.on_start(span, context);
            }
            fn on_end(&self, span: SpanData) {
                self.batch.on_end(span);
            }
            fn force_flush(&self) -> OTelSdkResult {
                self.batch.force_flush()
            }
            fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
                let result = self.batch.shutdown_with_timeout(timeout);
                if let Err(OTelSdkError::Timeout(waited)) = &result {
                    *self.timed_out.lock() = Some(*waited);
                }
                result
            }
        }

        let (release, stalled) = mpsc::channel();
        let release = Arc::new(Mutex::new(Some(release)));
        let timed_out = Arc::new(Mutex::new(None));
        let provider = SdkTracerProvider::builder()
            .with_span_processor(Observed {
                batch: BatchSpanProcessor::builder(Stalled(Mutex::new(stalled))).build(),
                timed_out: Arc::clone(&timed_out),
            })
            .build();
        provider.tracer("stalled").start("queued");

        let (finished, watching) = mpsc::channel::<()>();
        let watchdog = std::thread::spawn({
            let release = Arc::clone(&release);
            move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) =
                    watching.recv_timeout(Duration::from_secs(2))
                    && let Some(release) = release.lock().take()
                {
                    release.send(()).ok();
                }
            }
        });
        let timeout = Duration::from_millis(10);
        let stopped = TelemetryGuard {
            console_worker: None,
            tracer_provider: Some(provider),
            logger_provider: None,
            shutdown_timeout: Some(timeout),
        }
        .shutdown();
        let released_after = release.lock().take();
        finished.send(()).ok();
        watchdog.join().unwrap();

        let release =
            released_after.expect("shutdown waited until the watchdog freed the exporter");
        assert!(stopped.is_err());
        assert_eq!(*timed_out.lock(), Some(timeout));
        release.send(()).ok();
    }
}
