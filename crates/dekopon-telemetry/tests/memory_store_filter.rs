#![cfg(feature = "exporter")]

use std::sync::Arc;

use dekopon_telemetry::{Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, Install};
use opentelemetry_sdk::{
    error::OTelSdkResult,
    logs::{LogBatch, LogExporter, SdkLoggerProvider},
    trace::{SdkTracerProvider, SpanData, SpanExporter},
};
use parking_lot::Mutex;

#[derive(Clone, Debug, Default)]
struct Traces(Arc<Mutex<Vec<SpanData>>>);

impl SpanExporter for Traces {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().extend(batch);
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct Logs(Arc<Mutex<Vec<&'static str>>>);

impl LogExporter for Logs {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        for (record, _) in batch.iter() {
            self.0.lock().push(record.event_name().unwrap_or(""));
        }
        Ok(())
    }
}

#[test]
fn a_memory_store_after_the_detail_probe_reaches_traces_and_logs() {
    let filter = "dekopon_brokerd=trace,dekopon_broker=trace,dekopon_broker_host=trace,dekopon_http_host=trace,gateway=debug,prompt=debug,model=debug,asset=debug,shell=debug,job=debug,broker=debug,provider=debug,http=debug,credential=debug,memory=debug,telemetry=debug,hyper=off,h2=off,tonic=off,reqwest=off";
    let traces = Traces::default();
    let logs = Logs::default();
    let guard = Install::new(Console {
        format: ConsoleFormat::Json,
        writer: ConsoleWriter::Stderr,
        filter: ConsoleFilter::Directive("info".into()),
    })
    .with_traces(
        SdkTracerProvider::builder()
            .with_simple_exporter(traces.clone())
            .build(),
        "memory-store-test",
        filter,
    )
    .with_logs(
        SdkLoggerProvider::builder()
            .with_simple_exporter(logs.clone())
            .build(),
        filter,
    )
    .install()
    .expect("sole global test subscriber");

    tracing::info_span!(target: "provider", "provider").in_scope(|| {
        tracing::info!(
            name: "memory.store",
            target: "memory",
            { telemetry.detail = if tracing::enabled!(target: "memory", tracing::Level::TRACE) {
                "full"
            } else if tracing::enabled!(target: "memory", tracing::Level::DEBUG) {
                "standard"
            } else {
                "drip"
            },
            audit.event = "memory.store" },
            "memory.store"
        );
    });
    guard.shutdown().expect("exporters flushed");

    let trace_received = traces
        .0
        .lock()
        .iter()
        .any(|span| span.events.iter().any(|event| event.name == "memory.store"));
    let log_received = logs.0.lock().contains(&"memory.store");
    assert!(
        trace_received && log_received,
        "memory.store delivery: traces={trace_received}, logs={log_received}"
    );
}
