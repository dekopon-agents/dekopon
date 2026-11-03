#![cfg(feature = "exporter")]

use dekopon_telemetry::{Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, Install};

#[test]
fn ids_without_an_exporter_keep_log_at_the_console_level() {
    let guard = Install::new(Console {
        format: ConsoleFormat::Json,
        writer: ConsoleWriter::Stderr,
        filter: ConsoleFilter::Directive("warn".into()),
    })
    .with_trace_settings(None, "log-level-test", "dekopon_gatewayd=trace")
    .install()
    .expect("sole global test subscriber");
    assert_eq!(log::max_level(), log::LevelFilter::Warn);
    assert_eq!(
        tracing::level_filters::LevelFilter::current(),
        tracing::level_filters::LevelFilter::TRACE
    );
    guard.shutdown().expect("shut down");
}
