//! One `tracing` capture layer, in place of nine that agreed only on how they rendered a field.
//!
//! The nine copies differed on the two things that matter and on nothing else: whether they kept
//! spans as well as events, and whether they refused callsites outside this workspace. The second
//! is not cosmetic — a test binary that compiles a real Wasm component captures Cranelift's own
//! instrumentation event by event, which both drowns the assertions and makes them depend on a
//! dependency's logging.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    thread::ThreadId,
};

use tracing::{
    Metadata,
    field::{Field, Visit},
    subscriber::Interest,
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// One captured span or event, flattened to the strings a collector would receive.
#[derive(Clone, Debug)]
pub enum Record {
    /// One event, with the span the subscriber attributed it to.
    Event {
        /// The event's level, as `INFO`/`WARN`/…
        level: &'static str,
        /// The emitting callsite's target.
        target: String,
        /// Every field rendered as ` name=value`.
        fields: String,
        /// The enclosing span's name, when there was one.
        parent: Option<String>,
    },
    /// One span, at creation or when a later field was recorded onto it.
    Span {
        /// The span's name.
        name: &'static str,
        /// Every field rendered as ` name=value`.
        fields: String,
    },
}

/// Captures every span and event a subscriber offers it, optionally filtered by target prefix.
#[derive(Clone, Default)]
pub struct CaptureLayer {
    records: Arc<Mutex<Vec<Record>>>,
    prefix: Option<&'static str>,
}

impl CaptureLayer {
    /// Captures every callsite the subscriber offers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Captures only this workspace's callsites.
    #[must_use]
    pub fn workspace() -> Self {
        Self::with_target_prefix("dekopon")
    }

    /// Captures only callsites whose target begins with `prefix`.
    #[must_use]
    pub fn with_target_prefix(prefix: &'static str) -> Self {
        Self {
            records: Arc::new(Mutex::new(Vec::new())),
            prefix: Some(prefix),
        }
    }

    fn interested(&self, metadata: &Metadata<'_>) -> bool {
        self.prefix
            .is_none_or(|prefix| metadata.target().starts_with(prefix))
    }

    fn push(&self, record: Record) {
        self.records.lock().expect("capture sink").push(record);
    }

    /// Every record captured so far, in arrival order.
    #[must_use]
    pub fn records(&self) -> Vec<Record> {
        self.records.lock().expect("capture sink").clone()
    }

    /// Every event's rendered fields paired with the span it was attributed to.
    #[must_use]
    pub fn events(&self) -> Vec<(String, Option<String>)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Event { fields, parent, .. } => Some((fields, parent)),
                Record::Span { .. } => None,
            })
            .collect()
    }

    /// Every span's name paired with its rendered fields.
    #[must_use]
    pub fn spans(&self) -> Vec<(&'static str, String)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Span { name, fields } => Some((name, fields)),
                Record::Event { .. } => None,
            })
            .collect()
    }

    /// Every event as one `LEVEL target field=value…` line.
    #[must_use]
    pub fn events_text(&self) -> String {
        render(
            self.records()
                .iter()
                .filter(|record| matches!(record, Record::Event { .. })),
        )
    }

    /// Every span as one `name field=value…` line.
    #[must_use]
    pub fn spans_text(&self) -> String {
        render(
            self.records()
                .iter()
                .filter(|record| matches!(record, Record::Span { .. })),
        )
    }

    /// Every record, spans and events together, in arrival order.
    #[must_use]
    pub fn text(&self) -> String {
        render(self.records().iter())
    }

    /// Drains the capture and returns what every event in it rendered to.
    #[must_use]
    pub fn take_events(&self) -> String {
        let drained = std::mem::take(&mut *self.records.lock().expect("capture sink"));
        render(
            drained
                .iter()
                .filter(|record| matches!(record, Record::Event { .. })),
        )
    }

    /// Whether any event captured so far rendered `marker`.
    #[must_use]
    pub fn saw(&self, marker: &str) -> bool {
        self.events_text().contains(marker)
    }

    /// Discards everything captured so far.
    pub fn clear(&self) {
        self.records.lock().expect("capture sink").clear();
    }

    /// Captures this thread's workspace callsites through the process-global subscriber.
    ///
    /// This is the only correct way for a test that runs beside others in the same binary to
    /// capture `tracing`; see [`CaptureSession`] for why a scoped dispatcher is not.
    #[must_use]
    pub fn install() -> CaptureSession {
        Self::install_with_target_prefix(GLOBAL_TARGET_PREFIX)
    }

    /// The same, narrowed to callsites whose target begins with `prefix`.
    ///
    /// # Panics
    ///
    /// When `prefix` does not itself begin with `dekopon`: the global subscriber refuses every
    /// other target outright, so such a capture could only ever be empty.
    #[must_use]
    pub fn install_with_target_prefix(prefix: &'static str) -> CaptureSession {
        assert!(
            prefix.starts_with(GLOBAL_TARGET_PREFIX),
            "a capture prefix must begin with {GLOBAL_TARGET_PREFIX:?}; the global capture \
             subscriber refuses every other target"
        );
        install_global_subscriber();
        let thread = std::thread::current().id();
        let layer = Self::with_target_prefix(prefix);
        let previous = routes()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(thread, layer.clone());
        assert!(
            previous.is_none(),
            "a CaptureSession for this thread is still alive; one capture per thread"
        );
        CaptureSession { layer, thread }
    }
}

