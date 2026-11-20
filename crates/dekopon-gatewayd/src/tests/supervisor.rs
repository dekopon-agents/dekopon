#![expect(clippy::todo, reason = "skeleton: daemon/supervisor lifecycle stubs")]

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_reader_that_fails_mid_frame_restarts_after_its_backoff() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_reconnected_reader_resets_its_backoff() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_reader_restart_delay_never_exceeds_the_backoff_cap() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn readers_restart_through_the_backoff_while_the_gateway_serves() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_wake_tick_panic_emits_one_cause_bearing_event_and_the_gateway_serves_on() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_reload_during_a_tick_keeps_the_tick_and_its_wake() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn ticks_in_flight_are_bounded_by_the_session_gate() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_tick_that_panicked_before_a_restart_runs_again_from_its_row() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_denied_wake_store_write_yields_one_cause_bearing_exit_and_a_nonzero_status() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_store_that_is_writable_again_after_the_restart_recovers() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_fatal_exit_joins_every_owned_task_within_the_shutdown_grace() {
    todo!()
}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_restart_after_a_fatal_exit_resumes_from_the_store() {
    todo!()
}
