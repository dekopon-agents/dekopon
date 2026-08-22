//! Pins this crate's vendored HTTP contract to the published one, byte for byte.
//!
//! `wit/deps/http.wit` is a copy of `wit/http/http.wit`, which `wit-package.yml` builds into the
//! immutable `dekopon:http@1.0.0` registry package. A copy that has drifted generates bindings for
//! an interface no broker implements, and a prefix check only proves the two agree on their first
//! line. The comparison lives in `tests/` rather than in `src/` because the published package
//! carries only `src/**`, `wit/**`, `README.md`, and `Cargo.toml` — the repository-relative path
//! below does not exist for a crates.io consumer.

use dekopon_provider_http::HTTP_WIT;

#[test]
fn vendored_http_contract_matches_the_published_package() {
    assert_eq!(HTTP_WIT, include_str!("../../../wit/http/http.wit"));
}
