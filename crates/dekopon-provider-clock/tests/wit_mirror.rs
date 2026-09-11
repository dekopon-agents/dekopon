//! Pins this crate's vendored clock contract to the published one, byte for byte.
//!
//! `wit/deps/clock.wit` is a copy of `wit/clock/clock.wit`, which `wit-package.yml` builds into the
//! immutable `dekopon:clock@1.0.0` registry package. A copy that has drifted generates bindings for
//! an interface no broker implements. The comparison lives in `tests/` rather than in `src/` because
//! the published package carries only `src/**`, `wit/**`, `README.md`, and `Cargo.toml` — the
//! repository-relative path below does not exist for a crates.io consumer.

#[test]
fn vendored_clock_contract_matches_the_published_package() {
    assert_eq!(
        include_str!("../wit/deps/clock.wit"),
        include_str!("../../../wit/clock/clock.wit")
    );
}
