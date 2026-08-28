#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::Path,
    sync::{Arc, Barrier, mpsc},
    thread,
};

use dekopon_capability::{StorageAccess, StorageInterface, StorageNamespace};
use dekopon_storage_host::{
    ContinuityPolicy, Durability, LockLevel, OpenOptions, StorageGrantRequest, StorageHost,
    StorageHostError, StorageLimits,
};
use dekopon_test_support::snapshot_tree;
use tempfile::TempDir;

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
    let limits = StorageLimits {
        max_root_bytes: 6 * 4_096,
        max_namespace_bytes: 16 * 1024,
        max_file_bytes: 1,
        max_files_per_namespace: 1,
        startup_max_entries: 9,
        gc_max_bytes_per_pass: 6 * 4_096,
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
        max_namespace_bytes: 4 * 4_096,
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
fn a_hard_linked_logical_file_is_quarantined_instead_of_served() {
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

    let base = fs::read_dir(root.join("namespaces"))
        .expect("base")
        .next()
        .expect("one base")
        .expect("entry")
        .path();
    let generation = fs::read_dir(base)
        .expect("generation")
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .expect("one generation")
        .path();
    let data = generation.join("data");
    let original = fs::read_dir(&data)
        .expect("data")
        .next()
        .expect("one file")
        .expect("entry")
        .path();
    fs::hard_link(&original, data.join("c".repeat(64))).expect("hard link");

    let reopened = StorageHost::open(&root, &key, StorageLimits::default())
        .expect("isolated corruption is quarantined");
    assert_eq!(
        fs::read_dir(root.join("quarantine"))
            .expect("quarantine")
            .count(),
        1
    );
    drop(reopened);
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
    assert!(matches!(
        mutation.vfs_write_at(handle, 0, b"x"),
        Err(StorageHostError::QuotaExceeded)
    ));
    mutation.abort();
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
        gc_max_namespaces_per_pass: 1,
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
        gc_max_bytes_per_pass: exact.0,
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
fn gc_trash_cleanup_never_exceeds_its_per_pass_entry_bound() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        gc_max_namespaces_per_pass: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits.clone()).expect("host");
    drop(host);
    for name in ["stale-a", "stale-b", "stale-c"] {
        let path = root.join("trash").join(name);
        fs::create_dir(&path).expect("trash directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("trash mode");
    }
    let host = StorageHost::open(&root, &key, limits).expect("reopen with accounted trash");
    let report = host.gc_once().expect("bounded GC");
    assert_eq!(report.namespaces_removed, 1);
    assert_eq!(
        fs::read_dir(root.join("trash"))
            .expect("remaining trash")
            .count(),
        2
    );
}

#[test]
fn gc_rotating_trash_cursor_cannot_be_starved_by_retained_unknown_state() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        gc_max_namespaces_per_pass: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let unknown = root
        .join("trash")
        .join(format!("transaction-{}", "a".repeat(64)));
    fs::create_dir(&unknown).expect("unknown transaction");
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o700)).expect("unknown mode");
    let commit = unknown.join("commit");
    fs::write(&commit, b"").expect("commit marker");
    fs::set_permissions(&commit, fs::Permissions::from_mode(0o600)).expect("commit mode");
    for name in ["stale-a", "stale-b"] {
        let path = root.join("trash").join(name);
        fs::create_dir(&path).expect("stale trash");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("stale mode");
    }

    for _ in 0..4 {
        let report = host.gc_once().expect("rotating bounded GC");
        assert!(report.namespaces_removed <= 1);
    }
    assert!(unknown.exists(), "unknown outcome evidence was deleted");
    assert!(!root.join("trash/stale-a").exists());
    assert!(!root.join("trash/stale-b").exists());
}

