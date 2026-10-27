use super::*;

#[tokio::test]
async fn a_memory_persist_span_reaches_the_installed_trace_pipeline() {
    if std::env::var_os("DEKOPON_TEST_MEMORY_PERSIST_SUBSCRIBER").is_none() {
        let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tests::memory::a_memory_persist_span_reaches_the_installed_trace_pipeline",
            ])
            .env("DEKOPON_TEST_MEMORY_PERSIST_SUBSCRIBER", "1")
            .output()
            .expect("isolated subscriber test");
        assert!(
            child.status.success(),
            "subscriber test: stdout={} stderr={}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        return;
    }
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
    let telemetry = dekopon_telemetry::Install::new(dekopon_telemetry::Console {
        format: dekopon_telemetry::ConsoleFormat::Json,
        writer: dekopon_telemetry::ConsoleWriter::Stderr,
        filter: dekopon_telemetry::ConsoleFilter::Directive("info".into()),
    })
    .with_traces(provider, "persist-test", crate::OTEL_TRACE_FILTER)
    .with_logs(
        opentelemetry_sdk::logs::SdkLoggerProvider::builder().build(),
        "job=debug,meter=info",
    )
    .install()
    .expect("sole global test subscriber");
    let directory = temporary();
    let journal = temporary();
    let (broker, _) = stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let model = ModelScript::new([answer("reply delivered")]);
    let runner = journaled_runner(
        broker,
        Arc::new(Arc::clone(&model)) as Arc<dyn ModelFactory>,
        journal.path(),
    );
    let route = persistent_route(model_config(), recall_window(RecallSource::Journal));
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        runner,
        route,
        message("persist after delivery"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(driver.replies(), ["reply delivered"]);
    telemetry.shutdown().expect("exporters flushed");

    let spans = exported.0.lock().clone();
    let named = |name: &str| {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("{name} exported"))
    };
    let (message, session, persist) = (
        named("gateway.message"),
        named("gateway.session"),
        named("memory.persist"),
    );
    assert_eq!(session.parent_span_id, message.span_context.span_id());
    assert_eq!(persist.parent_span_id, message.span_context.span_id());
    assert!(session.end_time <= persist.start_time);
    assert!(persist.end_time <= message.end_time);
    let delivered = session
        .events
        .iter()
        .find(|event| event.name == "reply.delivered")
        .expect("reply.delivered on gateway.session");
    assert!(
        delivered
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "outcome"
                && attribute.value.as_str() == "answered")
    );
}
