#[tokio::test]
#[ignore = "skeleton: egress/vault-rotation"]
async fn an_aborted_store_leaves_the_previous_cookie_set_on_disk() {}

#[tokio::test]
#[ignore = "skeleton: egress/vault-rotation"]
async fn a_reseed_replaces_the_whole_set_and_no_older_rotation_is_stored_after_it() {}

#[tokio::test]
#[ignore = "skeleton: egress/vault-rotation"]
async fn a_rotation_that_would_exceed_the_cookie_budget_is_never_stored() {}

#[tokio::test]
#[ignore = "skeleton: egress/vault-rotation"]
async fn a_restarted_broker_pushes_the_last_stored_rotation() {}

#[tokio::test]
#[ignore = "skeleton: egress/vault-rotation"]
async fn the_release_call_collects_the_final_rotation() {}
