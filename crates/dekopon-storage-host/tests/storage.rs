#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::Path,
    sync::{Arc, Barrier, mpsc},
    thread,
};

use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};
use dekopon_core::error_chain;
use dekopon_storage_host::{
    ContinuityPolicy, Durability, LockLevel, OpenOptions, StorageGrantRequest, StorageHost,
    StorageHostError, StorageLimits,
};
use dekopon_test_support::{CaptureLayer, snapshot_tree};
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt as _;

/// Runs `body` with every storage log record on this thread captured.
fn captured<T>(body: impl FnOnce() -> T) -> (T, CaptureLayer) {
    let capture = CaptureLayer::workspace();
    let value = tracing::subscriber::with_default(
        tracing_subscriber::registry().with(capture.clone()),
        body,
    );
    (value, capture)
}

/// The only namespace base under `root`.
fn only_base(root: &Path) -> std::path::PathBuf {
    let mut bases = fs::read_dir(root.join("namespaces"))
        .expect("namespace root")
        .map(|entry| entry.expect("base entry").path())
        .collect::<Vec<_>>();
    assert_eq!(bases.len(), 1, "{bases:?}");
    bases.remove(0)
}

/// Every generation directory under one base, sorted.
fn generations(base: &Path) -> Vec<String> {
    let mut names = fs::read_dir(base)
        .expect("base entries")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let directory = temporary
        .path()
        .canonicalize()
        .expect("canonical temporary directory");
    let root = directory.join("storage");
    let key = directory.join("storage-key.yaml");
    write_key(&key);
    (temporary, root, key)
}

fn write_key(path: &Path) {
    fs::write(
        path,
        "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
    )
    .expect("write key");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("key mode");
}

fn vfs_request(invocation: &str, access: StorageAccess) -> StorageGrantRequest {
    StorageGrantRequest::new(
        invocation.parse().expect("invocation"),
        "probe.vfs".parse().expect("capability"),
        "storage-probe".parse().expect("provider"),
        StorageInterface::DurableFiles,
        access,
        StorageNamespace::Chat,
        "reviewer".parse().expect("agent"),
        "slack.t0123abc.u9xyz".parse().expect("subject"),
        "slack",
        "scientist-slack",
        "c0123abc",
        "c0123abc:1712345678.000100",
        ContinuityPolicy::Stable,
        b"authority".to_vec(),
    )
}

fn scoped_request(
    surface: &[u8],
    continuity: ContinuityPolicy,
    invocation: &str,
    access: StorageAccess,
    subject: &str,
) -> StorageGrantRequest {
    StorageGrantRequest::new(
        invocation.parse().expect("invocation"),
        "memory.chat.record".parse().expect("capability"),
        "memory-chat".parse().expect("provider"),
        StorageInterface::Jsonl,
        access,
        StorageNamespace::Chat,
        "reviewer".parse().expect("agent"),
        subject.parse().expect("subject"),
        "slack",
        "scientist-slack",
        "c0123abc",
        "c0123abc:1712345678.000100",
        continuity,
        surface.to_vec(),
    )
}

fn request(
    surface: &[u8],
    continuity: ContinuityPolicy,
    invocation: &str,
    access: StorageAccess,
) -> StorageGrantRequest {
    scoped_request(
        surface,
        continuity,
        invocation,
        access,
        "slack.t0123abc.u9xyz",
    )
}

#[test]
fn jsonl_commits_and_reopens_without_raw_scope_paths() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let grant = host
        .grant(request(
            b"authority-a",
            ContinuityPolicy::AuthorityBound,
            "write-1",
            StorageAccess::ReadWrite,
        ))
        .expect("grant");
    let mut transaction = host.begin(grant).expect("transaction");
    assert_eq!(
        transaction
            .jsonl_append("turns.jsonl", 0, br#"{"turn":1}"#)
            .expect("append"),
        11
    );
    transaction.commit().expect("commit");
    drop(host);

    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("reopen");
    let grant = host
        .grant(request(
            b"authority-a",
            ContinuityPolicy::AuthorityBound,
            "read-1",
            StorageAccess::ReadOnly,
        ))
        .expect("read grant");
    let mut transaction = host.begin(grant).expect("read transaction");
    assert_eq!(transaction.jsonl_size("turns.jsonl").expect("size"), 11);
    assert_eq!(
        transaction
            .jsonl_read_chunk("turns.jsonl", 0, 64)
            .expect("read")
            .bytes,
        b"{\"turn\":1}\n"
    );
    transaction.finish_read().expect("finish");

    let tree = snapshot_tree(&root)
        .into_iter()
        .map(|entry| entry.relative.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    for sentinel in [
        "memory-chat",
        "reviewer",
        "slack.t0123abc.u9xyz",
        "scientist-slack",
        "c0123abc",
        "turns.jsonl",
    ] {
        assert!(!tree.contains(sentinel), "raw sentinel leaked: {sentinel}");
    }
}

#[test]
fn authority_bound_never_reuses_an_old_generation() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    for (index, surface) in [b"a".as_slice(), b"b", b"a"].into_iter().enumerate() {
        let grant = host
            .grant(request(
                surface,
                ContinuityPolicy::AuthorityBound,
                &format!("invoke-{index}"),
                StorageAccess::ReadOnly,
            ))
            .expect("grant");
        host.begin(grant)
            .expect("transaction")
            .finish_read()
            .expect("finish");
    }
    let namespaces = root.join("namespaces");
    let base = fs::read_dir(namespaces)
        .expect("bases")
        .next()
        .expect("one base")
        .expect("base")
        .path();
    let generations = fs::read_dir(base)
        .expect("generation entries")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count();
    assert_eq!(generations, 3, "A -> B -> A must mint three generations");
}

#[test]
fn authority_bound_does_not_reopen_an_epoch_after_stable_continuity() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");

    let mut first = host
        .begin(
            host.grant(request(
                b"authority-a",
                ContinuityPolicy::AuthorityBound,
                "authority-before-stable",
                StorageAccess::ReadWrite,
            ))
            .expect("first authority grant"),
        )
        .expect("first authority transaction");
    first
        .jsonl_append("turns.jsonl", 0, br#"{"oldAuthority":true}"#)
        .expect("first append");
    first.commit().expect("first commit");

    host.begin(
        host.grant(request(
            b"authority-a",
            ContinuityPolicy::Stable,
            "stable-between-authorities",
            StorageAccess::ReadOnly,
        ))
        .expect("stable grant"),
    )
    .expect("stable transaction")
    .finish_read()
    .expect("finish stable");

    let mut after = host
        .begin(
            host.grant(request(
                b"authority-a",
                ContinuityPolicy::AuthorityBound,
                "authority-after-stable",
                StorageAccess::ReadOnly,
            ))
            .expect("second authority grant"),
        )
        .expect("second authority transaction");
    assert!(matches!(
        after.jsonl_size("turns.jsonl"),
        Err(StorageHostError::NotFound)
    ));
    after.finish_read().expect("finish second authority");

    let base = fs::read_dir(root.join("namespaces"))
        .expect("namespace root")
        .next()
        .expect("one base")
        .expect("base")
        .path();
    assert_eq!(
        fs::read_dir(base)
            .expect("base entries")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .count(),
        3,
        "authority -> stable -> same authority must use three generations"
    );
}

