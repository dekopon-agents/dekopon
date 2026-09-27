#![forbid(unsafe_code)]

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "random-client",
        generate_all,
    });
}

pub const MAX_RANDOM_BYTES: u32 = 4096;

/// Call only during invoke; the host refuses larger requests and metadata-phase reads.
#[must_use]
pub fn get_random_bytes(length: u32) -> Vec<u8> {
    bindings::dekopon::random::source::get_random_bytes(length)
}
