#![cfg(unix)]
#![allow(clippy::unwrap_used)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests own their threads and synchronization"
)]

use std::{
    collections::BTreeMap,
    fs::{self, File, FileTimes},
    time::{Duration, SystemTime},
};

use dekopon_capability::{StorageAccess, StorageInterface, StorageRetention, StorageScope};
use dekopon_storage_host::{
    ContinuityPolicy, RetentionPolicies, StorageGrantRequest, StorageHost, StorageLimits,
};

fn request(
    scope: StorageScope,
    subject: &str,
    agent: &str,
    provider: &str,
    conversation: &str,
    surface: &[u8],
) -> StorageGrantRequest {
    request_at(
        scope,
        subject,
        agent,
        provider,
        "slack",
        "scientist-slack",
        conversation,
        surface,
    )
}

#[allow(clippy::too_many_arguments)]
fn request_at(
    scope: StorageScope,
    subject: &str,
    agent: &str,
    provider: &str,
    kind: &str,
    transport: &str,
    conversation: &str,
    surface: &[u8],
) -> StorageGrantRequest {
    StorageGrantRequest::new(
        "retention-invocation".parse().unwrap(),
        "probe.vfs".parse().unwrap(),
        provider.parse().unwrap(),
        StorageInterface::Jsonl,
        StorageAccess::ReadWrite,
        scope,
        agent.parse().unwrap(),
        subject.parse().unwrap(),
        kind,
        transport,
        "c0123abc",
        conversation,
        if scope == StorageScope::PrivateConversation {
            ContinuityPolicy::AuthorityBound
        } else {
            ContinuityPolicy::Stable
        },
        surface.to_vec(),
    )
}

fn policies(retention: StorageRetention) -> RetentionPolicies {
    BTreeMap::from([(
        ("storage-probe".parse().unwrap(), StorageScope::Agent),
        retention,
    )])
}

fn base(root: &std::path::Path, token: &str) -> std::path::PathBuf {
    root.join("namespaces").join(token)
}

fn logical_usage(directory: &std::path::Path) -> (u64, u64) {
    let mut bytes = 0;
    let mut entries = 0;
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let metadata = entry.metadata().unwrap();
        entries += 1;
        bytes += 4096;
        if metadata.is_dir() {
            let (child_bytes, child_entries) = logical_usage(&entry.path());
            bytes += child_bytes;
            entries += child_entries;
        } else {
            bytes += metadata.len();
        }
    }
    (bytes, entries)
}

fn old_marker(path: &std::path::Path) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(60)))
        .unwrap();
}