#[test]
fn later_generic_base_trash_cleanup_reclaims_the_namespace_slot() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_namespaces: 1,
        gc_max_namespaces_per_pass: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits.clone()).expect("host");
    drop(
        host.grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "trash-slot-first",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uone",
        ))
        .expect("first namespace"),
    );
    let base = fs::read_dir(root.join("namespaces"))
        .expect("namespaces")
        .next()
        .expect("base")
        .expect("base entry");
    let token = base.file_name().to_string_lossy().into_owned();
    fs::rename(
        base.path(),
        root.join("trash").join(format!("base-{token}")),
    )
    .expect("simulate interrupted post-rename cleanup");
    drop(host);
    let host = StorageHost::open(&root, &key, limits).expect("restart with base trash");
    assert!(matches!(
        host.grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "trash-slot-still-retained",
            StorageAccess::ReadOnly,
            "slack.t0123abc.utwo",
        )),
        Err(StorageHostError::QuotaExceeded)
    ));
    host.gc_once().expect("generic trash cleanup");
    drop(
        host.grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "trash-slot-second",
            StorageAccess::ReadOnly,
            "slack.t0123abc.utwo",
        ))
        .expect("generic cleanup reclaimed slot"),
    );
}

#[test]
fn gc_removes_an_inactive_base_and_reclaims_its_namespace_slot() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        max_namespaces: 1,
        gc_max_namespaces_per_pass: 1,
        retired_generation_grace_ms: 1,
        retired_generation_ttl_ms: 1,
        inactive_namespace_ttl_ms: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let first = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            "gc-first",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uone",
        ))
        .expect("first grant");
    host.begin(first)
        .expect("first transaction")
        .finish_read()
        .expect("finish");
    thread::sleep(std::time::Duration::from_millis(5));
    let report = host.gc_once().expect("gc");
    assert_eq!(report.namespaces_removed, 1);
    assert_eq!(
        fs::read_dir(root.join("namespaces"))
            .expect("namespaces")
            .count(),
        0
    );
    let second = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            "gc-second",
            StorageAccess::ReadOnly,
            "slack.t0123abc.utwo",
        ))
        .expect("slot was reclaimed");
    host.begin(second)
        .expect("second transaction")
        .finish_read()
        .expect("finish second");
}

#[test]
fn stable_reactivation_clears_retirement_and_survives_gc_and_restart() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        retired_generation_grace_ms: 1,
        retired_generation_ttl_ms: 1,
        inactive_namespace_ttl_ms: 60_000,
        gc_max_namespaces_per_pass: 8,
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
    host.gc_once().expect("bounded GC");
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
fn gc_cannot_unlink_a_base_from_an_already_granted_transaction() {
    let (_temporary, root, key) = fixture();
    let limits = StorageLimits {
        retired_generation_grace_ms: 1,
        retired_generation_ttl_ms: 1,
        inactive_namespace_ttl_ms: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, &key, limits).expect("host");
    let held = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            "gc-held-grant",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uone",
        ))
        .expect("held grant");
    thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(
        host.gc_once().expect("active base is skipped"),
        dekopon_storage_host::GcReport::default()
    );
    host.begin(held)
        .expect("granted base remains linked")
        .finish_read()
        .expect("finish held transaction");
    thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(
        host.gc_once()
            .expect("inactive base collects")
            .namespaces_removed,
        1
    );
}

