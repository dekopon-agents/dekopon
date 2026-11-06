#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use dekopon_capability::{StorageAccess, StorageInterface, StorageScope};
use dekopon_core::history::HistoryKey;
use dekopon_storage_host::{
    ChatReclaimOutcome, ContinuityPolicy, StorageGrantRequest, StorageHost, StorageHostError,
    StorageLimits,
};

fn request(provider: &str, scope: StorageScope, conversation: &str) -> StorageGrantRequest {
    StorageGrantRequest::new(
        "history-invocation".parse().unwrap(),
        "memory-chat.record".parse().unwrap(),
        provider.parse().unwrap(),
        StorageInterface::Jsonl,
        StorageAccess::ReadWrite,
        scope,
        "assistant".parse().unwrap(),
        "drn:slack:user:t0123abc:u0123abc".parse().unwrap(),
        "slack",
        "example-slack",
        "c0123abc",
        conversation,
        ContinuityPolicy::Relaxed,
        b"surface".to_vec(),
    )
}

fn key(value: &str) -> HistoryKey {
    HistoryKey::new(value.to_owned()).unwrap()
}

fn record(host: &StorageHost, conversation: &str, history: &str) -> String {
    let preparation = host
        .prepare_grant(
            request(
                "memory-chat",
                StorageScope::PrivateConversation,
                conversation,
            )
            .with_history(Some(key(history))),
        )
        .unwrap();
    let token = preparation.namespace().to_owned();
    let mut handle = host.begin(preparation.materialize().unwrap()).unwrap();
    handle
        .jsonl_append("turns.jsonl", 0, br#"{"turn":1}"#)
        .unwrap();
    handle.commit().unwrap();
    token
}

fn open(root: &std::path::Path, limits: StorageLimits) -> StorageHost {
    StorageHost::open(root, limits).unwrap()
}

#[test]
fn chat_history_is_admitted_past_the_namespace_ceiling_and_reclaimed_one_chat_at_a_time() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let limits = StorageLimits {
        max_namespaces: 1,
        ..StorageLimits::default()
    };
    let host = open(&root, limits.clone());
    let turso = host
        .grant(request("turso", StorageScope::Agent, "unused"))
        .unwrap();
    let mut turso = host.begin(turso).unwrap();
    turso.jsonl_append("db.jsonl", 0, b"{}").unwrap();
    turso.commit().unwrap();

    let first = record(&host, "c-first", "assistant/first");
    let second = record(&host, "c-second", "assistant/second");
    let third = record(&host, "c-third", "assistant/third");
    assert!(matches!(
        host.grant(request("python", StorageScope::Agent, "unused")),
        Err(StorageHostError::QuotaExceeded)
    ));
    let recorded = host.history_usage().chat_used;
    assert!(recorded > 0);

    let outcome = host.reclaim_chat(&key("assistant/first")).unwrap();
    let ChatReclaimOutcome::Reclaimed(reclaimed) = outcome else {
        panic!("an idle chat is reclaimed");
    };
    assert_eq!(reclaimed.namespaces, 1);
    assert_eq!(host.history_usage().chat_used, recorded - reclaimed.bytes);
    let namespaces = root.join("namespaces");
    assert!(!namespaces.join(&first).exists());
    assert!(namespaces.join(&second).exists());
    assert!(namespaces.join(&third).exists());

    drop(host);
    let reopened = open(&root, limits);
    assert_eq!(
        reopened.history_usage().chat_used,
        recorded - reclaimed.bytes
    );
    assert!(
        reopened
            .grant(request("turso", StorageScope::Agent, "unused"))
            .is_ok(),
        "the non-chat namespace survives and keeps its slot"
    );
}

#[test]
fn a_chat_with_an_open_handle_is_busy_and_keeps_every_byte() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = open(&root, StorageLimits::default());
    let token = record(&host, "c-busy", "assistant/busy");
    let before = host.history_usage().chat_used;
    let grant = host
        .grant(
            request("memory-chat", StorageScope::PrivateConversation, "c-busy")
                .with_history(Some(key("assistant/busy"))),
        )
        .unwrap();
    let handle = host.begin(grant).unwrap();
    assert_eq!(
        host.reclaim_chat(&key("assistant/busy")).unwrap(),
        ChatReclaimOutcome::Busy
    );
    assert!(root.join("namespaces").join(&token).exists());
    drop(handle);
    assert_eq!(host.history_usage().chat_used, before);
}

#[test]
fn a_reclaim_interrupted_after_its_rename_finishes_at_the_next_open() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = open(&root, StorageLimits::default());
    let token = record(&host, "c-crash", "assistant/crash");
    let kept = record(&host, "c-kept", "assistant/kept");
    drop(host);
    let namespaces = root.join("namespaces");
    std::fs::rename(
        namespaces.join(&token),
        namespaces.join(format!("{token}.reclaiming")),
    )
    .unwrap();

    let reopened = open(&root, StorageLimits::default());
    assert!(!namespaces.join(format!("{token}.reclaiming")).exists());
    assert!(namespaces.join(&kept).exists());
    let ChatReclaimOutcome::Reclaimed(reclaimed) =
        reopened.reclaim_chat(&key("assistant/crash")).unwrap()
    else {
        panic!("an absent chat counts as reclaimed");
    };
    assert_eq!(reclaimed.namespaces, 0);
}