/// The one target prefix the process-global capture subscriber will consider at all.
const GLOBAL_TARGET_PREFIX: &str = "dekopon";

/// One test's capture, routed to it by thread from the process-global subscriber.
///
/// `tracing` caches interest per callsite for the whole process, and its single-dispatch fast path
/// registers a callsite against whatever dispatcher the *first* thread to reach it had. A test that
/// installs a scoped dispatcher (`with_default`, `with_subscriber`) therefore races every sibling
/// test in the same binary: a sibling reaching the callsite first, with no dispatcher, caches
/// `Interest::never()`, and the capturing test fails with `missing <event>` only under parallel
/// load. `rebuild_interest_cache` does not close the race, because the sibling can re-register
/// afterwards.
///
/// The fix is one dispatcher for the process, installed once and never replaced, so a cached
/// interest stays correct: this session installs it (the first caller in the binary does) and
/// registers the calling thread in a routing table. Events are attributed to the thread that
/// emitted them, so sibling tests stay parallel and cannot contaminate each other's records.
/// Dropping the session unregisters the thread.
///
/// A capture is therefore per *thread*, not per task: drive it from a `#[test]`, or from a
/// `#[tokio::test]` on the default current-thread runtime, and do not spawn the work under test
/// onto another thread.
pub struct CaptureSession {
    layer: CaptureLayer,
    thread: ThreadId,
}

impl std::ops::Deref for CaptureSession {
    type Target = CaptureLayer;
    fn deref(&self) -> &CaptureLayer {
        &self.layer
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        routes()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.thread);
    }
}

fn routes() -> &'static Mutex<HashMap<ThreadId, CaptureLayer>> {
    static ROUTES: OnceLock<Mutex<HashMap<ThreadId, CaptureLayer>>> = OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn route() -> Option<CaptureLayer> {
    routes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&std::thread::current().id())
        .cloned()
}

fn install_global_subscriber() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        use tracing_subscriber::layer::SubscriberExt as _;
        let subscriber = tracing_subscriber::registry().with(RoutingLayer);
        if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
            panic!(
                "CaptureLayer::install owns this test binary's global subscriber, and something \
                 else set one first: {error}"
            );
        }
        // Callsites a sibling test reached before this point were registered against the default
        // no-op dispatcher. Rebuilding is sound exactly because this dispatcher is now permanent.
        tracing::callsite::rebuild_interest_cache();
    });
}

/// Routes each span and event to the capture registered for the emitting thread, if any.
struct RoutingLayer;

impl<S> Layer<S> for RoutingLayer
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    /// The process-wide answer, so it is safe to cache: a target outside this workspace is never
    /// captured by any session, and one inside it is offered to whichever session owns the thread.
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if metadata.target().starts_with(GLOBAL_TARGET_PREFIX) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        metadata.target().starts_with(GLOBAL_TARGET_PREFIX)
    }

    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        if let Some(layer) = route()
            && layer.interested(attributes.metadata())
        {
            <CaptureLayer as Layer<S>>::on_new_span(&layer, attributes, id, context);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        if let Some(layer) = route()
            && context
                .span(id)
                .is_some_and(|span| layer.interested(span.metadata()))
        {
            <CaptureLayer as Layer<S>>::on_record(&layer, id, values, context);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        if let Some(layer) = route()
            && layer.interested(event.metadata())
        {
            <CaptureLayer as Layer<S>>::on_event(&layer, event, context);
        }
    }
}

