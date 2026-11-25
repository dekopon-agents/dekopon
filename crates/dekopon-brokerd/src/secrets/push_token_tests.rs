use std::{os::unix::fs::PermissionsExt as _, path::Path, sync::Arc};

use dekopon_broker::{SecretResolver as _, TokenTarget};
use dekopon_core::{
    SecretDrn,
    base64::{Engine as _, URL_SAFE_NO_PAD},
};
use dekopon_test_support::LoopbackServer;
use serde_json::{Value, json};

use super::{RefreshError, chatgpt::ChatGptSubscription, load_with_bytes, tests::write};

const DRN: &str = "drn:secret:vm/codex";

fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn generated(label: &str) -> String {
    let mut bytes = [0_u8; 24];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .expect("random fixture bytes");
    format!("{label}-{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn access_token(nonce: &str) -> String {
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-fixture"},
            "nonce": nonce,
        }))
        .expect("serialize JWT fixture"),
    );
    format!("header.{payload}.signature")
}

struct Record {
    _directory: tempfile::TempDir,
    auth: std::path::PathBuf,
    access: String,
    refresh: String,
}

async fn record(expires_at: u64) -> Record {
    let directory = tempfile::tempdir().expect("temporary directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private directory");
    let auth = directory.path().join("auth.json");
    let access = access_token(&generated("previous"));
    let refresh = generated("refresh");
    write(
        &auth,
        json!({
            "version": 1,
            "access": access,
            "refresh": refresh,
            "expiresAt": expires_at,
            "accountId": "acct-fixture",
        })
        .to_string()
        .as_bytes(),
    )
    .await;
    Record {
        _directory: directory,
        auth,
        access,
        refresh,
    }
}

fn stored(auth: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(auth).expect("read record")).expect("record JSON")
}

fn granted(access: &str) -> Vec<u8> {
    let body = json!({
        "access_token": access,
        "refresh_token": generated("rotated-refresh"),
        "expires_in": 3_600,
    })
    .to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn subscription(record: &Record, server: &LoopbackServer) -> ChatGptSubscription {
    ChatGptSubscription::open_at_local_endpoints(
        &DRN.parse().expect("DRN"),
        &record.auth,
        uid(),
        &server.url(),
    )
    .expect("subscription opens")
}

#[tokio::test]
async fn an_aborted_vm_lane_refresh_keeps_the_previous_access_token() {
    let record = record(1).await;
    let rotated = access_token(&generated("rotated"));
    let server = LoopbackServer::sequence([
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
        granted(&rotated),
    ]);
    let subscription = subscription(&record, &server);

    let aborted = subscription
        .pushed_token()
        .await
        .expect_err("the token endpoint refused");
    assert!(matches!(aborted, RefreshError::Unavailable { .. }));
    assert_eq!(stored(&record.auth)["access"], record.access.as_str());
    assert_eq!(stored(&record.auth)["refresh"], record.refresh.as_str());

    let pushed = subscription
        .pushed_token()
        .await
        .expect("the retry rotates");
    assert_eq!(pushed.token.expose(), &rotated);
    let requests = [server.request_text(), server.request_text()];
    for request in &requests {
        assert!(
            request.contains(&format!("refresh_token={}", record.refresh)),
            "{request}"
        );
    }
    server.join();
}

#[tokio::test]
async fn a_reloaded_map_keeps_one_vm_lane_login_per_record() {
    let record = record(1).await;
    let rotated = access_token(&generated("rotated"));
    let server = LoopbackServer::once(&granted(&rotated));
    let before = subscription(&record, &server);
    let after = subscription(&record, &server);

    let first = before
        .pushed_token()
        .await
        .expect("first generation pushes");
    let second = after
        .pushed_token()
        .await
        .expect("reloaded generation pushes");

    assert_eq!(first.token.expose(), &rotated);
    assert_eq!(second.token.expose(), &rotated);
    assert_eq!(stored(&record.auth)["access"], rotated.as_str());
    assert!(server.request_text().contains("grant_type=refresh_token"));
    assert!(server.recorded().is_empty(), "one refresh for the record");
    server.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_pushes_share_one_vm_lane_refresh() {
    let record = record(1).await;
    let rotated = access_token(&generated("rotated"));
    let server = LoopbackServer::once(&granted(&rotated));
    let subscription = Arc::new(subscription(&record, &server));

    let mut pushes = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let subscription = Arc::clone(&subscription);
        pushes.spawn(async move { subscription.pushed_token().await });
    }
    while let Some(pushed) = pushes.join_next().await {
        let pushed = pushed.expect("push task").expect("push");
        assert_eq!(pushed.token.expose(), &rotated);
    }
    assert!(server.request_text().contains("grant_type=refresh_token"));
    assert!(server.recorded().is_empty(), "exactly one token request");
    server.join();
}

#[tokio::test]
async fn a_restarted_broker_pushes_the_access_token_from_the_stored_record() {
    let record = record(4_102_444_800).await;
    let map = record.auth.with_file_name("map.yaml");
    write(
        &map,
        format!(
            "apiVersion: dekopon.dev/secret-map/v1alpha1\nmapRevision: test\nsecrets:\n  - drn: {DRN}\n    destinations: [chatgpt.com]\n    source: {{ kind: chatgptSubscription, authFile: {} }}\n    bindings:\n      - id: codex\n        capabilities: [vm.exec]\n        sink: httpBearer\n        allowedHosts: [chatgpt.com]\n        allowedMethods: [POST]\n        allowedPaths: [{{ match: exact, path: /backend-api/codex/responses }}]\n",
            record.auth.display()
        )
        .as_bytes(),
    )
    .await;
    let drn: SecretDrn = DRN.parse().expect("DRN");

    for _ in 0..2 {
        let loaded = load_with_bytes(&map, uid()).await.expect("map loads");
        let resolver = loaded.scheduled.resolver.expect("map resolver");
        let material = resolver
            .egress_profile(&drn)
            .await
            .expect("profile resolves");
        assert!(material.cookies.entries().is_empty());
        let [token] = &material.tokens[..] else {
            panic!("one pushed token");
        };
        assert_eq!(token.token.expose(), &record.access);
        assert_eq!(
            token.on,
            [TokenTarget::Bearer {
                host: "chatgpt.com".to_owned(),
                paths: None,
            }]
        );
    }
}
