#![allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::expect_used)]

use dekopon_http_host::asset::{AssetDirectory, AssetIoError, AssetJobs};

#[tokio::test]
async fn empty_spools_are_capped_per_invocation_and_capacity_returns_on_drop() {
    let root = tempfile::tempdir().unwrap();
    let directory = AssetDirectory::new(root.path().to_owned(), 0);
    let first = directory.invocation(AssetJobs::default());
    let second = directory.invocation(AssetJobs::default());
    let mut live = Vec::new();
    let mut other_live = Vec::new();
    for _ in 0..32 {
        live.push(first.allocate().await.unwrap());
        other_live.push(second.allocate().await.unwrap());
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 64);
    assert!(matches!(
        first.allocate().await,
        Err(AssetIoError::TooManySpools { maximum: 32 })
    ));
    assert!(matches!(
        second.allocate().await,
        Err(AssetIoError::TooManySpools { maximum: 32 })
    ));
    drop(live.pop());
    let replacement = first.allocate().await.unwrap();
    replacement.finish().await.unwrap();
    let _replacement = first.allocate().await.unwrap();
    assert!(matches!(
        first.allocate().await,
        Err(AssetIoError::TooManySpools { maximum: 32 })
    ));
}
