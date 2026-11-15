#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests spawn and join freely"
)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

const TOKEN: &str = "fixture-secret-0123456789";

async fn credential_quoting_panic() -> tokio::task::JoinError {
    tokio::task::spawn_blocking(|| {
        let body = format!("\"{TOKEN}\"");
        let _seconds: u64 = serde_json::from_str(&body).expect("expires_in is a number");
    })
    .await
    .expect_err("a string is not a number")
}

#[tokio::test]
async fn a_join_failure_field_records_only_the_class() {
    let failure = dekopon_core::TaskOutcome::from_join_error(credential_quoting_panic().await);
    let rendered = failure.to_string();
    assert_eq!(failure.category(), "panicked");
    assert!(
        !rendered.contains(TOKEN),
        "the payload is in the logged field: {rendered}"
    );
}

#[tokio::test]
async fn a_broker_storage_task_error_chain_omits_the_panic_payload() {
    let error = dekopon_broker::BrokerError::StorageTask {
        source: dekopon_core::TaskOutcome::from_join_error(credential_quoting_panic().await),
    };
    let rendered = dekopon_core::error_chain(&error);
    assert!(
        !rendered.contains(TOKEN),
        "the payload is in the logged chain: {rendered}"
    );
}

#[tokio::test]
async fn a_chatgpt_refresh_error_chain_omits_the_panic_payload() {
    let error = dekopon_model::chatgpt::ChatGptError::RefreshTask(std::io::Error::other(
        dekopon_core::TaskOutcome::from_join_error(credential_quoting_panic().await),
    ));
    let rendered = dekopon_core::error_chain(&error);
    assert!(
        !rendered.contains(TOKEN),
        "the payload is in the logged chain: {rendered}"
    );
}
