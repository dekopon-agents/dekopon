#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "clock-client",
        generate_all,
    });
}

#[cfg(feature = "monotonic")]
mod monotonic_bindings {
    wit_bindgen::generate!({
        path: "wit/monotonic",
        world: "monotonic-client",
        generate_all,
    });
}

#[cfg(feature = "monotonic")]
#[must_use]
pub fn now_nanos() -> u64 {
    monotonic_bindings::dekopon::clock::monotonic::now_nanos()
}

/// Call only from invoke; the broker host traps a component that reads the clock from describe or
/// run-command, which must stay pure.
#[must_use]
pub fn now_unix_millis() -> u64 {
    bindings::dekopon::clock::wall::now_unix_millis()
}
