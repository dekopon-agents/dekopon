//! Content-free metric helpers.

/// Powers-of-two bucket for a byte count. Zero is its own bucket; no exact count is exported.
pub(crate) const fn byte_bucket(bytes: u64) -> u8 {
    if bytes == 0 {
        0
    } else {
        (64 - bytes.leading_zeros()) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::byte_bucket;

    #[test]
    fn buckets_do_not_preserve_exact_message_sizes() {
        assert_eq!(byte_bucket(0), 0);
        assert_eq!(byte_bucket(17), byte_bucket(31));
        assert_ne!(byte_bucket(31), byte_bucket(32));
    }
}
