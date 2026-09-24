use std::sync::{Arc, Mutex};

use tracing::{
    Metadata,
    field::{Field, Visit},
    subscriber::Interest,
};
use tracing_subscriber::{layer::Context, registry::LookupSpan};

#[derive(Clone, Debug)]
pub enum Record {
    Event {
        level: &'static str,
        target: String,
        fields: String,
        parent: Option<String>,
        scope: Vec<&'static str>,
    },
    Span {
        name: &'static str,
        fields: String,
        parent: Option<String>,
    },
}

#[derive(Clone, Default)]
pub struct CaptureLayer {
    records: Arc<Mutex<Vec<Record>>>,
    prefix: Option<&'static str>,
}

impl CaptureLayer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn workspace() -> Self {
        Self::with_target_prefix("dekopon")
    }

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

    #[must_use]
    pub fn records(&self) -> Vec<Record> {
        self.records.lock().expect("capture sink").clone()
    }

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

    #[must_use]
    pub fn spans(&self) -> Vec<(&'static str, String)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Span { name, fields, .. } => Some((name, fields)),
                Record::Event { .. } => None,
            })
            .collect()
    }

    #[must_use]
    pub fn span_parents(&self) -> Vec<(&'static str, Option<String>)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Span { name, parent, .. } => Some((name, parent)),
                Record::Event { .. } => None,
            })
            .collect()
    }

    #[must_use]
    pub fn events_text(&self) -> String {
        render(
            self.records()
                .iter()
                .filter(|record| matches!(record, Record::Event { .. })),
        )
    }

    #[must_use]
    pub fn spans_text(&self) -> String {
        render(
            self.records()
                .iter()
                .filter(|record| matches!(record, Record::Span { .. })),
        )
    }

    #[must_use]
    pub fn text(&self) -> String {
        render(self.records().iter())
    }

    #[must_use]
    pub fn take_events(&self) -> String {
        let drained = std::mem::take(&mut *self.records.lock().expect("capture sink"));
        render(
            drained
                .iter()
                .filter(|record| matches!(record, Record::Event { .. })),
        )
    }

    #[must_use]
    pub fn saw(&self, marker: &str) -> bool {
        self.events_text().contains(marker)
    }

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
            Record::Span { name, fields, .. } => {
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
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if self.interested(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        self.interested(metadata)
    }

    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        let mut fields = String::new();
        attributes.record(&mut Visitor(&mut fields));
        self.push(Record::Span {
            name: attributes.metadata().name(),
            fields,
            parent: context
                .span(id)
                .and_then(|span| span.parent())
                .map(|parent| parent.metadata().name().to_owned()),
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
            parent: span
                .parent()
                .map(|parent| parent.metadata().name().to_owned()),
        });
    }

    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        let mut fields = String::new();
        event.record(&mut Visitor(&mut fields));
        let attributed = context.event_span(event);
        self.push(Record::Event {
            level: event.metadata().level().as_str(),
            target: event.metadata().target().to_owned(),
            fields,
            parent: attributed
                .as_ref()
                .map(|span| span.metadata().name().to_owned()),
            scope: attributed.map_or_else(Vec::new, |span| {
                span.scope()
                    .from_root()
                    .map(|ancestor| ancestor.metadata().name())
                    .collect()
            }),
        });
    }
}

/// This renders every field including Debug ones, since a redaction test checking only expected
/// fields would miss a new one leaking a secret.
struct Visitor<'a>(&'a mut String);

impl Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}
