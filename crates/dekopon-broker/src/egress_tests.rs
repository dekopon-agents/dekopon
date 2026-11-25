#[test]
#[ignore = "skeleton: state/grant-settle"]
fn a_grant_taking_capability_without_a_grant_is_refused_before_any_provider_call() {}

#[test]
#[ignore = "skeleton: state/grant-settle"]
fn an_applied_false_return_reports_grant_unused() {}

#[test]
#[ignore = "skeleton: state/grant-settle"]
fn a_missing_or_unparsable_return_reports_grant_used() {}

#[test]
#[ignore = "skeleton: egress/push-contract"]
fn a_vm_exec_push_leaves_in_the_host_owned_header_and_never_in_provider_memory() {}

#[test]
#[ignore = "skeleton: egress/push-contract"]
fn every_vm_call_under_an_egress_profile_carries_the_push() {}

#[test]
#[ignore = "skeleton: egress/push-contract"]
fn an_oversized_push_is_refused_naming_the_profile() {}

#[test]
#[ignore = "skeleton: egress/push-contract"]
fn cedar_secret_use_chooses_the_pushed_set() {}

#[test]
#[ignore = "skeleton: egress/vault-rotation"]
fn only_storable_rotations_from_an_applied_return_reach_the_vault() {}
