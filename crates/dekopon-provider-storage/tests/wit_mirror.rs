//! Pins this crate's vendored storage contract to the canonical one, byte for byte.
//!
//! `wit/deps/storage.wit` is a copy of `wit/storage/storage.wit`, the file the broker's host side
//! implements. A copy that has drifted generates bindings for an interface no host provides, and a
//! prefix check only proves the two agree on their first line — which is exactly the version
//! number a drifting edit is least likely to touch. The storage contract is also the one under
//! active change, so this is the mirror most worth pinning.
//!
//! The comparison lives in `tests/` rather than in `src/` because the published package carries
//! only `src/**`, `wit/**`, `README.md`, and `Cargo.toml` — the repository-relative path below
//! does not exist for a crates.io consumer.

use dekopon_provider_storage::STORAGE_WIT;

#[test]
fn vendored_storage_contract_matches_the_canonical_wit() {
    assert_eq!(
        STORAGE_WIT,
        include_str!("../../../wit/storage/storage.wit")
    );
}
