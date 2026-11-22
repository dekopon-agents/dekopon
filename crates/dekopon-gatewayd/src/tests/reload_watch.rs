#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn a_panicking_reload_task_stops_the_gateway_with_its_cause() {}

#[test]
#[ignore = "skeleton: daemon/supervisor"]
fn the_reload_task_is_joined_at_shutdown() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_aborted_gateway_reload_keeps_the_last_good_generation_serving() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_projected_gateway_config_update_applies_twice_without_a_restart() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_invalid_gateway_change_is_refused_while_last_good_serves() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_valid_restart_category_change_exits_with_its_cause() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_catalog_only_change_swaps_the_catalog() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_burst_of_catalog_events_runs_one_reconcile() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_restarted_gateway_serves_the_files_on_disk() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn an_activation_that_cannot_settle_exits_after_its_bound() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_skill_directory_named_by_a_reload_is_watched_from_then_on() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn the_gateway_never_watches_a_broker_private_path() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_reload_block_change_is_a_restart_category() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_restart_only_change_that_cannot_boot_is_refused_not_exited() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn the_gateway_watch_set_classifies_each_source_path() {}

#[test]
#[ignore = "skeleton: daemon/watcher-core"]
fn a_config_change_during_boot_is_served_after_startup() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_gateway_reload_sends_no_prepare_or_commit() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn the_gateway_folds_a_published_broker_digest_without_calling_prepare() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_gateway_reload_racing_a_broker_commit_serves_the_newest_broker_digest() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn the_broker_generation_feed_holds_only_the_latest_digest() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_reconnected_gateway_learns_the_broker_digest_at_connect() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_lost_broker_feed_reconnects_and_serves_the_last_digest_meanwhile() {}

#[test]
#[ignore = "skeleton: daemon/broker-self-reload"]
fn a_route_naming_an_agent_the_broker_refused_is_refused_until_the_broker_applies() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn a_reload_that_adds_the_first_follow_route_enables_follow() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn a_follow_table_swap_keeps_in_flight_follow_tickets() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn a_swapped_follow_table_retires_the_previous_table() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn follow_state_survives_a_restart_through_the_durable_store() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn every_restart_category_names_the_resource_it_binds() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn a_proxy_tls_volume_change_swaps_the_acceptor_through_the_gateway_watcher() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn a_proxy_tls_change_applies_in_signaled_mode() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn a_refused_tls_bundle_is_reported_once_per_change() {}

#[test]
#[ignore = "skeleton: daemon/reload-consumers"]
fn a_panicking_tls_prepare_stops_the_gateway_with_its_cause() {}
