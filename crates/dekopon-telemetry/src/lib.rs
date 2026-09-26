//! Ingest credentials are read only from OTEL_EXPORTER_OTLP_HEADERS by the SDK itself; this crate
//! never accepts a token as an argument or attaches one to a span or log field.

#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
#[cfg(feature = "exporter")]
mod exporter;
#[cfg(feature = "exporter")]
mod install;

use opentelemetry::{
    Context,
    trace::{SpanContext, TraceContextExt as _, TraceFlags, TraceState},
};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

#[cfg(feature = "exporter")]
pub use exporter::{CA_CERTIFICATE_ENV, ExporterSettings, TelemetryError, Transport};
#[cfg(feature = "exporter")]
pub use install::{
    Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, Install, InstallError, ShutdownError,
    TelemetryGuard, optional_logger_provider, optional_tracer_provider,
};

pub fn link_span(execution: &tracing::Span, receipt: &tracing::Span) {
    let context = receipt.context();
    let span = context.span();
    if span.span_context().is_valid() {
        execution.add_link(span.span_context().clone());
    }
}

pub fn link_remote(execution: &tracing::Span, parts: TraceContextParts) {
    let context = remote_context(parts);
    let span = context.span();
    execution.add_link(span.span_context().clone());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceContextParts {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub flags: u8,
}

#[must_use]
pub fn current_trace_context() -> Option<TraceContextParts> {
    let context = tracing::Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    if !span_context.is_valid() {
        return None;
    }
    Some(TraceContextParts {
        trace_id: span_context.trace_id().to_bytes(),
        span_id: span_context.span_id().to_bytes(),
        flags: span_context.trace_flags().to_u8(),
    })
}

#[must_use]
pub fn remote_context(parts: TraceContextParts) -> Context {
    let span_context = SpanContext::new(
        opentelemetry::trace::TraceId::from_bytes(parts.trace_id),
        opentelemetry::trace::SpanId::from_bytes(parts.span_id),
        TraceFlags::new(parts.flags),
        true,
        TraceState::default(),
    );
    Context::new().with_remote_span_context(span_context)
}

#[cfg(test)]
mod tests {
    use super::{TraceContextParts, remote_context};
    use opentelemetry::trace::TraceContextExt as _;

    #[test]
    fn remote_context_preserves_identifiers_and_marks_them_remote() {
        let parts = TraceContextParts {
            trace_id: [
                0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e,
                0x47, 0x36,
            ],
            span_id: [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7],
            flags: 1,
        };

        let context = remote_context(parts);
        let span_context = context.span().span_context().clone();

        assert!(span_context.is_valid());
        assert!(span_context.is_remote());
        assert_eq!(span_context.trace_id().to_bytes(), parts.trace_id);
        assert_eq!(span_context.span_id().to_bytes(), parts.span_id);
        assert_eq!(span_context.trace_flags().to_u8(), 1);
    }
}

#[cfg(test)]
mod aggregation_tests {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        trace::{SdkTracerProvider, SpanData, SpanExporter},
    };
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Debug, Default)]
    struct Exported(Arc<Mutex<Vec<SpanData>>>);
    impl SpanExporter for Exported {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().expect("exported spans").extend(batch);
            Ok(())
        }
    }

    #[test]
    fn collected_execution_exports_lead_parent_and_every_constituent_link() {
        let exported = Exported::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exported.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("aggregation-test")));
        tracing::subscriber::with_default(subscriber, || {
            let lead = tracing::info_span!(parent: None, "receipt.lead");
            let second = tracing::info_span!(parent: None, "receipt.second");
            let third = tracing::info_span!(parent: None, "receipt.third");
            let execution = tracing::info_span!(parent: &lead, "gateway.message");
            for receipt in [&lead, &second, &third] {
                super::link_span(&execution, receipt);
            }
        });
        provider.force_flush().expect("flush");
        let spans = exported.0.lock().expect("exported spans");
        let execution = spans
            .iter()
            .find(|span| span.name == "gateway.message")
            .expect("execution exported");
        let receipts: Vec<_> = spans
            .iter()
            .filter(|span| span.name.starts_with("receipt."))
            .collect();
        assert_eq!(receipts.len(), 3);
        assert_eq!(execution.links.links.len(), 3);
        for receipt in &receipts {
            assert!(
                execution
                    .links
                    .links
                    .iter()
                    .any(|link| link.span_context == receipt.span_context)
            );
        }
        let lead = receipts
            .iter()
            .find(|span| span.name == "receipt.lead")
            .expect("lead");
        assert_eq!(execution.parent_span_id, lead.span_context.span_id());
        assert_eq!(
            execution.span_context.trace_id(),
            lead.span_context.trace_id()
        );
        drop(spans);
        provider.shutdown().expect("shutdown");
    }
}
