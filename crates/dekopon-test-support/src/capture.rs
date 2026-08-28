//! One `tracing` capture layer, in place of nine that agreed only on how they rendered a field.
//!
//! The nine copies differed on the two things that matter and on nothing else: whether they kept
//! spans as well as events, and whether they refused callsites outside this workspace. The second
//! is not cosmetic — a test binary that compiles a real Wasm component captures Cranelift's own
//! instrumentation event by event, which both drowns the assertions and makes them depend on a
//! dependency's logging.

use std::sync::{Arc, Mutex};

use tracing::{
    Metadata,
    field::{Field, Visit},
    subscriber::Interest,
};
use tracing_subscriber::{layer::Context, registry::LookupSpan};

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