#[test]
fn private_shared_and_agent_scopes_have_distinct_resolved_identities() {
    let temporary = tempfile::tempdir().unwrap();
    let host = StorageHost::open(
        temporary.path().canonicalize().unwrap().join("storage"),
        StorageLimits::default(),
    )
    .unwrap();
    let token = |scope, subject, agent, provider, conversation| {
        host.prepare_grant(request(
            scope,
            subject,
            agent,
            provider,
            conversation,
            b"same",
        ))
        .unwrap()
        .namespace()
        .to_owned()
    };
    let private = StorageScope::PrivateConversation;
    let shared = StorageScope::SharedConversation;
    let agent = StorageScope::Agent;
    let a = "slack.t0123abc.u9xyz";
    let b = "slack.t0123abc.u8xyz";
    assert_eq!(
        token(private, a, "reviewer", "storage-probe", "thread-1"),
        "48f374ed95a5c6ae899b727e9d7648d5a9f9443c3f97bc0c2e4134019432380a"
    );
    assert_ne!(
        token(private, a, "reviewer", "storage-probe", "thread-1"),
        token(private, b, "reviewer", "storage-probe", "thread-1")
    );
    assert_eq!(
        token(shared, a, "reviewer", "storage-probe", "thread-1"),
        token(shared, b, "reviewer", "storage-probe", "thread-1")
    );
    assert_ne!(
        token(shared, a, "reviewer", "storage-probe", "thread-1"),
        token(shared, a, "reviewer", "storage-probe", "thread-2")
    );
    assert_eq!(
        token(agent, a, "reviewer", "storage-probe", "thread-1"),
        token(agent, b, "reviewer", "storage-probe", "thread-2")
    );
    assert_ne!(
        token(agent, a, "reviewer", "storage-probe", "thread-1"),
        token(agent, a, "other", "storage-probe", "thread-1")
    );
    assert_ne!(
        token(agent, a, "reviewer", "storage-probe", "thread-1"),
        token(agent, a, "reviewer", "other-probe", "thread-1")
    );
    assert_ne!(
        token(agent, a, "reviewer", "storage-probe", "thread-1"),
        token(shared, a, "reviewer", "storage-probe", "thread-1")
    );
    let cross_transport = |scope| {
        host.prepare_grant(request_at(
            scope,
            "discord.123456789012345678",
            "reviewer",
            "storage-probe",
            "discord",
            "discord-server",
            "other-thread",
            b"same",
        ))
        .unwrap()
        .namespace()
        .to_owned()
    };
    assert_eq!(
        token(agent, a, "reviewer", "storage-probe", "thread-1"),
        cross_transport(agent)
    );
    assert_ne!(
        token(shared, a, "reviewer", "storage-probe", "thread-1"),
        cross_transport(shared)
    );
    let mut first = host
        .begin(
            host.grant(request(
                agent,
                a,
                "reviewer",
                "storage-probe",
                "thread-1",
                b"broad",
            ))
            .unwrap(),
        )
        .unwrap();
    first
        .jsonl_append("shared.jsonl", 0, br#"{"shared":true}"#)
        .unwrap();
    first.commit().unwrap();
    let mut second = host
        .begin(
            host.grant(request(
                agent,
                b,
                "reviewer",
                "storage-probe",
                "thread-2",
                b"narrow",
            ))
            .unwrap(),
        )
        .unwrap();
    assert!(second.jsonl_size("shared.jsonl").unwrap() > 0);
    second.commit().unwrap();
    let mut third = host
        .begin(
            host.grant(request_at(
                agent,
                "discord.123456789012345678",
                "reviewer",
                "storage-probe",
                "discord",
                "discord-server",
                "other-thread",
                b"other-authority",
            ))
            .unwrap(),
        )
        .unwrap();
    assert!(third.jsonl_size("shared.jsonl").unwrap() > 0);
    third.commit().unwrap();
    let mut shared_writer = host
        .begin(
            host.grant(request(
                shared,
                a,
                "reviewer",
                "storage-probe",
                "thread-1",
                b"broad",
            ))
            .unwrap(),
        )
        .unwrap();
    shared_writer
        .jsonl_append("conversation.jsonl", 0, br#"{"shared":true}"#)
        .unwrap();
    shared_writer.commit().unwrap();
    let mut participant = host
        .begin(
            host.grant(request(
                shared,
                b,
                "reviewer",
                "storage-probe",
                "thread-1",
                b"narrow",
            ))
            .unwrap(),
        )
        .unwrap();
    assert!(participant.jsonl_size("conversation.jsonl").unwrap() > 0);
    participant.finish_read().unwrap();
    let mut elsewhere = host
        .begin(
            host.grant(request(
                shared,
                b,
                "reviewer",
                "storage-probe",
                "thread-2",
                b"narrow",
            ))
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        elsewhere.jsonl_size("conversation.jsonl"),
        Err(dekopon_storage_host::StorageHostError::NotFound)
    ));
    elsewhere.finish_read().unwrap();
}

#[test]
fn keep_and_legacy_are_preserved_while_expired_agent_data_is_removed_and_quota_reused() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let limits = StorageLimits {
        max_namespaces: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, limits).unwrap();
    let first = request(
        StorageScope::Agent,
        "slack.t0123abc.u9xyz",
        "reviewer",
        "storage-probe",
        "thread-1",
        b"first",
    );
    let token = host
        .prepare_grant(request(
            StorageScope::Agent,
            "slack.t0123abc.u9xyz",
            "reviewer",
            "storage-probe",
            "thread-1",
            b"first",
        ))
        .unwrap()
        .namespace()
        .to_owned();
    host.begin(host.grant(first).unwrap())
        .unwrap()
        .commit()
        .unwrap();
    old_marker(&base(&root, &token).join("last-used"));
    assert_eq!(host.sweep(&RetentionPolicies::new()).unwrap().deleted, 0);
    assert_eq!(
        host.sweep(&policies(StorageRetention::Keep))
            .unwrap()
            .deleted,
        0
    );
    assert_eq!(
        host.sweep(&policies(StorageRetention::IdleTtl(Duration::from_secs(
            30
        ))))
        .unwrap()
        .deleted,
        1
    );
    assert!(!base(&root, &token).exists());
    let second = request(
        StorageScope::PrivateConversation,
        "slack.t0123abc.u9xyz",
        "reviewer",
        "storage-probe",
        "thread-2",
        b"second",
    );
    host.begin(host.grant(second).unwrap())
        .unwrap()
        .commit()
        .unwrap();
    let private_base = fs::read_dir(root.join("namespaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_file(private_base.join("identity")).unwrap();
    fs::remove_file(private_base.join("last-used")).unwrap();
    assert_eq!(
        host.sweep(&BTreeMap::from([(
            (
                "storage-probe".parse().unwrap(),
                StorageScope::PrivateConversation
            ),
            StorageRetention::IdleTtl(Duration::from_millis(1))
        )]))
        .unwrap()
        .deleted,
        0
    );
    assert!(private_base.exists());
    host.begin(
        host.grant(request(
            StorageScope::PrivateConversation,
            "slack.t0123abc.u9xyz",
            "reviewer",
            "storage-probe",
            "thread-2",
            b"second",
        ))
        .unwrap(),
    )
    .unwrap()
    .finish_read()
    .unwrap();
    assert!(private_base.join("identity").exists());
    assert!(private_base.join("last-used").exists());
}

#[test]
fn admitted_reads_refresh_mtime_and_future_markers_and_busy_leases_are_not_expired() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = StorageHost::open(&root, StorageLimits::default()).unwrap();
    let make = || {
        request(
            StorageScope::Agent,
            "slack.t0123abc.u9xyz",
            "reviewer",
            "storage-probe",
            "thread-1",
            b"same",
        )
    };
    let token = host.prepare_grant(make()).unwrap().namespace().to_owned();
    host.begin(host.grant(make()).unwrap())
        .unwrap()
        .finish_read()
        .unwrap();
    let marker = base(&root, &token).join("last-used");
    old_marker(&marker);
    let old = fs::metadata(&marker).unwrap().modified().unwrap();
    host.begin(host.grant(make()).unwrap())
        .unwrap()
        .finish_read()
        .unwrap();
    assert!(fs::metadata(&marker).unwrap().modified().unwrap() > old);
    let ttl = policies(StorageRetention::IdleTtl(Duration::from_secs(10)));
    assert_eq!(host.sweep(&ttl).unwrap().deleted, 0);
    File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(60)))
        .unwrap();
    let future = fs::metadata(&marker).unwrap().modified().unwrap();
    host.begin(host.grant(make()).unwrap())
        .unwrap()
        .finish_read()
        .unwrap();
    assert!(fs::metadata(&marker).unwrap().modified().unwrap() >= future);
    assert_eq!(host.sweep(&ttl).unwrap().deleted, 0);
    old_marker(&marker);
    let held = host.grant(make()).unwrap();
    old_marker(&marker);
    assert_eq!(host.sweep(&ttl).unwrap().deleted, 0);
    drop(held);
    assert_eq!(host.sweep(&ttl).unwrap().deleted, 1);
}

