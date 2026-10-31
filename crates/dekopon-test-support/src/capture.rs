use parking_lot::Mutex;
use std::{fmt, sync::Arc};

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
        fields: Fields,
        parent: Option<String>,
        scope: Vec<&'static str>,
    },
    Span {
        name: &'static str,
        fields: Fields,
        parent: Option<String>,
    },
}

impl Record {
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        match self {
            Self::Event { fields, .. } | Self::Span { fields, .. } => fields.get(name),
        }
    }

    #[must_use]
    pub fn parent(&self) -> Option<&str> {
        match self {
            Self::Event { parent, .. } | Self::Span { parent, .. } => parent.as_deref(),
        }
    }
}

#[derive(Clone, Default)]
pub struct Fields(Vec<(&'static str, Value)>);

#[derive(Clone)]
enum Value {
    Str(String),
    Formatted(String),
}

impl Fields {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| match value {
                Value::Str(value) | Value::Formatted(value) => value.as_str(),
            })
    }
}

impl fmt::Display for Fields {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, value) in &self.0 {
            match value {
                Value::Str(value) => write!(formatter, " {name}={value:?}")?,
                Value::Formatted(value) => write!(formatter, " {name}={value}")?,
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Fields {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.to_string(), formatter)
    }
}

const WORKSPACE_CATEGORIES: &[&str] = &[
    "gateway",
    "prompt",
    "model",
    "asset",
    "shell",
    "job",
    "broker",
    "provider",
    "http",
    "credential",
    "memory",
    "telemetry",
];

#[derive(Clone, Copy, Default)]
enum TargetFilter {
    #[default]
    All,
    Workspace,
}

#[derive(Clone, Default)]
pub struct CaptureLayer {
    records: Arc<Mutex<Vec<Record>>>,
    targets: TargetFilter,
}

impl CaptureLayer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn workspace() -> Self {
        Self {
            records: Arc::new(Mutex::new(Vec::new())),
            targets: TargetFilter::Workspace,
        }
    }

    fn interested(&self, metadata: &Metadata<'_>) -> bool {
        match self.targets {
            TargetFilter::All => true,
            TargetFilter::Workspace => {
                metadata.target().starts_with("dekopon")
                    || WORKSPACE_CATEGORIES.contains(&metadata.target())
            }
        }
    }

    fn push(&self, record: Record) {
        self.records.lock().push(record);
    }

    #[must_use]
    pub fn records(&self) -> Vec<Record> {
        self.records.lock().clone()
    }

    #[must_use]
    pub fn events(&self) -> Vec<(String, Option<String>)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Event { fields, parent, .. } => Some((fields.to_string(), parent)),
                Record::Span { .. } => None,
            })
            .collect()
    }

    #[must_use]
    pub fn spans(&self) -> Vec<(&'static str, String)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Span { name, fields, .. } => Some((name, fields.to_string())),
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
    pub fn events_where(&self, target: &str, fields: &[(&str, &str)]) -> Vec<Record> {
        self.records()
            .into_iter()
            .filter(|record| match record {
                Record::Event { target: actual, .. } => {
                    actual
                        .strip_prefix(target)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
                        && fields
                            .iter()
                            .all(|(name, value)| record.field(name) == Some(*value))
                }
                Record::Span { .. } => false,
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
        let drained = std::mem::take(&mut *self.records.lock());
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
        self.records.lock().clear();
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
                output.push_str(&fields.to_string());
            }
            Record::Span { name, fields, .. } => {
                output.push_str(name);
                output.push_str(&fields.to_string());
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
        let mut fields = Fields::default();
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
        let mut fields = Fields::default();
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
        let mut fields = Fields::default();
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

struct Visitor<'a>(&'a mut Fields);

impl Visit for Visitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.0.push((field.name(), Value::Str(value.to_owned())));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .0
            .push((field.name(), Value::Formatted(format!("{value:?}"))));
    }
}

#[cfg(test)]
mod tests {
    use super::{CaptureLayer, Record};
    use tracing_subscriber::layer::SubscriberExt as _;

    #[test]
    fn workspace_capture_includes_categories_but_not_third_party_targets() {
        let capture = CaptureLayer::workspace();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(capture.clone()),
            || {
                tracing::info!(target: "dekopon_broker_host", "workspace");
                tracing::info!(target: "gateway", "category");
                tracing::info!(target: "prompt", "category");
                tracing::info!(target: "model", "category");
                tracing::info!(target: "asset", "category");
                tracing::info!(target: "shell", "category");
                tracing::info!(target: "job", "category");
                tracing::info!(target: "broker", "category");
                tracing::info!(target: "provider", "category");
                tracing::info!(target: "http", "category");
                tracing::info!(target: "credential", "category");
                tracing::info!(target: "memory", "category");
                tracing::info!(target: "telemetry", "category");
                tracing::info!(target: "http::client", "third-party");
                tracing::info!(target: "wasmtime", "third-party");
            },
        );
        let targets = capture
            .records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Event { target, .. } => Some(target),
                Record::Span { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), super::WORKSPACE_CATEGORIES.len() + 1);
        assert!(targets.contains(&"dekopon_broker_host".to_owned()));
        for category in super::WORKSPACE_CATEGORIES {
            assert!(targets.iter().any(|target| target == category));
        }
    }

    #[test]
    fn a_field_reads_by_name_while_the_text_keeps_debug_quoting() {
        let capture = CaptureLayer::new();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(capture.clone()),
            || {
                tracing::info!(
                    target: "dekopon_gatewayd::session",
                    event = "gateway_recalled",
                    messages = 2_u64,
                    route = %"slack:C1",
                    path = ?"/tmp/x"
                );
            },
        );
        let records = capture.records();
        assert_eq!(
            ["event", "messages", "route", "path", "absent"].map(|name| records[0].field(name)),
            [
                Some("gateway_recalled"),
                Some("2"),
                Some("slack:C1"),
                Some("\"/tmp/x\""),
                None
            ]
        );
        assert_eq!(
            capture.text(),
            "INFO dekopon_gatewayd::session event=\"gateway_recalled\" messages=2 route=slack:C1 \
             path=\"/tmp/x\"\n"
        );
    }

    #[test]
    fn events_where_matches_a_target_by_whole_module_path_segments() {
        let capture = CaptureLayer::new();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(capture.clone()),
            || {
                tracing::info!(target: "dekopon_gatewayd::session", event = "gateway_recalled");
                tracing::info!(target: "dekopon_gatewayd", event = "gateway_recalled");
                tracing::info!(target: "dekopon_gatewayd_extra", event = "gateway_recalled");
                tracing::info!(target: "dekopon_gatewayd", event = "gateway_other");
            },
        );
        let targets = |target| {
            capture
                .events_where(target, &[("event", "gateway_recalled")])
                .into_iter()
                .map(|record| match record {
                    Record::Event { target, .. } => target,
                    Record::Span { name, .. } => name.to_owned(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            targets("dekopon_gatewayd"),
            ["dekopon_gatewayd::session", "dekopon_gatewayd"]
        );
        assert_eq!(
            targets("dekopon_gatewayd::session"),
            ["dekopon_gatewayd::session"]
        );
    }
}
