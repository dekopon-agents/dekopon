#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_panicking_broker_reload_task_stops_the_broker_with_its_cause() {}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn the_broker_reload_task_is_joined_at_shutdown() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_broker_config_edit_applies_in_process() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn an_invalid_broker_edit_is_refused_while_the_active_generation_serves() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_valid_broker_restart_category_change_exits_with_its_cause() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn an_aborted_broker_reload_leaves_the_active_generation_serving() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_burst_of_broker_config_events_runs_one_reload() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_restarted_broker_loads_its_config_from_disk() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn the_refresh_scheduler_sees_a_new_generation_at_its_commit() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_replaced_provider_lock_ends_in_a_bounded_broker_exit() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn an_unchanged_lock_digest_stands_the_check_down() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn three_unreadable_lock_reads_stop_the_broker_with_its_cause() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn the_lock_is_watched_in_signaled_mode() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn the_brokers_own_credential_writes_trigger_no_reload() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_prepare_frame_from_an_older_gateway_is_refused() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn each_commit_publishes_the_broker_digest() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_restart_only_broker_change_that_cannot_boot_is_refused_not_exited() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_rotated_github_app_key_reloads_the_broker() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn an_aborted_lock_check_leaves_the_broker_serving() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_restarted_broker_verifies_the_lock_on_disk() {}

#[test]
#[ignore = "skeleton: daemon/signer-reload"]
fn a_renewed_signing_key_leaves_the_config_generation_unchanged() {}

#[test]
#[ignore = "skeleton: daemon/signer-reload"]
fn a_renewed_signing_key_is_promoted_only_after_the_jwks_serves_its_kid() {}

#[test]
#[ignore = "skeleton: daemon/signer-reload"]
fn a_jwks_that_never_serves_the_kid_keeps_the_active_key() {}

#[test]
#[ignore = "skeleton: daemon/signer-reload"]
fn a_second_renewal_replaces_the_staged_key() {}

#[test]
#[ignore = "skeleton: daemon/signer-reload"]
fn a_restarted_broker_signs_with_the_mounted_key() {}