#[test]
fn corrupt_namespace_is_quarantined_without_blocking_a_healthy_neighbor() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let first = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            "quarantine-first",
            StorageAccess::ReadWrite,
            "slack.t0123abc.uone",
        ))
        .expect("first grant");
    let mut first = host.begin(first).expect("first transaction");
    first
        .jsonl_append("turns.jsonl", 0, br#"{"scope":"first"}"#)
        .expect("first append");
    first.commit().expect("first commit");
    let first_base = fs::read_dir(root.join("namespaces"))
        .expect("bases")
        .next()
        .expect("first base")
        .expect("entry")
        .path();

    let second = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            "quarantine-second",
            StorageAccess::ReadWrite,
            "slack.t0123abc.utwo",
        ))
        .expect("second grant");
    let mut second = host.begin(second).expect("second transaction");
    second
        .jsonl_append("turns.jsonl", 0, br#"{"scope":"second"}"#)
        .expect("second append");
    second.commit().expect("second commit");
    drop(host);

    // A regular, private, single-link file still needs valid schema and a namespace-key MAC. A
    // shallow usage scan alone would accept this isolated corruption and leave the base live.
    fs::write(first_base.join("current"), b"{}\n").expect("corrupt pointer document");
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("reopen");
    assert_eq!(
        fs::read_dir(root.join("quarantine"))
            .expect("quarantine")
            .count(),
        1
    );
    let healthy = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::AuthorityBound,
            "quarantine-read",
            StorageAccess::ReadOnly,
            "slack.t0123abc.utwo",
        ))
        .expect("healthy grant");
    let mut healthy = host.begin(healthy).expect("healthy transaction");
    assert!(healthy.jsonl_size("turns.jsonl").is_ok());
    healthy.finish_read().expect("finish healthy");
}

#[test]
fn inaccessible_namespace_is_quarantined_without_blocking_a_healthy_neighbor() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let first = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "permission-corrupt-first",
            StorageAccess::ReadOnly,
            "slack.t0123abc.uone",
        ))
        .expect("first grant");
    drop(first);
    let first_base = fs::read_dir(root.join("namespaces"))
        .expect("bases")
        .next()
        .expect("first base")
        .expect("entry")
        .path();
    let second = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "permission-corrupt-second",
            StorageAccess::ReadOnly,
            "slack.t0123abc.utwo",
        ))
        .expect("second grant");
    drop(second);
    drop(host);

    fs::set_permissions(&first_base, fs::Permissions::from_mode(0o000))
        .expect("remove base permissions");
    let host = StorageHost::open(&root, &key, StorageLimits::default())
        .expect("one inaccessible namespace is isolated");
    assert_eq!(
        fs::read_dir(root.join("quarantine"))
            .expect("quarantine")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().len() == 64)
            .count(),
        1
    );
    let healthy = host
        .grant(scoped_request(
            b"authority",
            ContinuityPolicy::Stable,
            "permission-corrupt-healthy",
            StorageAccess::ReadOnly,
            "slack.t0123abc.utwo",
        ))
        .expect("healthy peer remains usable");
    host.begin(healthy)
        .expect("healthy transaction")
        .finish_read()
        .expect("healthy read finishes");
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

        let reopened = StorageHost::open(&root, &key, StorageLimits::default())
            .expect("isolated namespace substitution is quarantined");
        assert_eq!(
            fs::read_dir(root.join("quarantine"))
                .expect("quarantine")
                .count(),
            1,
            "level {level} was not isolated"
        );
        drop(reopened);
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
fn quarantined_wrong_mode_directories_retain_their_complete_quota_charge() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    let grant = host
        .grant(request(
            b"authority",
            ContinuityPolicy::Stable,
            "quarantine-accounting",
            StorageAccess::ReadOnly,
        ))
        .expect("grant");
    drop(grant);
    drop(host);

    let base = fs::read_dir(root.join("namespaces"))
        .expect("namespace")
        .next()
        .expect("one namespace")
        .expect("entry")
        .path();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o777)).expect("corrupt base mode");
    let (bytes, _) = logical_tree_usage(&root);
    let limits = StorageLimits {
        max_root_bytes: bytes - 1,
        max_namespace_bytes: 32 * 1024,
        max_file_bytes: 1,
        max_files_per_namespace: 1,
        gc_max_bytes_per_pass: bytes - 1,
        ..StorageLimits::default()
    };
    assert!(matches!(
        StorageHost::open(&root, &key, limits),
        Err(StorageHostError::QuotaExceeded)
    ));
    assert_eq!(
        fs::read_dir(root.join("quarantine"))
            .expect("quarantine")
            .count(),
        1,
        "isolated corruption is moved but never made quota-free"
    );
}