#[test]
fn wrong_key_and_second_writer_fail_closed() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("first host");
    assert!(matches!(
        StorageHost::open(&root, &key, StorageLimits::default()),
        Err(StorageHostError::SecondWriter)
    ));
    drop(host);
    fs::write(
        &key,
        "apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    )
    .expect("replace key");
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).expect("key mode");
    assert!(matches!(
        StorageHost::open(&root, &key, StorageLimits::default()),
        Err(StorageHostError::KeyMismatch)
    ));
}

#[test]
fn every_trusted_scope_dimension_isolated_from_the_others() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let grant = host
        .grant(request(
            b"authority",
            ContinuityPolicy::Stable,
            "writer",
            StorageAccess::ReadWrite,
        ))
        .expect("writer grant");
    let mut transaction = host.begin(grant).expect("writer transaction");
    transaction
        .jsonl_append("turns.jsonl", 0, br#"{"private":true}"#)
        .expect("append");
    transaction.commit().expect("commit");

    let cases = [
        (
            "other-provider",
            "reviewer",
            "slack.t0123abc.u9xyz",
            "slack",
            "scientist-slack",
            "c0123abc",
            "c0123abc:1712345678.000100",
        ),
        (
            "memory-chat",
            "other-agent",
            "slack.t0123abc.u9xyz",
            "slack",
            "scientist-slack",
            "c0123abc",
            "c0123abc:1712345678.000100",
        ),
        (
            "memory-chat",
            "reviewer",
            "slack.t0123abc.uother",
            "slack",
            "scientist-slack",
            "c0123abc",
            "c0123abc:1712345678.000100",
        ),
        (
            "memory-chat",
            "reviewer",
            "slack.t0123abc.u9xyz",
            "local",
            "scientist-slack",
            "c0123abc",
            "c0123abc:1712345678.000100",
        ),
        (
            "memory-chat",
            "reviewer",
            "slack.t0123abc.u9xyz",
            "slack",
            "other-transport",
            "c0123abc",
            "c0123abc:1712345678.000100",
        ),
        (
            "memory-chat",
            "reviewer",
            "slack.t0123abc.u9xyz",
            "slack",
            "scientist-slack",
            "c999999",
            "c999999:1712345678.000100",
        ),
        (
            "memory-chat",
            "reviewer",
            "slack.t0123abc.u9xyz",
            "slack",
            "scientist-slack",
            "c0123abc",
            "c0123abc:1712345678.000200",
        ),
    ];
    for (index, (provider, agent, subject, kind, transport, channel, conversation)) in
        cases.into_iter().enumerate()
    {
        let grant = host
            .grant(StorageGrantRequest::new(
                format!("reader-{index}").parse().expect("invocation"),
                "memory.chat.recent".parse().expect("capability"),
                provider.parse().expect("provider"),
                StorageInterface::Jsonl,
                StorageAccess::ReadOnly,
                StorageNamespace::Chat,
                agent.parse().expect("agent"),
                subject.parse().expect("subject"),
                kind,
                transport,
                channel,
                conversation,
                ContinuityPolicy::Stable,
                b"authority".to_vec(),
            ))
            .expect("reader grant");
        let mut transaction = host.begin(grant).expect("reader transaction");
        assert!(matches!(
            transaction.jsonl_size("turns.jsonl"),
            Err(StorageHostError::NotFound)
        ));
        transaction.finish_read().expect("finish read");
    }
}

