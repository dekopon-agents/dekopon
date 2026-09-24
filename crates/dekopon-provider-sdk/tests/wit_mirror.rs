use dekopon_provider_sdk::ASSET_WIT;

#[test]
fn vendored_asset_contract_matches_the_canonical_package() {
    assert_eq!(ASSET_WIT, include_str!("../../../wit/asset/asset.wit"));
}

#[test]
fn asset_dependency_mirrors_match_the_canonical_package() {
    for mirror in [
        include_str!("../../../wit/http/deps/asset.wit"),
        include_str!("../../dekopon-provider-http/wit/deps/asset.wit"),
        include_str!("../../dekopon-broker-host/wit/deps/asset.wit"),
        include_str!("../../../examples/providers/http-probe/wit/deps/asset.wit"),
    ] {
        assert_eq!(ASSET_WIT, mirror);
    }
}
