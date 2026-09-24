//! Rust guest facade for the `dekopon:clock@1.0.0` component interface.
//!
//! This crate contains no clock. [`now_unix_millis`] calls a host import that only the broker host
//! implements, and only while it runs an authorized `invoke`.

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

/// Reads the broker host's wall clock: milliseconds since 1970-01-01T00:00:00Z.
///
/// Call it from `invoke` only. `describe` and `run-command` are pure by
/// contract, and the broker host traps a component that reads the clock from any of them.
#[must_use]
pub fn now_unix_millis() -> u64 {
    bindings::dekopon::clock::wall::now_unix_millis()
}
