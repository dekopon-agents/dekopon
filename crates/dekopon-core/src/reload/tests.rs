#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn aborting_the_watcher_mid_reconcile_leaves_the_active_generation_serving() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn two_successive_data_symlink_swaps_each_apply_without_a_restart() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_change_during_a_running_reconcile_runs_exactly_one_more() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn signaled_mode_holds_a_change_until_hangup() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_unchanged_hangup_reconciles_nothing() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_machine_input_is_watched_in_signaled_mode() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_watch_on_the_file_instead_of_its_directory_misses_a_projected_update() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_full_event_queue_becomes_one_rescan_of_every_group() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_change_between_subscribe_and_baseline_still_reconciles() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_backend_failure_ends_the_task_with_its_cause() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_closed_event_queue_ends_the_task_with_its_cause() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_activation_that_overruns_its_bound_ends_the_task() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_applied_reload_that_names_a_new_directory_watches_it() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_watch_set_registers_each_directory_once() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_failed_registration_keeps_the_previous_watch_set() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_subpath_or_env_delivered_input_is_refused_as_unwatchable() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn the_verification_window_stops_reading_at_its_cap() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn mixed_case_reload_modes_parse_and_unknown_keys_are_refused() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_commit_subscriber_sees_only_the_latest_generation() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_dropped_commit_signal_ends_every_subscriber() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_new_commit_subscriber_starts_from_the_active_generation() {}