fn render<'a>(records: impl Iterator<Item = &'a Record>) -> String {
    let mut output = String::new();
    for record in records {
        match record {
            Record::Event {
                level,
                target,
                fields,
                ..
            } => {
                output.push_str(level);
                output.push(' ');
                output.push_str(target);
                output.push_str(fields);
            }
            Record::Span { name, fields } => {
                output.push_str(name);
                output.push_str(fields);
            }
        }
        output.push('\n');
    }
    output
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    /// A binary that compiles a real component would otherwise capture Wasmtime's own trace
    /// instrumentation event by event. Interest is cached per callsite for the process.
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if self.interested(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    /// `Layer::enabled` is what actually turns a callsite off when the inner subscriber is a
    /// registry, so the filter has to live here rather than only in `register_callsite`.
    fn enabled(&self, metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        self.interested(metadata)
    }

    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _context: Context<'_, S>,
    ) {
        let mut fields = String::new();
        attributes.record(&mut Visitor(&mut fields));
        self.push(Record::Span {
            name: attributes.metadata().name(),
            fields,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut fields = String::new();
        values.record(&mut Visitor(&mut fields));
        self.push(Record::Span {
            name: span.metadata().name(),
            fields,
        });
    }

    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        let mut fields = String::new();
        event.record(&mut Visitor(&mut fields));
        self.push(Record::Event {
            level: event.metadata().level().as_str(),
            target: event.metadata().target().to_owned(),
            fields,
            parent: context
                .event_span(event)
                .map(|span| span.metadata().name().to_owned()),
        });
    }
}

/// Renders every field, including ones recorded as `Debug`.
///
/// Rendering everything is the point: a redaction test that inspected only the fields it expected
/// would not notice a new one carrying a secret. `record_str` is deliberately left to the default
/// forward to `record_debug`, so a string field renders quoted and a test can tell `outcome="x"`
/// from a field whose value merely contains `x`.
struct Visitor<'a>(&'a mut String);

impl Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::CaptureLayer;

    /// The property the routing table exists for: two tests capturing at the same moment on two
    /// threads each see their own events and none of the other's.
    #[test]
    fn two_captures_running_at_once_on_two_threads_do_not_cross_talk() {
        let barrier = Arc::new(Barrier::new(2));
        let threads = ["alpha", "beta"].map(|marker| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let capture = CaptureLayer::install();
                // Both captures are registered before either emits, so an event landing in the
                // wrong sink would be observed rather than merely possible.
                barrier.wait();
                for sequence in 0..64 {
                    tracing::info!(marker, sequence, "capture routing fixture");
                }
                barrier.wait();
                capture.events_text()
            })
        });
        let [alpha, beta] = threads.map(|thread| thread.join().expect("capture thread joins"));

        assert_eq!(alpha.lines().count(), 64, "{alpha}");
        assert_eq!(beta.lines().count(), 64, "{beta}");
        assert!(alpha.contains("marker=\"alpha\""), "{alpha}");
        assert!(!alpha.contains("beta"), "{alpha}");
        assert!(beta.contains("marker=\"beta\""), "{beta}");
        assert!(!beta.contains("alpha"), "{beta}");
    }

    /// A dropped session leaves no route behind, so a later test reusing the thread starts empty.
    #[test]
    fn a_dropped_session_stops_capturing_and_frees_the_thread() {
        {
            let capture = CaptureLayer::install();
            tracing::info!(marker = "first", "capture routing fixture");
            assert!(capture.saw("first"));
        }
        tracing::info!(marker = "between", "capture routing fixture");
        let capture = CaptureLayer::install();
        tracing::info!(marker = "second", "capture routing fixture");
        assert!(capture.saw("second"));
        assert!(!capture.saw("between"), "{}", capture.events_text());
    }

    #[test]
    #[should_panic(expected = "must begin with")]
    fn a_prefix_the_global_subscriber_refuses_is_rejected_at_install() {
        let _capture = CaptureLayer::install_with_target_prefix("wasmtime");
    }
}