#[test]
fn missing_or_corrupt_metadata_cannot_authorize_deletion() {
    use std::os::unix::fs::PermissionsExt as _;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = StorageHost::open(&root, StorageLimits::default()).unwrap();
    let make = || {
        request(
            StorageScope::Agent,
            "slack.t0123abc.u9xyz",
            "reviewer",
            "storage-probe",
            "thread-1",
            b"same",
        )
    };
    let token = host.prepare_grant(make()).unwrap().namespace().to_owned();
    host.begin(host.grant(make()).unwrap())
        .unwrap()
        .commit()
        .unwrap();
    let path = base(&root, &token);
    old_marker(&path.join("last-used"));
    let ttl = policies(StorageRetention::IdleTtl(Duration::from_secs(10)));
    fs::remove_file(path.join("last-used")).unwrap();
    assert_eq!(host.sweep(&ttl).unwrap().deleted, 0);
    assert!(matches!(
        host.grant(make()),
        Err(dekopon_storage_host::StorageHostError::Corrupt {
            scope: "storage-resource-metadata",
            ..
        })
    ));
    fs::write(path.join("last-used"), b"not-a-marker").unwrap();
    fs::set_permissions(path.join("last-used"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(host.sweep(&ttl).unwrap().errors, 1);
    let refusal = host.grant(make());
    assert!(
        matches!(
            refusal,
            Err(dekopon_storage_host::StorageHostError::Corrupt {
                scope: "last-used",
                ..
            })
        ),
        "{refusal:?}"
    );
    fs::write(path.join("last-used"), b"").unwrap();
    fs::write(path.join("identity"), b"broken").unwrap();
    assert_eq!(host.sweep(&ttl).unwrap().errors, 1);
    assert!(path.exists());
}

#[test]
fn concurrent_grant_and_sweep_never_recreate_a_path_behind_an_unlinked_lease() {
    use std::sync::{Arc, Barrier};
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let host = StorageHost::open(&root, StorageLimits::default()).unwrap();
    let make = || {
        request(
            StorageScope::Agent,
            "slack.t0123abc.u9xyz",
            "reviewer",
            "storage-probe",
            "thread-1",
            b"same",
        )
    };
    let token = host.prepare_grant(make()).unwrap().namespace().to_owned();
    let ttl = policies(StorageRetention::IdleTtl(Duration::from_secs(10)));
    host.begin(host.grant(make()).unwrap())
        .unwrap()
        .commit()
        .unwrap();
    for _ in 0..20 {
        old_marker(&base(&root, &token).join("last-used"));
        let barrier = Arc::new(Barrier::new(2));
        let worker = host.clone();
        let start = Arc::clone(&barrier);
        let policy = ttl.clone();
        let sweep = std::thread::spawn(move || {
            start.wait();
            worker.sweep(&policy).unwrap()
        });
        barrier.wait();
        host.begin(host.grant(make()).unwrap())
            .unwrap()
            .finish_read()
            .unwrap();
        let _ = sweep.join().unwrap();
        assert!(base(&root, &token).join("base.lock").exists());
        assert!(base(&root, &token).join("identity").exists());
        host.begin(host.grant(make()).unwrap())
            .unwrap()
            .finish_read()
            .unwrap();
    }
}

#[test]
fn partial_sweep_error_preserves_identity_then_recovers_and_releases_quota() {
    use std::os::unix::fs::PermissionsExt as _;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("storage");
    let limits = StorageLimits {
        max_namespaces: 1,
        ..StorageLimits::default()
    };
    let host = StorageHost::open(&root, limits).unwrap();
    let make = |surface: &[u8]| {
        request(
            StorageScope::PrivateConversation,
            "slack.t0123abc.u9xyz",
            "reviewer",
            "storage-probe",
            "thread-1",
            surface,
        )
    };
    let token = host
        .prepare_grant(make(b"a"))
        .unwrap()
        .namespace()
        .to_owned();
    let path = base(&root, &token);
    let mut first_generation = None;
    let mut pointers = Vec::new();
    for surface in [b"a".as_slice(), b"b"] {
        let mut handle = host.begin(host.grant(make(surface)).unwrap()).unwrap();
        handle
            .jsonl_append("turns.jsonl", 0, br#"{"one":true}"#)
            .unwrap();
        handle.commit().unwrap();
        pointers.push(fs::read(path.join("current")).unwrap());
        if first_generation.is_none() {
            first_generation = Some(
                fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|entry| entry.is_dir())
                    .unwrap(),
            );
        }
    }
    let mut generations = fs::read_dir(&path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().unwrap().is_dir())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    generations.sort();
    assert_eq!(generations.len(), 2);
    let (retained_pointer, removed_surface): (&[u8], &[u8]) =
        if generations[0] == first_generation.unwrap() {
            (&pointers[0], b"a")
        } else {
            (&pointers[1], b"b")
        };
    fs::write(path.join("current"), retained_pointer).unwrap();
    let protected = generations[1].join("data");
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o500)).unwrap();
    old_marker(&path.join("last-used"));
    let ttl = BTreeMap::from([(
        (
            "storage-probe".parse().unwrap(),
            StorageScope::PrivateConversation,
        ),
        StorageRetention::IdleTtl(Duration::from_secs(10)),
    )]);
    let result = host.sweep(&ttl).unwrap();
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(result.errors, 1);
    assert!(path.join("identity").exists());
    assert!(path.join("last-used").exists());
    assert_eq!(fs::read(path.join("current")).unwrap(), retained_pointer);
    let reset = host.grant(make(removed_surface)).unwrap_err();
    assert!(reset.namespace_reset(), "{reset:?}");
    old_marker(&path.join("last-used"));
    assert_eq!(host.sweep(&ttl).unwrap().deleted, 1);
    assert!(!path.exists());
    host.begin(
        host.grant(request(
            StorageScope::Agent,
            "slack.t0123abc.u9xyz",
            "reviewer",
            "storage-probe",
            "thread-2",
            b"new",
        ))
        .unwrap(),
    )
    .unwrap()
    .commit()
    .unwrap();
}

