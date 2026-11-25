#[tokio::test]
#[ignore = "skeleton: daemon/broker-self-reload"]
async fn an_aborted_reconcile_keeps_the_active_generation_and_its_digest() {}

#[tokio::test]
#[ignore = "skeleton: daemon/broker-self-reload"]
async fn a_broker_d_edit_commits_in_process_and_publishes_one_digest() {}

#[tokio::test]
#[ignore = "skeleton: daemon/broker-self-reload"]
async fn the_commit_signal_holds_only_the_latest_digest() {}

#[tokio::test]
#[ignore = "skeleton: daemon/broker-self-reload"]
async fn a_restarted_broker_publishes_the_digest_of_its_boot_generation_at_connect() {}

#[tokio::test]
#[ignore = "skeleton: daemon/broker-self-reload"]
async fn an_isolated_lock_replacement_ends_in_a_bounded_broker_exit() {}

#[tokio::test]
#[ignore = "skeleton: daemon/broker-self-reload"]
async fn the_refresh_scheduler_rereads_the_catalog_on_each_commit() {}