#[test]
fn key_symlinks_and_unknown_transaction_states_fail_closed() {
    let (_temporary, root, key) = fixture();
    let key_link = key.with_file_name("key-link.yaml");
    symlink(&key, &key_link).expect("key symlink");
    assert!(StorageHost::open(&root, &key_link, StorageLimits::default()).is_err());

    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    drop(host);
    let transaction = root.join("transactions").join("a".repeat(64));
    fs::create_dir(&transaction).expect("transaction directory");
    fs::set_permissions(&transaction, fs::Permissions::from_mode(0o700)).expect("transaction mode");
    let unknown = transaction.join("unknown");
    fs::write(&unknown, b"unknown").expect("unknown state");
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o600)).expect("unknown mode");
    assert!(matches!(
        StorageHost::open(&root, &key, StorageLimits::default()),
        Err(StorageHostError::Corrupt { .. })
    ));
}

#[test]
fn recovery_markers_reject_symlink_and_hardlink_substitution() {
    for kind in ["symlink", "hardlink"] {
        let (temporary, root, key) = fixture();
        let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
        drop(host);
        let transaction = root.join("transactions").join("d".repeat(64));
        fs::create_dir(&transaction).expect("transaction directory");
        fs::set_permissions(&transaction, fs::Permissions::from_mode(0o700))
            .expect("transaction mode");
        let external = temporary.path().join(format!("marker-{kind}"));
        fs::write(&external, b"").expect("external marker");
        fs::set_permissions(&external, fs::Permissions::from_mode(0o600)).expect("marker mode");
        match kind {
            "symlink" => symlink(&external, transaction.join("commit")).expect("marker symlink"),
            "hardlink" => {
                fs::hard_link(&external, transaction.join("commit")).expect("marker hardlink")
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            StorageHost::open(&root, &key, StorageLimits::default()),
            Err(StorageHostError::Corrupt { .. })
        ));
    }
}

#[test]
fn an_incomplete_pending_manifest_is_a_recognized_pre_marker_rollback() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    drop(host);

    let token = "b".repeat(64);
    let transaction = root.join("transactions").join(&token);
    fs::create_dir(&transaction).expect("transaction directory");
    fs::set_permissions(&transaction, fs::Permissions::from_mode(0o700)).expect("transaction mode");
    let pending = transaction.join("manifest.pending");
    fs::write(&pending, b"{\"partial\":").expect("partial pending manifest");
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o600)).expect("pending mode");

    let reopened = StorageHost::open(&root, &key, StorageLimits::default())
        .expect("pre-marker recovery rolls back");
    assert!(!root.join("transactions").join(token).exists());
    drop(reopened);
}

#[test]
fn initialized_root_never_recreates_missing_layout_entries_or_accepts_unknown_ones() {
    let (_temporary, root, key) = fixture();
    let host = StorageHost::open(&root, &key, StorageLimits::default()).expect("host");
    drop(host);

    fs::remove_dir(root.join("transactions")).expect("remove required directory");
    let before = tree_snapshot(&root);
    assert!(matches!(
        StorageHost::open(&root, &key, StorageLimits::default()),
        Err(StorageHostError::CorruptLayout)
    ));
    assert_eq!(
        before,
        tree_snapshot(&root),
        "startup must not repair data loss"
    );

    fs::create_dir(root.join("transactions")).expect("restore required directory");
    fs::set_permissions(root.join("transactions"), fs::Permissions::from_mode(0o700))
        .expect("transaction directory mode");
    let unknown = root.join("unknown-root-entry");
    fs::write(&unknown, b"unknown").expect("write unknown root entry");
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o600)).expect("unknown mode");
    assert!(matches!(
        StorageHost::open(&root, &key, StorageLimits::default()),
        Err(StorageHostError::CorruptLayout)
    ));
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