#[test]
fn partial_sweep_releases_root_bytes_and_entries_before_retry() {
    use std::os::unix::fs::PermissionsExt as _;

    for ceiling in ["bytes", "entries"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap().join("storage");
        let seed = StorageHost::open(&root, StorageLimits::default()).unwrap();
        let make = |surface: &[u8]| {
            request(
                StorageScope::PrivateConversation,
                "slack.t0123abc.u9xyz",
                "reviewer",
                "storage-probe",
                "thread-1",
                surface,
            )
        };
        let token = seed
            .prepare_grant(make(b"a"))
            .unwrap()
            .namespace()
            .to_owned();
        let path = base(&root, &token);
        let mut first_generation = None;
        let mut pointers = Vec::new();
        for surface in [b"a".as_slice(), b"b"] {
            let mut handle = seed.begin(seed.grant(make(surface)).unwrap()).unwrap();
            handle
                .jsonl_append("turns.jsonl", 0, br#"{"one":true}"#)
                .unwrap();
            handle.commit().unwrap();
            pointers.push(fs::read(path.join("current")).unwrap());
            if first_generation.is_none() {
                first_generation = Some(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path())
                        .find(|entry| entry.is_dir())
                        .unwrap(),
                );
            }
        }
        let mut generations = fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|entry| entry.is_dir())
            .collect::<Vec<_>>();
        generations.sort();
        assert_eq!(generations.len(), 2);
        let current_surface: &[u8] = if generations[1] == first_generation.unwrap() {
            fs::write(path.join("current"), &pointers[0]).unwrap();
            b"a"
        } else {
            b"b"
        };
        let baseline = logical_usage(&root);
        drop(seed);
        let limits = StorageLimits {
            max_root_bytes: if ceiling == "bytes" {
                baseline.0
            } else {
                StorageLimits::default().max_root_bytes
            },
            startup_max_entries: if ceiling == "entries" {
                baseline.1
            } else {
                StorageLimits::default().startup_max_entries
            },
            max_namespaces: 2,
            max_namespace_bytes: 32 * 1024,
            max_file_bytes: 4096,
            max_files_per_namespace: 4,
            ..StorageLimits::default()
        };
        let host = StorageHost::open(&root, limits).unwrap();
        let mut before = host
            .begin(host.grant(make(current_surface)).unwrap())
            .unwrap();
        assert!(
            matches!(
                before.jsonl_append("extra.jsonl", 0, br#"{"extra":true}"#),
                Err(dekopon_storage_host::StorageHostError::QuotaExceeded)
            ),
            "{ceiling}"
        );
        drop(before);

        let protected = generations[1].join("data");
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o500)).unwrap();
        old_marker(&path.join("last-used"));
        let ttl = BTreeMap::from([(
            (
                "storage-probe".parse().unwrap(),
                StorageScope::PrivateConversation,
            ),
            StorageRetention::IdleTtl(Duration::from_secs(10)),
        )]);
        assert_eq!(host.sweep(&ttl).unwrap().errors, 1, "{ceiling}");
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o700)).unwrap();
        let after_partial = logical_usage(&root);
        assert!(
            after_partial.0 < baseline.0 && after_partial.1 < baseline.1,
            "{ceiling}"
        );
        assert!(path.join("current").exists(), "{ceiling}");
        let mut after = host
            .begin(host.grant(make(current_surface)).unwrap())
            .unwrap();
        assert!(after.jsonl_size("turns.jsonl").unwrap() > 0, "{ceiling}");
        after
            .jsonl_append("extra.jsonl", 0, br#"{"extra":true}"#)
            .unwrap();
        after.commit().unwrap();

        old_marker(&path.join("last-used"));
        assert_eq!(host.sweep(&ttl).unwrap().deleted, 1, "{ceiling}");
        assert!(!path.exists(), "{ceiling}");
        host.begin(
            host.grant(request(
                StorageScope::Agent,
                "slack.t0123abc.u9xyz",
                "reviewer",
                "storage-probe",
                "thread-2",
                b"new",
            ))
            .unwrap(),
        )
        .unwrap()
        .commit()
        .unwrap();
    }
}
