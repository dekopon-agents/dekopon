#![expect(clippy::todo, reason = "skeleton: state/durable-store lifecycle stubs")]

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_crash_between_the_due_commit_and_handling_replays_the_delivery_at_boot() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_store_moved_to_a_new_path_restores_every_conversation() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_record_with_nothing_pending_is_deleted_and_a_wait_expires_after_thirty_days() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_wake_registered_before_a_restart_fires_once_from_its_row() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_contender_fails_at_once_and_activates_no_agent() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn after_the_owner_exits_a_new_instance_acquires_the_store_and_restores() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn one_connection_serializes_every_write_so_a_row_has_one_writer() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn ownership_is_observed_before_activation_on_a_store_that_needs_no_migration() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn an_interrupted_conversion_retries_and_converts_each_wake_once() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_converted_store_keeps_wakes_jsonl_as_converted_and_never_reads_it_again() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn conversion_streams_the_legacy_wake_file_line_by_line() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn thread_ownership_without_persisted_evidence_is_marked_lost_at_cutover() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_corrupt_legacy_wake_line_fails_boot_naming_the_line() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_failed_migration_keeps_the_backup_and_names_the_one_line_restore() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_newer_schema_or_an_unupgradable_record_fails_boot_listing_every_one() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_migration_writes_one_backup_per_old_schema_version() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn every_record_is_migrated_forward_at_boot() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn an_unacknowledged_delivery_replays_and_a_duplicate_is_accepted() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn an_acknowledgement_commits_in_the_same_transaction_as_its_wake_change() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_recurring_wake_holds_at_most_one_pending_delivery() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn an_interrupted_job_is_reported_with_its_deadline_within_one_boot() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_leave_drops_the_follow_and_the_thread_ownership_in_one_transaction() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn follows_and_thread_ownership_survive_a_restart() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_denied_wake_store_write_yields_one_cause_bearing_exit_and_a_nonzero_status() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_store_that_is_writable_again_after_the_restart_recovers() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_fatal_exit_joins_every_owned_task_within_the_shutdown_grace() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_restart_after_a_fatal_exit_resumes_from_the_store() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn the_store_is_acquired_once_at_initial_boot_and_never_by_check_or_reload() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_second_gateway_process_on_the_same_store_fails_at_once() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn conversion_reads_legacy_inputs_through_a_projected_symlink() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn legacy_follow_markers_are_retained_and_counted_never_keyed_by_guess() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_durable_off_gateway_keeps_waits_in_memory_and_warns_they_are_lost_on_restart() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn first_served_is_recorded_once_after_a_migration() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn every_serving_write_failure_raises_one_first_cause_fatal() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_queued_wake_turn_settles_its_ticket_only_when_the_turn_completes() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn an_orphaned_or_rejected_wake_delivery_settles_by_its_disposition() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_restored_delivery_is_admitted_when_serving_starts() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_subscription_synchronize_while_pending_keeps_the_pending_delivery() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn an_interrupted_job_notice_replays_after_a_crash_before_its_acknowledgement() {
    todo!()
}

#[test]
#[ignore = "skeleton: state/durable-store"]
fn a_tick_result_keeps_its_wake_id_on_every_outcome() {
    todo!()
}
