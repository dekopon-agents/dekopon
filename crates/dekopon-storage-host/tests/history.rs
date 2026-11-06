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

fn record_unlinked(host: &StorageHost, conversation: &str) -> String {
    let preparation = host
        .prepare_grant(request(
            "memory-chat",
            StorageScope::PrivateConversation,
            conversation,
        ))
        .unwrap();
    let token = preparation.namespace().to_owned();
    let mut handle = host.begin(preparation.materialize().unwrap()).unwrap();
    handle
        .jsonl_append("turns.jsonl", 0, br#"{"turn":1}"#)
        .unwrap();
    handle.commit().unwrap();
    token
}

#[test]
fn history_recorded_without_a_key_counts_and_is_reclaimable_as_its_own_unit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = open(&root, StorageLimits::default());
    let token = record_unlinked(&host, "c-legacy");
    drop(host);

    let host = open(&root, StorageLimits::default());
    assert_eq!(host.history_usage().chat_used, 0);
    assert_eq!(
        host.adopt_unlinked_history(&"memory-chat".parse().unwrap())
            .unwrap(),
        1
    );
    let usage = host.history_usage();
    assert!(usage.chat_used > 0);
    let unlinked = usage.oldest_unlinked.expect("the legacy unit is offered");
    assert_eq!(unlinked.key.as_str(), format!("unlinked/{token}"));
    let ChatReclaimOutcome::Reclaimed(reclaimed) = host.reclaim_chat(&unlinked.key).unwrap() else {
        panic!("an idle unit is reclaimed");
    };
    assert_eq!(reclaimed.namespaces, 1);
    assert_eq!(host.history_usage().chat_used, 0);
    assert!(host.history_usage().oldest_unlinked.is_none());
}

#[test]
fn the_record_bound_covers_a_new_namespace_and_its_first_turn() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = open(&root, StorageLimits::default());
    let bound = host.history_usage().record_bound;
    record(&host, "c-bound", "assistant/bound");
    let grown = host.history_usage().chat_used;
    assert!(grown > 8 * 4_096, "a new namespace costs its entry charges");
    assert!(bound >= grown, "bound {bound} below growth {grown}");
}

#[test]
fn a_tombstone_left_by_a_failed_removal_is_finished_by_the_next_reclaim_or_grant() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = open(&root, StorageLimits::default());
    let token = record(&host, "c-stuck", "assistant/stuck");
    let namespaces = root.join("namespaces");
    let tombstone = namespaces.join(format!("{token}.reclaiming"));
    std::fs::rename(namespaces.join(&token), &tombstone).unwrap();

    let ChatReclaimOutcome::Reclaimed(reclaimed) =
        host.reclaim_chat(&key("assistant/stuck")).unwrap()
    else {
        panic!("a stranded tombstone is finished");
    };
    assert_eq!(reclaimed.namespaces, 1);
    assert!(!tombstone.exists());
    assert_eq!(host.history_usage().chat_used, 0);

    let again = record(&host, "c-stuck", "assistant/stuck");
    std::fs::rename(namespaces.join(&again), &tombstone).unwrap();
    record(&host, "c-stuck", "assistant/stuck");
    assert!(
        !tombstone.exists(),
        "a regrant finishes the tombstone first"
    );
}

#[test]
fn the_idle_ttl_sweep_leaves_chat_history_to_the_chat_clock() {
    use dekopon_capability::StorageRetention;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = open(&root, StorageLimits::default());
    let token = record(&host, "c-ttl", "assistant/ttl");
    let marker = root.join("namespaces").join(&token).join("last-used");
    std::fs::File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(600)),
        )
        .unwrap();
    let policies = std::collections::BTreeMap::from([(
        (
            "memory-chat".parse().unwrap(),
            StorageScope::PrivateConversation,
        ),
        StorageRetention::IdleTtl(std::time::Duration::from_secs(60)),
    )]);
    assert_eq!(host.sweep(&policies).unwrap().deleted, 0);
    assert!(root.join("namespaces").join(&token).exists());
}
