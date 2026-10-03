pub(crate) fn fill_chunks(out: &mut [u8], mut read: impl FnMut(&mut [u8])) {
    for chunk in out.chunks_mut(4096) {
        read(chunk);
    }
}

#[cfg(target_arch = "wasm32")]
mod bindings {
    wit_bindgen::generate!({ path: "wit", world: "random-client", generate_all });
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn read(out: &mut [u8]) {
    let bytes = bindings::dekopon::random::source::get_random_bytes(out.len() as u32);
    // The host returns exactly `length` bytes or ends the invocation; a mismatch cannot happen and would trap here.
    out.copy_from_slice(&bytes);
}

#[cfg(test)]
mod tests {
    use super::fill_chunks;

    #[test]
    fn large_buffers_are_filled_in_bounded_chunks() {
        let mut buffer = vec![0; 9000];
        let mut lengths = Vec::new();
        fill_chunks(&mut buffer, |chunk| {
            lengths.push(chunk.len());
            chunk.fill(42);
        });
        assert_eq!(lengths, [4096, 4096, 808]);
        assert!(buffer.iter().all(|byte| *byte == 42));
    }
}
