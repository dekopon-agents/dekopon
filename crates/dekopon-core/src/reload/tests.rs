#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn two_successive_projection_swaps_each_reconcile() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_projected_file_is_watched_through_its_data_link() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_event_through_a_symlinked_parent_is_covered() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_burst_inside_the_debounce_runs_one_reconcile() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_change_during_a_running_reconcile_runs_exactly_one_more() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn signaled_mode_holds_an_operator_change_until_hangup() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_machine_input_reconciles_in_signaled_mode() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_full_event_queue_becomes_one_rescan_of_every_group() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_change_between_subscribe_and_startup_still_reconciles() {}

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
fn a_directory_two_groups_watch_stays_registered_when_one_drops_it() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_failed_registration_keeps_the_previous_registrations() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_single_mounted_file_is_refused_as_unwatchable() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn mixed_case_reload_modes_parse_and_unknown_keys_are_refused() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_backend_failure_behind_a_full_queue_still_ends_the_task() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_overflow_wakes_an_idle_loop() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_hangup_before_any_event_reconciles_every_operator_group() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_rescan_rereads_every_group_without_a_safety_poll() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_recreated_watched_directory_is_registered_again() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_symlinked_file_entry_is_watched_at_its_target_directory() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_commit_subscriber_sees_only_the_latest_generation() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_dropped_commit_signal_ends_every_subscriber() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_new_commit_subscriber_starts_from_the_active_generation() {}
