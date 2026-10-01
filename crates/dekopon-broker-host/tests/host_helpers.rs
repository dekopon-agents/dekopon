use dekopon_broker_host::host::{StoreLimits, command_input_bytes, validate_limits};

#[test]
fn broker_owned_helpers_keep_the_existing_bounds() {
    let limits = StoreLimits::default();
    validate_limits(&limits, &[("fuel", 1)]).expect("nonzero bounds");
    assert_eq!(command_input_bytes(&["echo".into()], Some("ok")), 6);
}
