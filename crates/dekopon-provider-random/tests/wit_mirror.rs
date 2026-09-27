#[test]
fn random_guest_mirrors_published_package() {
    assert_eq!(
        include_str!("../wit/deps/random.wit"),
        include_str!("../../../wit/random/random.wit")
    );
}
