#![allow(clippy::unwrap_used)]

use dekopon_provider_storage::STORAGE_WIT;

#[test]
fn vendored_storage_contract_matches_the_canonical_wit() {
    assert_eq!(
        STORAGE_WIT,
        include_str!("../../../wit/storage/storage.wit")
    );
}
