mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "clock-client",
        generate_all,
    });
}

/// Call only from invoke; the broker host traps a component that reads the clock from describe or
/// run-command, which must stay pure.
#[must_use]
pub fn now_unix_millis() -> u64 {
    bindings::dekopon::clock::wall::now_unix_millis()
}

mod monotonic_bindings {
    wit_bindgen::generate!({ path: "wit", world: "monotonic-client", generate_all });
}

pub(crate) fn now_nanos() -> u64 {
    monotonic_bindings::dekopon::clock::monotonic::now_nanos()
}
