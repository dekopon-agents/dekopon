#[tokio::test]
#[ignore = "skeleton: daemon/signer-reload"]
async fn an_aborted_jwks_confirmation_keeps_signing_with_the_active_key() {}

#[tokio::test]
#[ignore = "skeleton: daemon/signer-reload"]
async fn a_staged_key_is_promoted_only_after_the_jwks_serves_its_kid() {}

#[tokio::test]
#[ignore = "skeleton: daemon/signer-reload"]
async fn the_signer_holds_at_most_one_staged_key() {}

#[tokio::test]
#[ignore = "skeleton: daemon/signer-reload"]
async fn a_restarted_broker_signs_with_the_key_on_disk() {}