#[test]
fn tighter_file_and_namespace_limits_rotate_away_from_valid_historical_bytes() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut old = host
        .begin(
            host.grant(request(
                b"old-limits",
                ContinuityPolicy::AuthorityBound,
                "old-limit-write",
                StorageAccess::ReadWrite,
            ))
            .expect("old grant"),
        )
        .expect("old transaction");
    let record = format!(r#"{{"text":"{}"}}"#, "x".repeat(2_000));
    old.jsonl_append("turns.jsonl", 0, record.as_bytes())
        .expect("old limits admit the file");
    old.commit().expect("old commit");
    drop(host);

    let limits = StorageLimits {
        max_namespace_bytes: 20 * 1024,
        max_file_bytes: 1_024,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("historical quota is not corruption");
    let mut current = host
        .begin(
            host.grant(request(
                b"new-limits",
                ContinuityPolicy::AuthorityBound,
                "new-limit-read",
                StorageAccess::ReadOnly,
            ))
            .expect("new authority rotates"),
        )
        .expect("new empty generation fits tighter limits");
    assert!(matches!(
        current.jsonl_size("turns.jsonl"),
        Err(StorageHostError::NotFound)
    ));
    current.finish_read().expect("finish");
}

#[test]
fn explicit_stable_continuity_survives_authority_surface_changes() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let grant = host
        .grant(request(
            b"authority-a",
            ContinuityPolicy::Stable,
            "stable-write",
            StorageAccess::ReadWrite,
        ))
        .expect("write grant");
    let mut transaction = host.begin(grant).expect("write transaction");
    transaction
        .jsonl_append("turns.jsonl", 0, br#"{"stable":true}"#)
        .expect("append");
    transaction.commit().expect("commit");

    let grant = host
        .grant(request(
            b"authority-b",
            ContinuityPolicy::Stable,
            "stable-read",
            StorageAccess::ReadOnly,
        ))
        .expect("read grant");
    let mut transaction = host.begin(grant).expect("read transaction");
    assert!(transaction.jsonl_size("turns.jsonl").is_ok());
    transaction.finish_read().expect("finish");
}

#[test]
fn exact_file_quota_succeeds_and_one_byte_overflow_mutates_nothing() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_file_bytes: 11,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let grant = host
        .grant(request(
            b"authority",
            ContinuityPolicy::Stable,
            "quota-write",
            StorageAccess::ReadWrite,
        ))
        .expect("grant");
    let mut transaction = host.begin(grant).expect("transaction");
    assert_eq!(
        transaction
            .jsonl_append("turns.jsonl", 0, br#"{"turn":1}"#)
            .expect("exact bound"),
        11
    );
    transaction.commit().expect("commit exact bound");

    let grant = host
        .grant(request(
            b"authority",
            ContinuityPolicy::Stable,
            "quota-deny",
            StorageAccess::ReadWrite,
        ))
        .expect("grant");
    let mut transaction = host.begin(grant).expect("transaction");
    assert!(matches!(
        transaction.jsonl_append("turns.jsonl", 11, b"0"),
        Err(StorageHostError::QuotaExceeded)
    ));
    transaction.abort();

    let grant = host
        .grant(request(
            b"authority",
            ContinuityPolicy::Stable,
            "quota-read",
            StorageAccess::ReadOnly,
        ))
        .expect("grant");
    let mut transaction = host.begin(grant).expect("transaction");
    assert_eq!(
        transaction
            .jsonl_size("turns.jsonl")
            .expect("old file remains"),
        11
    );
    transaction.finish_read().expect("finish");
}

#[test]
fn root_initialization_quota_denial_creates_no_layout_entry() {
    let (_temporary, root, key) = fixture();
    let parent = root.parent().expect("root parent");
    let before = tree_snapshot(parent);
    // The three root entries fit; the layout document's own bytes do not.
    let limits = StorageLimits {
        max_root_bytes: 3 * 4_096,
        max_namespace_bytes: 8 * 1024,
        max_file_bytes: 1,
        max_files_per_namespace: 1,
        startup_max_entries: 9,
        ..StorageLimits::default()
    };
    assert!(matches!(
        StorageHost::open(&root, &key, limits),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert!(!root.exists());
    assert_eq!(tree_snapshot(parent), before);
}

#[test]
fn namespace_housekeeping_quota_denial_precedes_every_mutation() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        // Generation directory, data directory and lease: one byte below their live peak.
        max_namespace_bytes: 3 * 4_096 - 1,
        max_file_bytes: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let before = tree_snapshot(&root);
    assert!(matches!(
        host.grant(request(
            b"authority",
            ContinuityPolicy::Stable,
            "namespace-housekeeping-denied",
            StorageAccess::ReadOnly,
        )),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(before, tree_snapshot(&root));
    assert_eq!(
        fs::read_dir(root.join("namespaces"))
            .expect("namespace root")
            .count(),
        0
    );
}

#[test]
fn read_only_vfs_rejects_every_write_bearing_open_before_a_handle_exists() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut writer = host
        .begin(
            host.grant(vfs_request("vfs-seed", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let handle = writer
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("open");
    writer.vfs_write_at(handle, 0, b"seed").expect("write");
    writer.vfs_close(handle).expect("close");
    writer.commit().expect("commit");

    for (index, options) in [
        OpenOptions {
            read: true,
            write: true,
            ..OpenOptions::default()
        },
        OpenOptions {
            read: true,
            write: true,
            delete_on_close: true,
            ..OpenOptions::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut reader = host
            .begin(
                host.grant(vfs_request(
                    &format!("vfs-read-only-{index}"),
                    StorageAccess::ReadOnly,
                ))
                .expect("grant"),
            )
            .expect("transaction");
        assert!(matches!(
            reader.vfs_open("main.db", options),
            Err(StorageHostError::PermissionDenied)
        ));
        assert_eq!(reader.open_handle_count(), 0);
        reader.abort();
    }
}

#[test]
fn logical_names_reject_traversal_and_separators_without_creating_data_entries() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut transaction = host
        .begin(
            host.grant(request(
                b"authority",
                ContinuityPolicy::Stable,
                "invalid-logical-names",
                StorageAccess::ReadWrite,
            ))
            .expect("grant"),
        )
        .expect("transaction");
    for name in [
        "../turns.jsonl",
        "/turns.jsonl",
        "nested/turns.jsonl",
        "nested\\turns",
    ] {
        assert!(matches!(
            transaction.jsonl_append(name, 0, br#"{"turn":1}"#),
            Err(StorageHostError::InvalidName)
        ));
    }
    transaction.abort();

    let data_entries = tree_snapshot(&root)
        .into_iter()
        .filter(|(path, _)| path.contains("/data/"))
        .count();
    assert_eq!(
        data_entries, 0,
        "invalid names must not create physical data"
    );
}

#[test]
fn sparse_growth_obeys_the_exact_file_bound_and_one_byte_over_mutates_nothing() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_file_bytes: 16,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let mut exact = host
        .begin(
            host.grant(vfs_request("sparse-exact", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let handle = exact
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                create_new: true,
                ..OpenOptions::default()
            },
        )
        .expect("open");
    exact
        .vfs_write_at(handle, 15, b"x")
        .expect("exact sparse growth");
    assert_eq!(exact.vfs_size(handle).expect("size"), 16);
    exact.vfs_close(handle).expect("close");
    exact.commit().expect("commit exact sparse file");

    let mut denied = host
        .begin(
            host.grant(vfs_request("sparse-over", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let handle = denied
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                ..OpenOptions::default()
            },
        )
        .expect("open existing");
    assert!(matches!(
        denied.vfs_write_at(handle, 16, b"y"),
        Err(StorageHostError::QuotaExceeded)
    ));
    denied.abort();

    let mut reader = host
        .begin(
            host.grant(vfs_request("sparse-read", StorageAccess::ReadOnly))
                .expect("grant"),
        )
        .expect("transaction");
    assert_eq!(
        reader
            .vfs_stat("main.db")
            .expect("stat")
            .expect("file")
            .size,
        16
    );
    reader.finish_read().expect("finish");
}

#[test]
fn a_hard_linked_logical_file_fails_its_own_grant_and_not_the_broker() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut writer = host
        .begin(
            host.grant(request(
                b"authority",
                ContinuityPolicy::Stable,
                "hard-link-seed",
                StorageAccess::ReadWrite,
            ))
            .expect("grant"),
        )
        .expect("transaction");
    writer
        .jsonl_append("turns.jsonl", 0, br#"{"turn":1}"#)
        .expect("append");
    writer.commit().expect("commit");
    drop(host);

    let base = only_base(&root);
    let generation = base.join(generations(&base).remove(0));
    let data = generation.join("data");
    let original = fs::read_dir(&data)
        .expect("data")
        .next()
        .expect("one file")
        .expect("entry")
        .path();
    fs::hard_link(&original, data.join("c".repeat(64))).expect("hard link");

    let (host, startup) = captured(|| StorageHost::open(&root, &key, StorageLimits::default()));
    let host = host.expect("one namespace's hard link does not stop the broker");
    let logged = startup.events_text();
    assert!(logged.contains("storage_root_entry_ignored"), "{logged}");
    assert!(logged.contains("private-file"), "{logged}");
    assert!(logged.contains(&*base.to_string_lossy()), "{logged}");

    // A second link is outside the store's shape, and a fresh generation beside it would fail the
    // same scan, so the grant is refused naming the file and nothing on disk changes.
    let before = tree_snapshot(&root);
    let refused = host.grant(request(
        b"authority",
        ContinuityPolicy::Stable,
        "hard-link-read",
        StorageAccess::ReadOnly,
    ));
    let Err(
        error @ StorageHostError::Corrupt {
            scope: "private-file",
            site: Some(site),
        },
    ) = &refused
    else {
        panic!("expected a private-file refusal naming its site, got {refused:?}");
    };
    assert!(!error.namespace_reset(), "{error}");
    let path = site.path.as_ref().expect("the refusal names the file");
    assert!(path.starts_with(&data), "{}", path.display());
    assert_eq!(
        before,
        tree_snapshot(&root),
        "a refused grant changes nothing"
    );
}

#[test]
fn metadata_calls_do_not_load_whole_files_or_bypass_native_read_memory_bounds() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut writer = host
        .begin(
            host.grant(vfs_request("metadata-seed", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let handle = writer
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("open");
    writer.vfs_write_at(handle, 0, b"12345678").expect("write");
    writer.vfs_close(handle).expect("close");
    writer.commit().expect("commit");
    drop(host);

    let limits = StorageLimits {
        max_read_bytes_per_call: 4,
        max_read_bytes_per_invocation: 4,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("reopen");
    let mut metadata = host
        .begin(
            host.grant(vfs_request("metadata-stat", StorageAccess::ReadOnly))
                .expect("grant"),
        )
        .expect("transaction");
    assert_eq!(
        metadata
            .vfs_stat("main.db")
            .expect("stat")
            .expect("file")
            .size,
        8
    );
    metadata.finish_read().expect("metadata-only finish");

    let mut mutation = host
        .begin(
            host.grant(vfs_request("metadata-mutate", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let handle = mutation
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                ..OpenOptions::default()
            },
        )
        .expect("open metadata only");
    // A write reads nothing, so it is bounded by the write budgets, not by the read ceiling the
    // file's own length would exhaust.
    mutation
        .vfs_write_at(handle, 0, b"x")
        .expect("a write charges the read budget nothing");
    assert_eq!(mutation.vfs_size(handle).expect("size"), 8);
    assert_eq!(
        mutation.vfs_read_at(handle, 0, 4).expect("bounded read"),
        b"x234"
    );
    // Four of the four budgeted bytes are spent; the next read of any length is refused.
    assert!(matches!(
        mutation.vfs_read_at(handle, 4, 1),
        Err(StorageHostError::QuotaExceeded)
    ));
    mutation.abort();
}

/// A SQLite database outlives the invocation that created it and is then extended page by page
/// and read back in short positional reads. Only the bytes an invocation asks for may be charged
/// to its read budget: the pages already on disk, and the pages a write is not supplying, are
/// never pulled into memory, so a database far larger than that budget stays writable,
/// truncatable, renameable, and removable under it.
#[test]
fn a_database_larger_than_the_read_budget_is_extended_and_read_back_in_short_reads() {
    const PAGE: u64 = 4_096;
    const PAGES: u64 = 64;
    const SEEDED: u64 = 32;
    const FILE_BYTES: u64 = PAGE * PAGES;
    const READ_BUDGET: u64 = 8 * 1_024;

    fn page_contents(page: u64) -> Vec<u8> {
        // Never zero, so a sparse hole or a truncated tail cannot read back as valid page bytes.
        let byte = u8::try_from(page % 200 + 1).expect("page byte");
        vec![byte; usize::try_from(PAGE).expect("page size")]
    }

    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_read_bytes_per_call: PAGE,
        max_read_bytes_per_invocation: READ_BUDGET,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");

    let mut creating = host
        .begin(
            host.grant(vfs_request("sqlite-create", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let database = creating
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("create database");
    for page in 0..SEEDED {
        creating
            .vfs_write_at(database, page * PAGE, &page_contents(page))
            .expect("seed page");
    }
    let journal = creating
        .vfs_open(
            "main.db-wal",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("create journal");
    creating
        .vfs_write_at(journal, 0, b"committed")
        .expect("journal write");
    creating.vfs_close(journal).expect("close journal");
    creating.vfs_close(database).expect("close database");
    creating.commit().expect("commit");

    // A fresh invocation reopens a database many times its read budget.
    let mut extending = host
        .begin(
            host.grant(vfs_request("sqlite-extend", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    assert_eq!(
        extending
            .vfs_stat("main.db")
            .expect("stat")
            .expect("present")
            .size,
        SEEDED * PAGE
    );
    let database = extending
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                ..OpenOptions::default()
            },
        )
        .expect("reopen database");
    for page in SEEDED..PAGES {
        extending
            .vfs_write_at(database, page * PAGE, &page_contents(page))
            .expect("append page to a database larger than the read budget");
    }
    assert_eq!(extending.vfs_size(database).expect("size"), FILE_BYTES);
    assert_eq!(
        extending
            .vfs_read_at(database, 0, u32::try_from(PAGE).expect("page"))
            .expect("first page"),
        page_contents(0)
    );
    assert_eq!(
        extending
            .vfs_read_at(
                database,
                (PAGES - 1) * PAGE,
                u32::try_from(PAGE).expect("page")
            )
            .expect("last page"),
        page_contents(PAGES - 1)
    );
    // Two pages spend the whole read budget: positional reads remain bounded by it.
    assert!(matches!(
        extending.vfs_read_at(database, PAGE, 1),
        Err(StorageHostError::QuotaExceeded)
    ));
    extending.vfs_close(database).expect("close database");
    // With the read budget exhausted, entry operations on files far larger than it still apply:
    // neither charges the bytes it moves or unlinks.
    extending
        .vfs_rename_atomic("main.db", "main.db.bak", false, Durability::Full)
        .expect("rename a file larger than the read budget");
    extending
        .vfs_remove("main.db-wal", Durability::Full)
        .expect("remove journal");
    assert_eq!(
        extending
            .vfs_stat("main.db.bak")
            .expect("stat")
            .expect("renamed")
            .size,
        FILE_BYTES
    );
    assert!(extending.vfs_stat("main.db").expect("stat").is_none());
    assert!(extending.vfs_stat("main.db-wal").expect("stat").is_none());
    extending.commit().expect("commit");

    let mut shrinking = host
        .begin(
            host.grant(vfs_request("sqlite-truncate", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let database = shrinking
        .vfs_open(
            "main.db.bak",
            OpenOptions {
                read: true,
                write: true,
                ..OpenOptions::default()
            },
        )
        .expect("reopen renamed database");
    shrinking
        .vfs_truncate(database, PAGE)
        .expect("truncate a file larger than the read budget");
    assert_eq!(shrinking.vfs_size(database).expect("size"), PAGE);
    assert_eq!(
        shrinking
            .vfs_read_at(database, 0, u32::try_from(PAGE).expect("page"))
            .expect("surviving page"),
        page_contents(0)
    );
    shrinking.vfs_close(database).expect("close database");
    shrinking.commit().expect("commit");
}

#[test]
fn counted_resource_drop_cannot_mask_an_exhausted_host_call_budget() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_host_calls_per_invocation: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let mut transaction = host
        .begin(
            host.grant(vfs_request("drop-budget", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let handle = transaction
        .vfs_open(
            "main.db",
            OpenOptions {
                write: true,
                create_new: true,
                ..OpenOptions::default()
            },
        )
        .expect("first and only admitted call");
    assert!(matches!(
        transaction.vfs_close(handle),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(transaction.open_handle_count(), 0);
    transaction.abort();

    let mut invalid = host
        .begin(
            host.grant(vfs_request("invalid-drop-budget", StorageAccess::ReadOnly))
                .expect("grant"),
        )
        .expect("transaction");
    assert!(
        invalid
            .vfs_stat("missing.db")
            .expect("first call")
            .is_none()
    );
    assert!(matches!(
        invalid.vfs_close(999),
        Err(StorageHostError::QuotaExceeded)
    ));
    invalid.abort();
}

#[test]
fn rename_then_recreate_assigns_a_fresh_live_identity() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut transaction = host
        .begin(
            host.grant(vfs_request("vfs-incarnation", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let original = transaction
        .vfs_open(
            "a.db",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("open original");
    transaction.vfs_close(original).expect("close original");
    transaction
        .vfs_rename_atomic("a.db", "b.db", false, Durability::Full)
        .expect("rename");
    let recreated = transaction
        .vfs_open(
            "a.db",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("recreate");
    transaction.vfs_close(recreated).expect("close recreated");
    let a = transaction.vfs_stat("a.db").expect("stat a").expect("a");
    let b = transaction.vfs_stat("b.db").expect("stat b").expect("b");
    assert_ne!(a.identity, b.identity);
    transaction.abort();
}

#[test]
fn pending_lock_blocks_a_new_shared_reader_while_existing_readers_drain() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut transaction = host
        .begin(
            host.grant(vfs_request("vfs-locks", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("transaction");
    let first = transaction
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("first");
    let reopen = OpenOptions {
        read: true,
        write: true,
        ..OpenOptions::default()
    };
    let second = transaction.vfs_open("main.db", reopen).expect("second");
    let third = transaction.vfs_open("main.db", reopen).expect("third");
    transaction
        .vfs_lock(first, LockLevel::Shared)
        .expect("first shared");
    transaction
        .vfs_lock(second, LockLevel::Shared)
        .expect("second shared");
    transaction
        .vfs_lock(first, LockLevel::Reserved)
        .expect("reserved");
    transaction
        .vfs_lock(first, LockLevel::Pending)
        .expect("pending");
    assert!(matches!(
        transaction.vfs_lock(third, LockLevel::Shared),
        Err(StorageHostError::Busy)
    ));
    transaction.vfs_close(first).expect("close first");
    transaction.vfs_close(second).expect("close second");
    transaction.vfs_close(third).expect("close third");
    transaction.abort();
}

#[test]
fn barrier_concurrent_first_grants_publish_one_epoch_and_authority_changes_publish_two() {
    for (surfaces, expected_generations) in [
        (vec![b"same".as_slice(); 8], 1),
        (
            vec![b"authority-a".as_slice(), b"authority-b".as_slice()],
            2,
        ),
    ] {
        let (_temporary, root, key) = fixture();
        let host =
            Arc::new(StorageHost::open(&root, &key, StorageLimits::default()).expect("host"));
        let barrier = Arc::new(Barrier::new(surfaces.len() + 1));
        let workers = surfaces
            .into_iter()
            .enumerate()
            .map(|(index, surface)| {
                let host = Arc::clone(&host);
                let barrier = Arc::clone(&barrier);
                let surface = surface.to_vec();
                thread::spawn(move || {
                    barrier.wait();
                    let grant = host.grant(request(
                        &surface,
                        ContinuityPolicy::AuthorityBound,
                        &format!("first-grant-{index}"),
                        StorageAccess::ReadOnly,
                    ))?;
                    host.begin(grant)?.finish_read().map(|_| ())
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for worker in workers {
            worker.join().expect("worker").expect("concurrent grant");
        }
        let base = fs::read_dir(root.join("namespaces"))
            .expect("namespace")
            .next()
            .expect("one namespace")
            .expect("entry")
            .path();
        assert_eq!(
            fs::read_dir(base)
                .expect("base entries")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .count(),
            expected_generations
        );
    }
}

#[test]
fn same_namespace_serializes_while_a_distinct_namespace_can_overlap() {
    let (_temporary, root, key) = fixture();
    let host = Arc::new(StorageHost::open(&root, &key, StorageLimits::default()).expect("host"));
    let held = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "lease-held",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uone",
        ))
        .expect("held grant");
    let held = host.begin(held).expect("held transaction");

    let (same_send, same_receive) = mpsc::channel();
    let same_host = Arc::clone(&host);
    let same = thread::spawn(move || {
        let result = same_host
            .grant(scoped_request(
                b"authority",
                ContinuityPolicy::Stable,
                "lease-same",
                StorageAccess::ReadOnly,
                "slack.t0123abc.uone",
            ))
            .and_then(|grant| same_host.begin(grant))
            .and_then(|transaction| transaction.finish_read());
        same_send.send(result).expect("send same result");
    });
    assert!(
        same_receive
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "a second grant on one base must wait for the first transaction"
    );

    let (other_send, other_receive) = mpsc::channel();
    let other_host = Arc::clone(&host);
    let other = thread::spawn(move || {
        let result = other_host
            .grant(scoped_request(
                b"authority",
                ContinuityPolicy::Stable,
                "lease-distinct",
                StorageAccess::ReadOnly,
                "slack.t0123abc.utwo",
            ))
            .and_then(|grant| other_host.begin(grant))
            .and_then(|transaction| transaction.finish_read());
        other_send.send(result).expect("send distinct result");
    });
    other_receive
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("distinct namespace did not block")
        .expect("distinct namespace succeeds");

    held.finish_read().expect("release held namespace");
    same_receive
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("same namespace resumes")
        .expect("same namespace succeeds after release");
    same.join().expect("same thread");
    other.join().expect("other thread");
}

#[test]
fn concurrent_namespace_cap_is_atomic_and_a_denial_mutates_nothing() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_namespaces: 1,
        ..StorageLimits::default()
    };
    let host = Arc::new(StorageHost::open(&root, &key, limits).expect("host"));
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for (index, subject) in ["slack.t0123abc.uone", "slack.t0123abc.utwo"]
        .into_iter()
        .enumerate()
    {
        let host = Arc::clone(&host);
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            host.grant(scoped_request(
                b"authority",
                ContinuityPolicy::Stable,
                &format!("namespace-race-{index}"),
                StorageAccess::ReadOnly,
                subject,
            ))
            .and_then(|grant| host.begin(grant))
            .and_then(|transaction| transaction.finish_read())
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().expect("thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StorageHostError::QuotaExceeded)))
            .count(),
        1
    );
    assert_eq!(
        fs::read_dir(root.join("namespaces"))
            .expect("namespaces")
            .count(),
        1
    );

    let before = tree_snapshot(&root);
    assert!(matches!(
        host.grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "namespace-denied",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uthree",
        )),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(before, tree_snapshot(&root));
}

#[test]
fn concurrent_root_byte_and_entry_caps_are_exact_and_denials_are_byte_identical() {
    let (_seed_temporary, seed_root, seed_key) = fixture();
    let seed_limits = StorageLimits {
        max_namespace_bytes: 32 * 1024,
        max_file_bytes: 1,
        max_files_per_namespace: 1,
        ..StorageLimits::default()
    };
    let seed = StorageHost::open(&seed_root, &seed_key, seed_limits.clone()).expect("seed host");
    let seed_grant = seed
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "root-cap-seed",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uone",
        ))
        .expect("one namespace fits");
    drop(seed_grant);
    drop(seed);
    let exact = logical_tree_usage(&seed_root);

    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_root_bytes: exact.0,
        startup_max_entries: exact.1,
        ..seed_limits
    };
    let host = Arc::new(StorageHost::open(&root, &key, limits).expect("exact-cap host"));
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for (index, subject) in ["slack.t0123abc.uone", "slack.t0123abc.utwo"]
        .into_iter()
        .enumerate()
    {
        let host = Arc::clone(&host);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            host.grant(scoped_request(
                b"authority",
                ContinuityPolicy::Stable,
                &format!("root-cap-race-{index}"),
                StorageAccess::ReadOnly,
                subject,
            ))
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StorageHostError::QuotaExceeded)))
            .count(),
        1
    );
    drop(results);
    assert_eq!(logical_tree_usage(&root), exact);

    let before = tree_snapshot(&root);
    assert!(matches!(
        host.grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "root-cap-plus-one",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uthree",
        )),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(tree_snapshot(&root), before);
    assert_eq!(logical_tree_usage(&root), exact);
}

#[test]
fn stable_reactivation_survives_restart() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits.clone()).expect("host");
    let stable = host
        .grant(request(
            b"authority-a",
            ContinuityPolicy::Stable,
            "stable-reactivation-write",
            StorageAccess::ReadWrite,
        ))
        .expect("stable grant");
    let mut stable = host.begin(stable).expect("stable transaction");
    stable
        .jsonl_append("turns.jsonl", 0, br#"{"stable":true}"#)
        .expect("stable append");
    stable.commit().expect("stable commit");

    drop(
        host.grant(request(
            b"authority-b",
            ContinuityPolicy::AuthorityBound,
            "stable-reactivation-bound",
            StorageAccess::ReadOnly,
        ))
        .expect("authority-bound generation"),
    );
    let reactivated = host
        .grant(request(
            b"authority-c",
            ContinuityPolicy::Stable,
            "stable-reactivation-return",
            StorageAccess::ReadOnly,
        ))
        .expect("stable reactivation");
    host.begin(reactivated)
        .expect("reactivated transaction")
        .finish_read()
        .expect("reactivated read");

    thread::sleep(std::time::Duration::from_millis(5));
    let reader = host
        .grant(request(
            b"authority-d",
            ContinuityPolicy::Stable,
            "stable-reactivation-read",
            StorageAccess::ReadOnly,
        ))
        .expect("stable survived GC");
    let mut reader = host.begin(reader).expect("reader");
    assert_eq!(reader.jsonl_size("turns.jsonl").expect("stable data"), 16);
    reader.finish_read().expect("finish reader");
    drop(host);

    let host = StorageHost::open(&root, &key, limits).expect("restart");
    let reader = host
        .grant(request(
            b"authority-e",
            ContinuityPolicy::Stable,
            "stable-reactivation-restart",
            StorageAccess::ReadOnly,
        ))
        .expect("stable survived restart");
    let mut reader = host.begin(reader).expect("restart reader");
    assert_eq!(reader.jsonl_size("turns.jsonl").expect("stable data"), 16);
    reader.finish_read().expect("finish restart reader");
}

#[test]
fn a_corrupt_authority_pointer_resets_the_namespace_once() {
    let (_temporary, root, key) = fixture();
    let first_scope = |invocation: &str, access| {
        scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            invocation,
            access,
            "slack.t0123abc.uone",
        )
    };
    let second_scope = |invocation: &str, access| {
        scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            invocation,
            access,
            "slack.t0123abc.utwo",
        )
    };
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut first = host
        .begin(
            host.grant(first_scope("corrupt-first", StorageAccess::ReadWrite))
                .expect("first grant"),
        )
        .expect("first transaction");
    first
        .jsonl_append("turns.jsonl", 0, br#"{"scope":"first"}"#)
        .expect("first append");
    first.commit().expect("first commit");
    let first_base = only_base(&root);
    let mut second = host
        .begin(
            host.grant(second_scope("corrupt-second", StorageAccess::ReadWrite))
                .expect("second grant"),
        )
        .expect("second transaction");
    second
        .jsonl_append("turns.jsonl", 0, br#"{"scope":"second"}"#)
        .expect("second append");
    second.commit().expect("second commit");
    drop(host);

    // Still a private single-link JSON document naming a real epoch, so nothing but the pointer
    // check itself can object to it.
    let pointer = first_base.join("current");
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&pointer).expect("pointer")).expect("pointer document");
    document["authority"] = serde_json::Value::from("not-a-token");
    fs::write(&pointer, serde_json::to_vec(&document).expect("encode")).expect("corrupt pointer");
    let token = first_base
        .file_name()
        .expect("base token")
        .to_string_lossy()
        .into_owned();
    let [previous] = <[String; 1]>::try_from(generations(&first_base)).expect("one generation");

    let host = StorageHost::open(&root, &key, StorageLimits::default())
        .expect("a corrupt namespace does not stop the broker");

    let (refused, log) =
        captured(|| host.grant(first_scope("corrupt-reset", StorageAccess::ReadOnly)));
    let error = refused.expect_err("the invocation that finds the corruption fails");
    assert!(error.namespace_reset(), "{error}");
    let StorageHostError::Corrupt {
        scope: "authority-pointer",
        site: Some(site),
    } = &error
    else {
        panic!("expected a reset authority-pointer corruption, got {error:?}");
    };
    assert_eq!(site.namespace.as_deref(), Some(token.as_str()));
    assert_eq!(site.generation.as_deref(), Some(previous.as_str()));
    assert_eq!(site.path.as_deref(), Some(pointer.as_path()));
    let fresh = site.reset.as_ref().expect("the fresh generation is named");
    assert_ne!(*fresh, previous);
    let logged = log.events_text();
    assert_eq!(
        logged.matches("storage_namespace_reset").count(),
        1,
        "{logged}"
    );
    for field in [
        token.as_str(),
        previous.as_str(),
        fresh.as_str(),
        "authority-pointer",
    ] {
        assert!(logged.contains(field), "{field} missing from {logged}");
    }
    // The operator reads one rendered chain, so the location has to survive Display.
    let rendered = error_chain(&error);
    assert!(rendered.contains(&token), "{rendered}");
    assert!(rendered.contains("authority-pointer"), "{rendered}");

    // The next invocation lands on the fresh generation, and the corrupt one is still on disk.
    let (retried, log) = captured(|| {
        let mut retried = host
            .begin(
                host.grant(first_scope("corrupt-retry", StorageAccess::ReadOnly))
                    .expect("the retry is granted"),
            )
            .expect("retry transaction");
        let size = retried.jsonl_size("turns.jsonl");
        retried.finish_read().expect("finish retry");
        size
    });
    assert!(
        matches!(retried, Err(StorageHostError::NotFound)),
        "{retried:?}"
    );
    assert!(!log.saw("storage_namespace_reset"), "the reset repeated");
    let kept = generations(&first_base);
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert!(kept.contains(&previous) && kept.contains(fresh), "{kept:?}");

    // The neighbour never noticed.
    let mut neighbour = host
        .begin(
            host.grant(second_scope("neighbour-read", StorageAccess::ReadOnly))
                .expect("neighbour grant"),
        )
        .expect("neighbour transaction");
    assert_eq!(
        neighbour.jsonl_size("turns.jsonl").expect("neighbour data"),
        19
    );
    neighbour.finish_read().expect("finish neighbour");
}

#[test]
fn a_corrupt_stable_generation_is_moved_aside_and_reset() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let mut writer = host
        .begin(
            host.grant(request(
                b"authority",
                ContinuityPolicy::Stable,
                "stable-seed",
                StorageAccess::ReadWrite,
            ))
            .expect("grant"),
        )
        .expect("transaction");
    writer
        .jsonl_append("turns.jsonl", 0, br#"{"turn":1}"#)
        .expect("append");
    writer.commit().expect("commit");

    let base = only_base(&root);
    let [stable] = <[String; 1]>::try_from(generations(&base)).expect("one generation");
    // A private file, so the base scan accepts it; only the logical-name check refuses the name.
    let stray = base.join(&stable).join("data").join("not-a-token");
    fs::write(&stray, b"stray").expect("stray logical entry");
    fs::set_permissions(&stray, fs::Permissions::from_mode(0o600)).expect("stray mode");

    let refused = host.grant(request(
        b"authority",
        ContinuityPolicy::Stable,
        "stable-reset",
        StorageAccess::ReadOnly,
    ));
    let Err(
        error @ StorageHostError::Corrupt {
            scope: "logical-token",
            site: Some(site),
        },
    ) = &refused
    else {
        panic!("expected a reset logical-token corruption, got {refused:?}");
    };
    assert!(error.namespace_reset());
    assert_eq!(site.generation.as_deref(), Some(stable.as_str()));
    // Stable continuity keeps its deterministic name: the fresh generation takes it, and the
    // corrupt one moved to a new token beside it.
    assert_eq!(site.reset.as_deref(), Some(stable.as_str()));
    let kept = generations(&base);
    assert_eq!(kept.len(), 2, "{kept:?}");
    let aside = kept
        .iter()
        .find(|name| **name != stable)
        .expect("the set-aside generation");
    assert!(base.join(aside).join("data").join("not-a-token").exists());

    let mut retried = host
        .begin(
            host.grant(request(
                b"authority",
                ContinuityPolicy::Stable,
                "stable-retry",
                StorageAccess::ReadOnly,
            ))
            .expect("the retry is granted"),
        )
        .expect("retry transaction");
    assert!(matches!(
        retried.jsonl_size("turns.jsonl"),
        Err(StorageHostError::NotFound)
    ));
    retried.finish_read().expect("finish retry");
}

#[test]
fn symlink_substitution_is_never_followed_at_any_namespace_tree_level() {
    for level in ["base", "generation", "data", "logical-file"] {
        let (temporary, root, key) = fixture();
        let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
        let mut transaction = host
            .begin(
                host.grant(request(
                    b"authority",
                    ContinuityPolicy::Stable,
                    &format!("substitution-{level}"),
                    StorageAccess::ReadWrite,
                ))
                .expect("grant"),
            )
            .expect("transaction");
        transaction
            .jsonl_append("turns.jsonl", 0, br#"{"turn":1}"#)
            .expect("append");
        transaction.commit().expect("commit");
        drop(host);

        let base = fs::read_dir(root.join("namespaces"))
            .expect("base")
            .next()
            .expect("one base")
            .expect("entry")
            .path();
        let generation = fs::read_dir(&base)
            .expect("generation")
            .filter_map(Result::ok)
            .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .expect("generation entry")
            .path();
        let data = generation.join("data");
        let logical = fs::read_dir(&data)
            .expect("logical file")
            .next()
            .expect("one logical file")
            .expect("entry")
            .path();
        let victim = match level {
            "base" => base,
            "generation" => generation,
            "data" => data,
            "logical-file" => logical,
            _ => unreachable!(),
        };
        let replacement = temporary.path().join(format!("substituted-{level}"));
        fs::rename(&victim, &replacement).expect("move trusted entry out of tree");
        symlink(&replacement, &victim).expect("substitute symlink");
        let outside = snapshot_tree(temporary.path())
            .into_iter()
            .filter(|entry| !entry.relative.starts_with("storage"))
            .map(|entry| (entry.relative, entry.contents))
            .collect::<Vec<_>>();

        let host = StorageHost::open(&root, &key, StorageLimits::default())
            .unwrap_or_else(|error| panic!("level {level} stopped the broker: {error}"));
        let refused = host.grant(request(
            b"authority",
            ContinuityPolicy::Stable,
            &format!("substitution-{level}-read"),
            StorageAccess::ReadOnly,
        ));
        assert!(
            matches!(&refused, Err(error @ StorageHostError::Corrupt { .. }) if !error.namespace_reset()),
            "level {level} was followed or reset instead of refused: {refused:?}"
        );
        assert_eq!(
            outside,
            snapshot_tree(temporary.path())
                .into_iter()
                .filter(|entry| !entry.relative.starts_with("storage"))
                .map(|entry| (entry.relative, entry.contents))
                .collect::<Vec<_>>(),
            "level {level}: the symlink target changed"
        );
    }
}

#[test]
fn configured_root_and_key_ancestor_symlinks_are_rejected_before_canonicalization() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let directory = temporary.path().canonicalize().expect("canonical tempdir");
    let actual_parent = directory.join("actual-parent");
    fs::create_dir(&actual_parent).expect("actual parent");
    fs::set_permissions(&actual_parent, fs::Permissions::from_mode(0o700)).expect("parent mode");
    let alias_parent = directory.join("alias-parent");
    symlink(&actual_parent, &alias_parent).expect("ancestor symlink");
    let key = directory.join("key.yaml");
    write_key(&key);

    assert!(matches!(
        StorageHost::open(alias_parent.join("storage"), &key, StorageLimits::default()),
        Err(StorageHostError::RootIo { .. } | StorageHostError::UnsafeRoot { .. })
    ));
    assert!(!actual_parent.join("storage").exists());

    let actual_key_parent = directory.join("actual-key-parent");
    fs::create_dir(&actual_key_parent).expect("key parent");
    fs::set_permissions(&actual_key_parent, fs::Permissions::from_mode(0o700))
        .expect("key parent mode");
    let actual_key = actual_key_parent.join("key.yaml");
    write_key(&actual_key);
    let key_alias = directory.join("key-alias");
    symlink(&actual_key_parent, &key_alias).expect("key ancestor symlink");
    assert!(matches!(
        StorageHost::open(
            directory.join("safe-storage"),
            key_alias.join("key.yaml"),
            StorageLimits::default()
        ),
        Err(StorageHostError::KeyIo { .. } | StorageHostError::UnsafeKeyFile { .. })
    ));
}

#[test]
fn root_and_root_layout_symlinks_fail_globally_without_being_followed() {
    let (temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    drop(host);
    let actual = temporary.path().join("actual-root");
    fs::rename(&root, &actual).expect("move root");
    symlink(&actual, &root).expect("root symlink");
    assert!(StorageHost::open(&root, &key, StorageLimits::default()).is_err());

    fs::remove_file(&root).expect("remove root symlink");
    fs::rename(&actual, &root).expect("restore root");
    let namespaces = root.join("namespaces");
    let actual_namespaces = temporary.path().join("actual-namespaces");
    fs::rename(&namespaces, &actual_namespaces).expect("move namespaces");
    symlink(&actual_namespaces, &namespaces).expect("namespace-root symlink");
    assert!(StorageHost::open(&root, &key, StorageLimits::default()).is_err());
}

#[test]
fn key_symlinks_fail_closed() {
    let (_temporary, root, key) = fixture();
    let key_link = key.with_file_name("key-link.yaml");
    symlink(&key, &key_link).expect("key symlink");
    assert!(StorageHost::open(&root, &key_link, StorageLimits::default()).is_err());
}

#[test]
fn initialized_root_never_recreates_missing_layout_entries_or_accepts_unknown_ones() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    drop(host);

    fs::remove_dir(root.join("namespaces")).expect("remove required directory");
    let before = tree_snapshot(&root);
    assert!(matches!(
        StorageHost::open(&root, &key, StorageLimits::default()),
        Err(StorageHostError::CorruptLayout { .. })
    ));
    assert_eq!(
        before,
        tree_snapshot(&root),
        "startup must not repair data loss"
    );

    fs::create_dir(root.join("namespaces")).expect("restore required directory");
    fs::set_permissions(root.join("namespaces"), fs::Permissions::from_mode(0o700))
        .expect("namespace directory mode");
    let unknown = root.join("unknown-root-entry");
    fs::write(&unknown, b"unknown").expect("write unknown root entry");
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o600)).expect("unknown mode");
    assert!(matches!(
        StorageHost::open(&root, &key, StorageLimits::default()),
        Err(StorageHostError::CorruptLayout { .. })
    ));
}

#[test]
fn a_root_retaining_the_removed_quarantine_directory_is_refused() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    drop(host);

    // Bytes an earlier release set aside are the operator's to keep or delete, so a quarantine
    // that still holds any refuses startup, naming the directory, and startup touches nothing.
    let retired = root.join("quarantine");
    fs::create_dir(&retired).expect("retired quarantine directory");
    fs::set_permissions(&retired, fs::Permissions::from_mode(0o700)).expect("retired mode");
    fs::write(retired.join("set-aside"), b"kept").expect("quarantined bytes");
    let before = tree_snapshot(&root);
    let refused = StorageHost::open(&root, &key, StorageLimits::default());
    let Err(StorageHostError::CorruptLayout { path }) = &refused else {
        panic!("expected a corrupt layout naming quarantine, got {refused:?}");
    };
    assert_eq!(*path, retired);
    assert_eq!(
        before,
        tree_snapshot(&root),
        "a refused root keeps every byte it had"
    );
}

#[test]
fn an_empty_retired_quarantine_directory_is_removed_at_startup() {
    let (_temporary, root, key) = fixture();
    drop(StorageHost::open(&root, &key, StorageLimits::default()).expect("host"));

    // Every root an earlier release initialized holds one, empty unless something was set aside.
    let retired = root.join("quarantine");
    fs::create_dir(&retired).expect("retired quarantine directory");
    fs::set_permissions(&retired, fs::Permissions::from_mode(0o700)).expect("retired mode");
    let (host, log) = captured(|| StorageHost::open(&root, &key, StorageLimits::default()));
    host.expect("an empty quarantine is not retained data");
    assert!(!retired.exists());
    assert!(
        log.saw("storage_quarantine_removed"),
        "{}",
        log.events_text()
    );
}

/// Every path under `root` paired with its file contents, directories carrying empty contents.
fn tree_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    snapshot_tree(root)
        .into_iter()
        .map(|entry| {
            (
                entry.relative.to_string_lossy().into_owned(),
                entry.contents,
            )
        })
        .collect()
}

/// The logical bytes and entries a namespace tree occupies, as the quota ledger counts them.
fn logical_tree_usage(root: &Path) -> (u64, u64) {
    let entries = snapshot_tree(root);
    let bytes = entries
        .iter()
        .map(|entry| 4_096 + if entry.is_dir { 0 } else { entry.len })
        .sum();
    (bytes, entries.len() as u64)
}

#[test]
fn empty_positional_growth_matches_live_reads_stat_and_reopen() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_file_bytes: 8,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let mut writer = host
        .begin(
            host.grant(vfs_request("empty-growth", StorageAccess::ReadWrite))
                .expect("grant"),
        )
        .expect("writer");
    let handle = writer
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                write: true,
                create: true,
                ..OpenOptions::default()
            },
        )
        .expect("open");
    writer
        .vfs_write_at(handle, 8, b"")
        .expect("empty sparse growth");
    assert_eq!(writer.vfs_size(handle).expect("size"), 8);
    assert_eq!(
        writer
            .vfs_stat("main.db")
            .expect("stat")
            .expect("file")
            .size,
        8
    );
    assert_eq!(
        writer.vfs_read_at(handle, 0, 8).expect("live zeros"),
        vec![0; 8]
    );
    let before = tree_snapshot(&root);
    assert!(matches!(
        writer.vfs_write_at(handle, 9, b""),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(tree_snapshot(&root), before);
    writer.abort();
    let mut reader = host
        .begin(
            host.grant(vfs_request("empty-reopen", StorageAccess::ReadOnly))
                .expect("grant"),
        )
        .expect("reader");
    let handle = reader
        .vfs_open(
            "main.db",
            OpenOptions {
                read: true,
                ..OpenOptions::default()
            },
        )
        .expect("reopen");
    assert_eq!(reader.vfs_size(handle).expect("reopened size"), 8);
    assert_eq!(
        reader
            .vfs_stat("main.db")
            .expect("stat")
            .expect("file")
            .size,
        8
    );
    assert_eq!(
        reader.vfs_read_at(handle, 0, 8).expect("reopened zeros"),
        vec![0; 8]
    );
    reader.vfs_close(handle).expect("close reopened handle");
    reader.finish_read().expect("finish");
}

#[test]
fn b1_original_load_budget_and_write_growth_are_independent() {
    let (_temporary, root, key) = fixture();
    let storage = StorageHost::open(&root, &key, StorageLimits::default()).expect("seed host");
    let grant = storage
        .grant(request(
            b"b1",
            ContinuityPolicy::Stable,
            "b1-seed",
            StorageAccess::ReadWrite,
        ))
        .expect("grant");
    let mut handle = storage.begin(grant).expect("handle");
    for name in ["a.jsonl", "b.jsonl", "c.jsonl"] {
        handle.jsonl_replace(name, 0, b"0\n").expect("seed");
    }
    handle.commit().expect("finish");
    drop(storage);
    let limits = StorageLimits {
        max_read_bytes_per_call: 4,
        max_read_bytes_per_invocation: 4,
        max_write_bytes_per_call: 4,
        max_write_bytes_per_invocation: 12,
        ..StorageLimits::default()
    };
    let storage = StorageHost::open(&root, &key, limits).expect("bounded host");
    let grant = storage
        .grant(request(
            b"b1",
            ContinuityPolicy::Stable,
            "b1-write",
            StorageAccess::ReadWrite,
        ))
        .expect("grant");
    let mut handle = storage.begin(grant).expect("handle");
    assert_eq!(
        handle
            .jsonl_append("a.jsonl", 2, b"100")
            .expect("write at call limit"),
        6
    );
    assert_eq!(
        handle
            .jsonl_append("b.jsonl", 2, b"0")
            .expect("original loads exactly at ceiling"),
        4
    );
    assert_eq!(
        handle
            .jsonl_append("a.jsonl", 6, b"0")
            .expect("load only once"),
        8
    );
    let before = tree_snapshot(&root);
    assert!(matches!(
        handle.jsonl_append("c.jsonl", 2, b"0"),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(
        tree_snapshot(&root),
        before,
        "load ceiling refuses before mutation"
    );
    assert!(matches!(
        handle.jsonl_append("a.jsonl", 8, b"1000"),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(
        tree_snapshot(&root),
        before,
        "per-call write ceiling refuses before mutation"
    );
    assert_eq!(
        handle
            .jsonl_append("a.jsonl", 8, b"0")
            .expect("write exactly at invocation ceiling including denied load charge"),
        10
    );
    let before = tree_snapshot(&root);
    assert!(matches!(
        handle.jsonl_append("a.jsonl", 10, b"0"),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(
        tree_snapshot(&root),
        before,
        "invocation write ceiling refuses before mutation"
    );
    assert_eq!(
        handle
            .jsonl_read_chunk("b.jsonl", 0, 4)
            .expect("independent read request")
            .bytes,
        b"0\n0\n"
    );
    assert!(matches!(
        handle.jsonl_read_chunk("c.jsonl", 0, 1),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(
        tree_snapshot(&root),
        before,
        "read ceiling preserves exact tree"
    );
    assert!(before.iter().any(|(_, bytes)| bytes == b"0\n100\n0\n0\n"));
    assert!(before.iter().any(|(_, bytes)| bytes == b"0\n0\n"));
    assert!(before.iter().any(|(_, bytes)| bytes == b"0\n"));
    handle.commit().expect("finish");
}
