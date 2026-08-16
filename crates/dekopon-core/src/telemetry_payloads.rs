//! Process-wide opt-in for payload-bearing span fields.
//!
//! Span verbosity is an operator deployment choice, not a per-call one: it says whether this
//! process's telemetry sink is in scope for the data the process handles. Threading that answer
//! through every host, policy, and HTTP constructor would put a boolean in a dozen signatures to
//! express one fact about the environment, so it lives here as process state instead.
//!
//! It lives in `dekopon-core` rather than `dekopon-telemetry` so the broker, broker host, and HTTP
//! host can consult it without taking a dependency on exporter machinery they otherwise have no
//! reason to link.
//!
//! # What this does and does not widen
//!
//! Enabling payloads adds provider input and output, and HTTP URL paths and queries, to spans.
//! It never unwraps a [`crate::Redacted`] value: that type renders its marker on every path, so a
//! credential stays redacted whichever mode the process runs in. Verbosity is about data the
//! operator has accepted retention for; credentials are not that data.

use std::sync::atomic::{AtomicBool, Ordering};

static SPAN_PAYLOADS: AtomicBool = AtomicBool::new(false);

/// Enables or disables payload-bearing span fields for this process.
///
/// Call once during startup, before serving. Defaults to disabled, so a process that never calls
/// this emits metadata-only spans.
pub fn set_telemetry_payloads(enabled: bool) {
    SPAN_PAYLOADS.store(enabled, Ordering::Relaxed);
}

/// Reports whether payload-bearing span fields are enabled.
#[must_use]
pub fn telemetry_payloads() -> bool {
    SPAN_PAYLOADS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{set_telemetry_payloads, telemetry_payloads};

    /// Metadata-only is the default, so forgetting to configure telemetry cannot widen what a
    /// process emits.
    #[test]
    fn payloads_are_disabled_until_enabled() {
        assert!(!telemetry_payloads());
        set_telemetry_payloads(true);
        assert!(telemetry_payloads());
        set_telemetry_payloads(false);
        assert!(!telemetry_payloads());
    }
}
