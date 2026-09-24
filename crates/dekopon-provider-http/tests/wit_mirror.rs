#![allow(clippy::unwrap_used)]

use dekopon_provider_http::HTTP_WIT;

#[test]
fn vendored_http_contract_matches_the_published_package() {
    assert_eq!(HTTP_WIT, include_str!("../../../wit/http/http.wit"));
}

#[test]
fn host_and_probe_http_mirrors_match_the_canonical_package() {
    for mirror in [
        include_str!("../../dekopon-broker-host/wit/deps/http.wit"),
        include_str!("../../../examples/providers/http-probe/wit/deps/http.wit"),
    ] {
        assert_eq!(HTTP_WIT, mirror);
    }
}
