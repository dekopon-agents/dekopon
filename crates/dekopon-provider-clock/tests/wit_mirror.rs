#![allow(clippy::unwrap_used)]

#[test]
fn vendored_clock_contract_matches_the_published_package() {
    assert_eq!(
        include_str!("../wit/deps/clock.wit"),
        include_str!("../../../wit/clock/clock.wit")
    );
}
