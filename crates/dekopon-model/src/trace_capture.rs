//! Per-test trace capture; no global subscriber or process state.

use std::sync::{Arc, Mutex};
use tracing::{
    Subscriber,
    field::{Field, Visit},
    span::{Attributes, Record},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

#[derive(Clone, Default)]
pub(crate) struct TraceCapture(Arc<Mutex<String>>);

impl TraceCapture {
    pub(crate) fn subscriber(&self) -> impl Subscriber + Send + Sync + 'static {
        tracing_subscriber::registry().with(self.clone())
    }

    pub(crate) fn text(&self) -> String {
        self.0.lock().unwrap().clone()
    }

    pub(crate) fn field(&self, name: &str) -> Option<String> {
        let prefix = format!("{name}=");
        self.text()
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
    }

    pub(crate) fn assert_exchange(&self, backend: &str, dialect: &str) {
        assert_eq!(self.text().matches("model.complete").count(), 1);
        assert_eq!(self.field("model.backend"), Some(format!("{backend:?}")));
        assert_eq!(self.field("model.dialect"), Some(format!("{dialect:?}")));
        assert_eq!(self.field("outcome").as_deref(), Some("\"success\""));
        assert_eq!(self.field("http.status").as_deref(), Some("200"));
        for field in [
            "timing.total_ms",
            "timing.headers_ms",
            "response.bytes",
            "tool_call.count",
        ] {
            assert!(self.field(field).unwrap().parse::<u64>().is_ok(), "{field}");
        }
        if let Some(first) = self.field("timing.first_event_ms") {
            let first = first.parse::<u64>().unwrap();
            assert!(
                first
                    >= self
                        .field("timing.headers_ms")
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
            );
            assert!(
                first
                    <= self
                        .field("timing.total_ms")
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
            );
        }
        assert!(self.field("error.kind").is_none());
        assert!(self.field("error.phase").is_none());
    }
}

impl Visit for TraceCapture {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        writeln!(self.0.lock().unwrap(), "{}={value:?}", field.name()).unwrap();
    }
}

impl<S: Subscriber> Layer<S> for TraceCapture {
    fn on_new_span(&self, attributes: &Attributes<'_>, _: &tracing::Id, _: Context<'_, S>) {
        self.0
            .lock()
            .unwrap()
            .push_str(&format!("{}\n", attributes.metadata().name()));
        attributes.record(&mut self.clone());
    }

    fn on_record(&self, _: &tracing::Id, values: &Record<'_>, _: Context<'_, S>) {
        values.record(&mut self.clone());
    }

    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        event.record(&mut self.clone());
    }
}
