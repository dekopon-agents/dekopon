use std::{
    io::{BufRead, BufReader, Write as _},
    os::unix::fs::symlink,
    process::{Command, Stdio},
    time::SystemTime,
};

use dekopon_agent::wake::WakeId;
use dekopon_process::JobId;

use super::*;
use crate::{
    config::WakeBounds,
    follow::{FollowHome, FollowTable, FollowTicket},
    journal::FollowMarker,
    store::{
        Acquired, Change, ConversationKey, DurableSettings, DurableStore, FollowConversion,
        FollowRow, JobRow, LegacyInputs, Persistence, StoreError, StoreFailed, SyncMode,
        testing::{MigrationStep, STEPS, SchemaVersion, acquire_with},
    },
    transport::ThreadClaim,
    wake::{
        Anchor, Delivery, DeliveryDisposition, Probe, Replay, Resolved, Verdict, WakeStore,
        store::Schedule,
    },
};

const HOLDER: &str = "DEKOPON_TEST_STORE_HOLDER";

fn settings(path: &Path) -> DurableSettings {
    DurableSettings {
        path: path.to_path_buf(),
        sync: SyncMode::Normal,
    }
}

fn acquire(path: &Path) -> Acquired {
    DurableStore::acquire(&settings(path), &LegacyInputs::default()).expect("the store opens")
}

fn bounds() -> WakeBounds {
    WakeBounds {
        max_per_subject: 8,
        max_subscriptions_per_agent: 64,
        subscription_expiry: Duration::from_secs(14 * 24 * 60 * 60),
        min_interval: Duration::from_secs(1),
        max_horizon: Duration::from_secs(40 * 24 * 60 * 60),
    }
}

fn anchor() -> Anchor {
    Anchor::from_inbound(&wakes::slack_message("anchor"), &journal_route()).expect("anchorable")
}

fn anchor_in(conversation: &str) -> Anchor {
    let mut message = wakes::slack_message("anchor");
    message.conversation.id = conversation.to_owned();
    Anchor::from_inbound(&message, &journal_route()).expect("anchorable")
}

fn restored(acquired: &Acquired, now: SystemTime) -> (Arc<WakeStore>, Replay) {
    let rows = acquired.store.restore(now).expect("the store restores");
    WakeStore::restored(
        Some(&acquired.store),
        rows.wakes.into_iter().map(|(_, row)| row).collect(),
        None,
        rows.interrupted_jobs,
        bounds(),
    )
}

fn once(store: &WakeStore, anchor: Anchor, note: &str, now: SystemTime) -> WakeId {
    store
        .register(
            anchor,
            note.to_owned(),
            now,
            Schedule::Once {
                after: Duration::from_secs(1),
            },
        )
        .expect("registered")
        .id
}

fn due(store: &WakeStore, at: SystemTime) -> Vec<Delivery> {
    store.take_due(at).expect("the store writes").fired
}

fn digest_anchor() -> Anchor {
    Anchor::from_inbound(&owned_slack_message("digest", false), &journal_route())
        .expect("anchorable")
        .as_space()
        .expect("a mapped channel")
}

fn digest(store: &WakeStore, now: SystemTime) -> WakeId {
    store
        .register(
            digest_anchor(),
            "daily".to_owned(),
            now,
            Schedule::Watch {
                probe: Probe::scripted_for_test("echo waiting").into_digest(),
                every: Duration::from_secs(86_400),
                until: Duration::from_secs(3 * 86_400),
            },
        )
        .expect("registered")
        .id
}

fn job(anchor: &Anchor, id: u64, deadline_ms: u64) -> JobRow {
    JobRow {
        id: JobId::new(id),
        anchor: anchor.row(),
        script_head: "sleep 600".to_owned(),
        deadline_ms,
    }
}

fn claim(thread: &str) -> ThreadClaim {
    ThreadClaim::Slack {
        team_id: "t0123abc".to_owned(),
        channel_id: "c0123abc".to_owned(),
        thread_ts: thread.to_owned(),
        user_id: "u0123abc".to_owned(),
    }
}

fn follow_row(key: &str, now: SystemTime) -> FollowRow {
    let at_ms = crate::store::millis(now);
    FollowRow {
        key: key.to_owned(),
        marker: FollowMarker {
            at_ms,
            until_ms: at_ms + 86_400_000,
        },
    }
}

fn legacy_line(id: u32, note: &str, next_at: SystemTime) -> String {
    let mut line = serde_json::to_value(anchor().row()).expect("anchor encodes");
    let object = line.as_object_mut().expect("anchor object");
    object.insert("id".to_owned(), json!(id));
    object.insert("note".to_owned(), json!(note));
    object.insert("nextAtMs".to_owned(), json!(crate::store::millis(next_at)));
    format!("{line}\n")
}

fn legacy(directory: &Path) -> LegacyInputs {
    LegacyInputs {
        wakes: Some(directory.join("wakes.jsonl")),
        journal: Some(directory.join("journal")),
    }
}

fn raw(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).expect("a raw connection opens")
}

#[test]
fn a_crash_between_the_due_commit_and_handling_replays_the_delivery_at_boot() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let id = {
        let acquired = acquire(&path);
        let (store, _replay) = restored(&acquired, now);
        let id = once(&store, anchor(), "call mom", now);
        let fired = due(&store, now + Duration::from_secs(2));
        assert_eq!(fired.len(), 1);
        drop(fired);
        id
    };
    let acquired = acquire(&path);
    let (_store, replay) = restored(&acquired, now);
    let replayed: Vec<WakeId> = replay
        .wakes
        .iter()
        .map(|delivery| delivery.fired.id())
        .collect();
    assert_eq!(replayed, [id]);
}

#[test]
fn a_store_moved_to_a_new_path_restores_every_conversation() {
    let directory = temporary();
    let first = directory.path().join("v1");
    let second = directory.path().join("v2");
    fs::create_dir_all(&first).unwrap();
    symlink("v1", directory.path().join("..data")).unwrap();
    symlink("..data/state.sqlite", directory.path().join("state.sqlite")).unwrap();
    let projected = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&first.join("state.sqlite"));
        let (store, _replay) = restored(&acquired, now);
        once(&store, anchor_in("d0123abc"), "first", now);
        once(&store, anchor_in("d0456def"), "second", now);
    }
    fs::create_dir_all(&second).unwrap();
    fs::copy(first.join("state.sqlite"), second.join("state.sqlite")).unwrap();
    let swap = directory.path().join("..data.swap");
    symlink("v2", &swap).unwrap();
    fs::rename(&swap, directory.path().join("..data")).unwrap();
    fs::remove_dir_all(&first).unwrap();

    let acquired = acquire(&projected);
    let rows = acquired.store.restore(now).unwrap();
    let mut notes: Vec<String> = rows.wakes.into_iter().map(|(_, row)| row.note).collect();
    notes.sort();
    assert_eq!(notes, ["first", "second"]);
    assert_eq!(acquired.store.records_for_test().len(), 2);
}

#[test]
fn a_record_with_nothing_pending_is_deleted_and_a_wait_expires_after_thirty_days() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let acquired = acquire(&path);
    let (store, _replay) = restored(&acquired, now);
    let id = once(&store, anchor(), "short", now);
    assert_eq!(acquired.store.records_for_test().len(), 1);
    store.cancel(anchor().subject(), id).unwrap();
    assert!(acquired.store.records_for_test().is_empty());

    let old = now - Duration::from_secs(31 * 24 * 60 * 60);
    once(&store, anchor(), "stale", old);
    drop(store);
    let rows = acquired.store.restore(now).unwrap();
    assert!(rows.wakes.is_empty());
    assert!(acquired.store.records_for_test().is_empty());
}

#[test]
fn a_wake_registered_before_a_restart_fires_once_from_its_row() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&path);
        let (store, _replay) = restored(&acquired, now);
        once(&store, anchor(), "after the restart", now);
    }
    {
        let acquired = acquire(&path);
        let (store, replay) = restored(&acquired, now);
        assert!(replay.wakes.is_empty());
        let mut fired = due(&store, now + Duration::from_secs(2));
        assert_eq!(fired.len(), 1);
        let delivery = fired.pop().unwrap();
        assert!(
            delivery
                .fired
                .into_inbound()
                .text
                .contains("after the restart")
        );
        delivery
            .ticket
            .settle(DeliveryDisposition::Handled)
            .unwrap();
        assert!(due(&store, now + Duration::from_secs(60)).is_empty());
    }
    let acquired = acquire(&path);
    assert!(acquired.store.restore(now).unwrap().wakes.is_empty());
}

#[test]
fn a_contender_fails_at_once_and_activates_no_agent() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let _owner = acquire(&path);
    let contender = Persistence::at_boot(Some(&settings(&path)), &LegacyInputs::default());
    assert!(matches!(contender, Err(StoreError::Owned { path: owned }) if owned == path));
}

#[test]
fn after_the_owner_exits_a_new_instance_acquires_the_store_and_restores() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let owner = acquire(&path);
    let (store, _replay) = restored(&owner, now);
    once(&store, anchor(), "kept", now);
    assert!(matches!(
        DurableStore::acquire(&settings(&path), &LegacyInputs::default()),
        Err(StoreError::Owned { .. })
    ));
    drop((store, owner));
    let next = acquire(&path);
    assert_eq!(next.store.restore(now).unwrap().wakes.len(), 1);
}

#[test]
fn one_connection_serializes_every_write_so_a_row_has_one_writer() {
    let directory = temporary();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let key = anchor().key();
    std::thread::scope(|scope| {
        for index in 0..8_u64 {
            let (store, key) = (&acquired.store, key.clone());
            scope.spawn(move || {
                store
                    .commit(vec![(
                        key,
                        Change::JobStarted(job(&anchor(), index + 1, 1)),
                    )])
                    .unwrap();
            });
        }
    });
    let snapshot = acquired.store.snapshot_for_test(&key).unwrap();
    assert_eq!(snapshot.jobs.len(), 8);
}

#[test]
fn ownership_is_observed_before_activation_on_a_store_that_needs_no_migration() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    drop(acquire(&path));
    let owner = acquire(&path);
    assert!(owner.report.migration.is_none());
    assert_eq!(owner.report.schema, SchemaVersion::CURRENT);
    assert!(matches!(
        DurableStore::acquire(&settings(&path), &LegacyInputs::default()),
        Err(StoreError::Owned { .. })
    ));
}

#[test]
fn an_interrupted_conversion_retries_and_converts_each_wake_once() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let lines = format!(
        "{}{}",
        legacy_line(1, "first", now),
        legacy_line(2, "second", now)
    );
    fs::write(
        directory.path().join("wakes.jsonl"),
        format!("{lines}{{\"id\":\n"),
    )
    .unwrap();
    assert!(matches!(
        DurableStore::acquire(&settings(&path), &legacy(directory.path())),
        Err(StoreError::LegacyWakes { line: 3, .. })
    ));
    fs::write(directory.path().join("wakes.jsonl"), &lines).unwrap();
    {
        let acquired = DurableStore::acquire(&settings(&path), &legacy(directory.path())).unwrap();
        let conversion = acquired.report.conversion.expect("converted");
        assert_eq!(conversion.wakes, 2);
        assert_eq!(acquired.store.restore(now).unwrap().wakes.len(), 2);
    }
    let acquired = DurableStore::acquire(&settings(&path), &legacy(directory.path())).unwrap();
    assert!(acquired.report.conversion.is_none());
    assert_eq!(acquired.store.restore(now).unwrap().wakes.len(), 2);
}

#[test]
fn a_converted_store_keeps_wakes_jsonl_as_converted_and_never_reads_it_again() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let lines = legacy_line(1, "converted", now);
    fs::write(directory.path().join("wakes.jsonl"), &lines).unwrap();
    drop(DurableStore::acquire(&settings(&path), &legacy(directory.path())).unwrap());
    assert_eq!(
        fs::read_to_string(directory.path().join("wakes.jsonl.converted")).unwrap(),
        lines
    );
    assert!(!directory.path().join("wakes.jsonl").exists());
    fs::write(directory.path().join("wakes.jsonl"), "not json\n").unwrap();
    let acquired = DurableStore::acquire(&settings(&path), &legacy(directory.path())).unwrap();
    assert!(acquired.report.conversion.is_none());
    assert_eq!(acquired.store.restore(now).unwrap().wakes.len(), 1);
    assert_eq!(
        fs::read_to_string(directory.path().join("wakes.jsonl")).unwrap(),
        "not json\n"
    );
}

#[test]
fn conversion_streams_the_legacy_wake_file_line_by_line() {
    let directory = temporary();
    let fifo = directory.path().join("wakes.jsonl");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let (release, released) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let mut pipe = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        pipe.write_all(b"{\"id\":\n").unwrap();
        pipe.flush().unwrap();
        released.recv().unwrap();
    });
    let result = DurableStore::acquire(
        &settings(&directory.path().join("state.sqlite")),
        &legacy(directory.path()),
    );
    release.send(()).unwrap();
    writer.join().unwrap();
    assert!(matches!(
        result,
        Err(StoreError::LegacyWakes { line: 1, .. })
    ));
}

#[test]
fn thread_ownership_without_persisted_evidence_is_marked_lost_at_cutover() {
    let directory = temporary();
    let now = SystemTime::now();
    fs::write(
        directory.path().join("wakes.jsonl"),
        legacy_line(1, "kept", now),
    )
    .unwrap();
    let acquired = DurableStore::acquire(
        &settings(&directory.path().join("state.sqlite")),
        &legacy(directory.path()),
    )
    .unwrap();
    assert!(acquired.report.conversion.is_some());
    let rows = acquired.store.restore(now).unwrap();
    assert_eq!(rows.wakes.len(), 1);
    assert!(rows.ownership.is_empty());
}

#[test]
fn a_corrupt_legacy_wake_line_fails_boot_naming_the_line() {
    let directory = temporary();
    let now = SystemTime::now();
    let wakes = directory.path().join("wakes.jsonl");
    fs::write(&wakes, format!("{}not json\n", legacy_line(1, "kept", now))).unwrap();
    let result = DurableStore::acquire(
        &settings(&directory.path().join("state.sqlite")),
        &legacy(directory.path()),
    );
    assert!(matches!(
        result,
        Err(StoreError::LegacyWakes { path, line: 2 }) if path == wakes
    ));
    assert!(wakes.exists());
}

fn later_step(to: u32, sql: &'static str) -> MigrationStep {
    MigrationStep {
        to: SchemaVersion::new(to),
        sql,
    }
}

#[test]
fn a_failed_migration_keeps_the_backup_and_names_the_one_line_restore() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&path);
        let (store, _replay) = restored(&acquired, now);
        once(&store, anchor(), "before", now);
    }
    let steps = [
        later_step(1, STEPS[0].sql),
        later_step(2, "CREATE TABLE conversations (key TEXT);"),
    ];
    let result = acquire_with(&settings(&path), &LegacyInputs::default(), &steps);
    let Err(StoreError::MigrationFailed { from, backup, .. }) = result else {
        panic!("the migration must fail");
    };
    assert_eq!(from, SchemaVersion::CURRENT);
    assert_eq!(backup, directory.path().join("state.sqlite.schema-1.bak"));
    fs::rename(&backup, &path).unwrap();
    let acquired = acquire(&path);
    assert_eq!(acquired.store.restore(now).unwrap().wakes.len(), 1);
}

#[test]
fn a_newer_schema_or_an_unupgradable_record_fails_boot_listing_every_one() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    drop(acquire(&path));
    raw(&path).pragma_update(None, "user_version", 7).unwrap();
    assert!(matches!(
        DurableStore::acquire(&settings(&path), &LegacyInputs::default()),
        Err(StoreError::SchemaTooNew { found, known }) if found == SchemaVersion::new(7) && known == SchemaVersion::CURRENT
    ));
    raw(&path).pragma_update(None, "user_version", 1).unwrap();
    let connection = raw(&path);
    for key in ["dev:[\"first\"]", "dev:[\"second\"]"] {
        connection
            .execute(
                "INSERT INTO conversations (key, version, snapshot, updated_ms) VALUES (?1, 9, '{}', 0)",
                [key],
            )
            .unwrap();
    }
    drop(connection);
    let Err(StoreError::Unupgradable { records, backup }) =
        DurableStore::acquire(&settings(&path), &LegacyInputs::default())
    else {
        panic!("unupgradable records refuse boot");
    };
    assert_eq!(records.len(), 2);
    assert!(backup.exists());
}

#[test]
fn a_migration_writes_one_backup_per_old_schema_version() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    drop(acquire(&path));
    let second = [
        later_step(1, STEPS[0].sql),
        later_step(2, "CREATE TABLE second (id INTEGER) STRICT;"),
    ];
    let migrated = acquire_with(&settings(&path), &LegacyInputs::default(), &second).unwrap();
    assert_eq!(
        migrated
            .report
            .migration
            .as_ref()
            .map(|migration| migration.from),
        Some(SchemaVersion::CURRENT)
    );
    drop(migrated);
    let third = [
        later_step(1, STEPS[0].sql),
        later_step(2, "CREATE TABLE second (id INTEGER) STRICT;"),
        later_step(3, "CREATE TABLE third (id INTEGER) STRICT;"),
    ];
    drop(acquire_with(&settings(&path), &LegacyInputs::default(), &third).unwrap());
    drop(acquire_with(&settings(&path), &LegacyInputs::default(), &third).unwrap());
    let mut backups: Vec<String> = fs::read_dir(directory.path())
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".bak"))
        .collect();
    backups.sort();
    assert_eq!(
        backups,
        ["state.sqlite.schema-1.bak", "state.sqlite.schema-2.bak"]
    );
}

#[test]
fn every_record_is_migrated_forward_at_boot() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&path);
        let (store, _replay) = restored(&acquired, now);
        once(&store, anchor_in("d0123abc"), "first", now);
        once(&store, anchor_in("d0456def"), "second", now);
    }
    let steps = [
        later_step(1, STEPS[0].sql),
        later_step(2, "ALTER TABLE conversations ADD COLUMN note TEXT;"),
    ];
    let acquired = acquire_with(&settings(&path), &LegacyInputs::default(), &steps).unwrap();
    assert_eq!(acquired.report.schema, SchemaVersion::new(2));
    assert_eq!(acquired.store.restore(now).unwrap().wakes.len(), 2);
}

#[test]
fn an_unacknowledged_delivery_replays_and_a_duplicate_is_accepted() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let acquired = acquire(&path);
    let (store, _replay) = restored(&acquired, now);
    once(&store, anchor(), "twice", now);
    let first = due(&store, now + Duration::from_secs(2));
    assert_eq!(first.len(), 1);
    drop(first);
    let later = SystemTime::now() + Duration::from_secs(31);
    let second = due(&store, later);
    assert_eq!(second.len(), 1);
    drop(second);
    drop((store, acquired));
    let acquired = acquire(&path);
    let (store, replay) = restored(&acquired, now);
    assert_eq!(replay.wakes.len(), 1);
    for delivery in replay.wakes {
        delivery
            .ticket
            .settle(DeliveryDisposition::Handled)
            .unwrap();
    }
    assert!(store.next_at().is_none());
}

#[test]
fn an_acknowledgement_commits_in_the_same_transaction_as_its_wake_change() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let acquired = acquire(&path);
    let (store, _replay) = restored(&acquired, now);
    let id = digest(&store, now);
    let tick = store
        .take_due(now + Duration::from_secs(86_401))
        .unwrap()
        .ticks
        .pop()
        .expect("the digest ticks");
    let Resolved::Fired(delivery) = tick
        .resolve(
            &store,
            Verdict::Fire {
                output: "stale PR".into(),
            },
            now + Duration::from_secs(86_401),
        )
        .unwrap()
    else {
        panic!("a changed digest fires");
    };
    let key = digest_anchor().key();
    let pending = acquired.store.snapshot_for_test(&key).unwrap();
    assert!(pending.wakes[0].pending.is_some());
    delivery
        .ticket
        .settle(DeliveryDisposition::Handled)
        .unwrap();
    let settled = acquired.store.snapshot_for_test(&key).unwrap();
    assert_eq!(settled.wakes.len(), 1);
    assert_eq!(settled.wakes[0].id, id);
    assert!(settled.wakes[0].pending.is_none());
}

#[test]
fn a_recurring_wake_holds_at_most_one_pending_delivery() {
    let directory = temporary();
    let now = SystemTime::now();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let (store, _replay) = restored(&acquired, now);
    digest(&store, now);
    let first = now + Duration::from_secs(86_401);
    let tick = store.take_due(first).unwrap().ticks.pop().unwrap();
    let fired = tick
        .resolve(
            &store,
            Verdict::Fire {
                output: "one".into(),
            },
            first,
        )
        .unwrap();
    assert!(matches!(fired, Resolved::Fired(_)));
    let second = now + Duration::from_secs(2 * 86_401);
    assert!(store.take_due(second).unwrap().ticks.is_empty());
    let snapshot = acquired
        .store
        .snapshot_for_test(&digest_anchor().key())
        .unwrap();
    assert_eq!(
        snapshot
            .wakes
            .iter()
            .filter(|wake| wake.pending.is_some())
            .count(),
        1
    );
    drop(fired);
}

#[test]
fn an_interrupted_job_is_reported_with_its_deadline_within_one_boot() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let deadline_ms = crate::store::millis(now) + 60_000;
    {
        let acquired = acquire(&path);
        acquired
            .store
            .commit(vec![(
                anchor().key(),
                Change::JobStarted(job(&anchor(), 4, deadline_ms)),
            )])
            .unwrap();
    }
    let acquired = acquire(&path);
    let (store, replay) = restored(&acquired, now);
    assert_eq!(replay.jobs.len(), 1);
    assert_eq!(replay.jobs[0].row.id, JobId::new(4));
    assert_eq!(replay.jobs[0].row.deadline_ms, deadline_ms);
    assert_eq!(store.reserved_job_ids(), Some(JobId::new(4)));
}

#[test]
fn a_leave_drops_the_follow_and_the_thread_ownership_in_one_transaction() {
    let directory = temporary();
    let now = SystemTime::now();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let key = anchor().key();
    acquired
        .store
        .commit(vec![
            (
                key.clone(),
                Change::Follow(follow_row(&"a".repeat(64), now)),
            ),
            (key.clone(), Change::Claim(claim("1700000000.000001"))),
        ])
        .unwrap();
    acquired
        .store
        .commit(vec![(
            key.clone(),
            Change::Leave {
                follow: "a".repeat(64),
            },
        )])
        .unwrap();
    assert!(acquired.store.snapshot_for_test(&key).is_none());
}

#[test]
fn follows_and_thread_ownership_survive_a_restart() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let key = anchor().key();
    {
        let acquired = acquire(&path);
        let table = Arc::new(FollowTable::restored(
            Some(Arc::clone(&acquired.store)),
            Vec::new(),
            now,
        ));
        table
            .stay_and_write(&"b".repeat(64), FollowHome::Conversation(key.clone()), now)
            .unwrap();
        acquired
            .store
            .commit(vec![(
                key.clone(),
                Change::Claim(claim("1700000000.000002")),
            )])
            .unwrap();
    }
    let acquired = acquire(&path);
    let rows = acquired.store.restore(now).unwrap();
    assert_eq!(rows.ownership, [(key.clone(), claim("1700000000.000002"))]);
    let follows = rows
        .follows
        .into_iter()
        .map(|(key, row)| {
            (
                key.map_or(FollowHome::Retained, FollowHome::Conversation),
                row,
            )
        })
        .collect();
    let table = FollowTable::restored(Some(Arc::clone(&acquired.store)), follows, now);
    assert!(matches!(
        table.ticket(&"b".repeat(64), now),
        FollowTicket::Active { .. }
    ));
}

#[tokio::test]
async fn a_denied_wake_store_write_yields_one_cause_bearing_exit_and_a_nonzero_status() {
    let directory = temporary();
    let now = SystemTime::now();
    let mut acquired = acquire(&directory.path().join("state.sqlite"));
    let (store, _replay) = restored(&acquired, now);
    acquired.store.deny_writes_for_test(true);
    assert_eq!(
        store.register(
            anchor(),
            "denied".into(),
            now,
            Schedule::Once {
                after: Duration::from_secs(1)
            }
        ),
        Err(dekopon_agent::wake::WakeRefusal::Store)
    );
    let cause = acquired.fatal.failed().await;
    assert!(matches!(cause, StoreError::Sqlite { .. }));
    let outcome = crate::ServeOutcome::StoreFailed(cause);
    assert!(!outcome.is_clean());
}

#[test]
fn a_store_that_is_writable_again_after_the_restart_recovers() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&path);
        let (store, _replay) = restored(&acquired, now);
        once(&store, anchor(), "kept", now);
        acquired.store.deny_writes_for_test(true);
        assert!(store.take_due(now + Duration::from_secs(2)).is_err());
    }
    raw(&path)
        .execute_batch(
            "DROP TRIGGER deny_insert; DROP TRIGGER deny_update; DROP TRIGGER deny_delete;",
        )
        .unwrap();
    let acquired = acquire(&path);
    let (store, _replay) = restored(&acquired, now);
    assert_eq!(due(&store, now + Duration::from_secs(2)).len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fatal_exit_joins_every_owned_task_within_the_shutdown_grace() {
    let directory = temporary();
    let now = SystemTime::now();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let (store, replay) = restored(&acquired, now);
    once(&store, anchor(), "orphaned", now - Duration::from_secs(2));
    let driver = Arc::new(RecordingDriver::default());
    let drivers: BTreeMap<String, Arc<dyn ChatDriver>> =
        BTreeMap::from([("dev".to_owned(), Arc::clone(&driver) as Arc<dyn ChatDriver>)]);
    let mut runner = Arc::into_inner(runner(
        ResolvedBroker {
            socket_path: directory.path().join("absent.sock"),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        ModelScript::forbidden(),
        4,
    ))
    .unwrap();
    runner.wakes = Some(Arc::clone(&store));
    let Acquired {
        store: durable,
        fatal,
        ..
    } = acquired;
    let (_sender, receiver) = mpsc::channel(4);
    let serving = tokio::spawn(
        crate::gateway::Gateway::new(
            Arc::new(runner),
            Arc::new(crate::routes::RoutingTable::default()),
            Arc::default(),
            Arc::new(drivers),
            Arc::default(),
        )
        .with_durable(Some(Arc::clone(&durable)), Some(fatal), Some(replay))
        .serve(
            receiver,
            std::future::pending(),
            Duration::from_secs(2),
            crate::collection::Collector::new(&[], 4),
        ),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while driver.replies().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the orphan notice is sent");
    durable.deny_writes_for_test(true);
    assert!(
        durable
            .commit(vec![(anchor().key(), Change::JobEnded(JobId::new(9)))])
            .is_err()
    );
    let outcome = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("serving ends within the grace")
        .expect("serving joins");
    assert!(matches!(outcome, crate::ServeOutcome::StoreFailed(_)));
    assert_eq!(driver.replies(), ["orphaned"]);
}

#[test]
fn a_restart_after_a_fatal_exit_resumes_from_the_store() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&path);
        let (store, _replay) = restored(&acquired, now);
        once(&store, anchor(), "survives", now);
        acquired.store.deny_writes_for_test(true);
        assert_eq!(
            acquired
                .store
                .commit(vec![(anchor().key(), Change::JobEnded(JobId::new(1)))]),
            Err(StoreFailed)
        );
        acquired.store.deny_writes_for_test(false);
    }
    let acquired = acquire(&path);
    let rows = acquired.store.restore(now).unwrap();
    assert_eq!(rows.wakes.len(), 1);
    assert_eq!(rows.wakes[0].1.note, "survives");
}

#[tokio::test]
async fn the_store_is_acquired_once_at_initial_boot_and_never_by_check_or_reload() {
    let directory = temporary();
    let path = directory.path().join("state").join("state.sqlite");
    let mut input = document(directory.path());
    input["sessions"] = json!({
        "wakes": {},
        "durable": {"path": path, "sync": "full"},
    });
    let catalog_path = directory.path().join("dekopon.yaml");
    fs::write(&catalog_path, catalog_text(true, Some("reasoning"))).unwrap();
    fs::set_permissions(&catalog_path, fs::Permissions::from_mode(0o600)).unwrap();
    let _owner = acquire(&path);
    let report = crate::check(write_config(directory.path(), &input), None, None).await;
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    let config = resolved(directory.path(), &input).await;
    assert_eq!(
        config.durable.as_ref().map(|durable| durable.sync),
        Some(SyncMode::Full)
    );
}

#[test]
#[ignore = "the child process of a_second_gateway_process_on_the_same_store_fails_at_once"]
fn durable_store_holder_child() {
    let Some(path) = std::env::var_os(HOLDER) else {
        return;
    };
    let _owner = match DurableStore::acquire(&settings(Path::new(&path)), &LegacyInputs::default())
    {
        Ok(owner) => owner,
        Err(StoreError::Owned { .. }) => {
            println!("store-owned");
            return;
        }
        Err(error) => panic!("the holder could not open the store: {error}"),
    };
    println!("store-held");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    drop(std::io::stdin().read_line(&mut line));
}

#[test]
fn a_second_gateway_process_on_the_same_store_fails_at_once() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "tests::durable_store::durable_store_holder_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER, &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        lines
            .by_ref()
            .map_while(Result::ok)
            .any(|line| line.ends_with("store-held")),
        "the first process holds the store"
    );
    assert!(matches!(
        DurableStore::acquire(&settings(&path), &LegacyInputs::default()),
        Err(StoreError::Owned { .. })
    ));
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    drop(acquire(&path));
}

#[test]
fn conversion_reads_legacy_inputs_through_a_projected_symlink() {
    let directory = temporary();
    let now = SystemTime::now();
    let data = directory.path().join("v1");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("wakes.jsonl"), legacy_line(1, "projected", now)).unwrap();
    symlink("v1", directory.path().join("..data")).unwrap();
    symlink("..data/wakes.jsonl", directory.path().join("wakes.jsonl")).unwrap();
    let acquired = DurableStore::acquire(
        &settings(&directory.path().join("state.sqlite")),
        &LegacyInputs {
            wakes: Some(directory.path().join("wakes.jsonl")),
            journal: None,
        },
    )
    .unwrap();
    let rows = acquired.store.restore(now).unwrap();
    assert_eq!(rows.wakes.len(), 1);
    assert_eq!(rows.wakes[0].1.note, "projected");
}

#[test]
fn legacy_follow_markers_are_retained_and_counted_never_keyed_by_guess() {
    let directory = temporary();
    let now = SystemTime::now();
    let journal = directory.path().join("journal");
    fs::create_dir_all(&journal).unwrap();
    for key in ["c".repeat(64), "d".repeat(64)] {
        fs::write(
            journal.join(format!("{key}.followed")),
            serde_json::to_vec(&follow_row(&key, now).marker).unwrap(),
        )
        .unwrap();
    }
    let acquired = DurableStore::acquire(
        &settings(&directory.path().join("state.sqlite")),
        &legacy(directory.path()),
    )
    .unwrap();
    let conversion = acquired.report.conversion.as_ref().expect("converted");
    assert!(matches!(
        conversion.follows,
        FollowConversion::Retained { markers: 2 }
    ));
    let rows = acquired.store.restore(now).unwrap();
    assert_eq!(rows.follows.len(), 2);
    assert!(rows.follows.iter().all(|(key, _)| key.is_none()));
    assert!(acquired.store.records_for_test().is_empty());
    assert!(
        !journal
            .join(format!("{}.followed", "c".repeat(64)))
            .exists()
    );
}

#[tokio::test]
async fn a_durable_off_gateway_keeps_waits_in_memory_and_warns_they_are_lost_on_restart() {
    let directory = temporary();
    let mut input = document(directory.path());
    input["sessions"] = json!({"wakes": {}});
    input["routes"][0]["wakes"] = json!(true);
    let config = resolved(directory.path(), &input).await;
    let routes = RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap();
    let (capture, _guard) = capture_spans();
    let (durable, fatal, restored) = crate::acquire_store(&config, &routes).await.unwrap();
    assert!(durable.is_none() && fatal.is_none());
    assert!(restored.wakes.is_empty());
    assert_eq!(
        capture
            .events_where(
                "dekopon_gatewayd",
                &[
                    ("event", "gateway_store_memory"),
                    ("warning", "waits-lost-on-restart")
                ]
            )
            .len(),
        1
    );
    let store = WakeStore::in_memory(bounds());
    once(&store, anchor(), "memory", SystemTime::now());
    assert_eq!(store.list(anchor().subject(), SystemTime::now()).len(), 1);
    assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".sqlite")
    }));
}

#[test]
fn first_served_is_recorded_once_after_a_migration() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    drop(acquire(&path));
    let steps = [
        later_step(1, STEPS[0].sql),
        later_step(2, "CREATE TABLE second (id INTEGER) STRICT;"),
    ];
    let acquired = acquire_with(&settings(&path), &LegacyInputs::default(), &steps).unwrap();
    let first = SystemTime::now();
    acquired.store.first_served(first).unwrap();
    acquired
        .store
        .first_served(first + Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        acquired.store.meta_for_test("first-served"),
        [crate::store::millis(first).to_string()]
    );
    drop(acquired);
    let unmigrated = acquire_with(&settings(&path), &LegacyInputs::default(), &steps).unwrap();
    unmigrated.store.first_served(SystemTime::now()).unwrap();
    assert_eq!(unmigrated.store.meta_for_test("first-served").len(), 1);
}

#[tokio::test]
async fn every_serving_write_failure_raises_one_first_cause_fatal() {
    let directory = temporary();
    let mut acquired = acquire(&directory.path().join("state.sqlite"));
    let key = anchor().key();
    acquired.store.deny_writes_for_test(true);
    for id in 1..=3 {
        assert_eq!(
            acquired.store.commit(vec![(
                key.clone(),
                Change::JobStarted(job(&anchor(), id, 1))
            )]),
            Err(StoreFailed)
        );
    }
    acquired.store.deny_writes_for_test(false);
    assert_eq!(
        acquired
            .store
            .commit(vec![(key, Change::JobStarted(job(&anchor(), 4, 1)))]),
        Err(StoreFailed)
    );
    assert!(matches!(
        acquired.fatal.failed().await,
        StoreError::Sqlite { .. }
    ));
}

#[test]
fn a_queued_wake_turn_settles_its_ticket_only_when_the_turn_completes() {
    let directory = temporary();
    let now = SystemTime::now();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let (store, _replay) = restored(&acquired, now);
    let id = once(&store, anchor(), "queued", now);
    let delivery = due(&store, now + Duration::from_secs(2)).pop().unwrap();
    let mut running = crate::collection::Dispositions::new(Vec::new());
    let mut queued =
        crate::collection::Dispositions::new(Vec::new()).with_delivery(Some(delivery.ticket));
    running.absorb(&mut queued);
    drop(queued);
    let pending = acquired.store.snapshot_for_test(&anchor().key()).unwrap();
    assert_eq!(pending.wakes[0].id, id);
    assert!(pending.wakes[0].pending.is_some());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(running.settle(DeliveryDisposition::Handled));
    assert!(acquired.store.snapshot_for_test(&anchor().key()).is_none());
}

#[test]
fn an_orphaned_or_rejected_wake_delivery_settles_by_its_disposition() {
    let directory = temporary();
    let now = SystemTime::now();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let (store, _replay) = restored(&acquired, now);
    once(&store, anchor_in("d0123abc"), "rejected", now);
    once(&store, anchor_in("d0456def"), "orphaned", now);
    let mut fired = due(&store, now + Duration::from_secs(2));
    fired.sort_by_key(|delivery| delivery.fired.id());
    let mut fired = fired.into_iter();
    let rejected = fired.next().unwrap();
    let orphaned = fired.next().unwrap();
    rejected
        .ticket
        .settle(DeliveryDisposition::Rejected)
        .unwrap();
    orphaned
        .ticket
        .settle(DeliveryDisposition::Orphaned)
        .unwrap();
    assert!(
        acquired
            .store
            .snapshot_for_test(&anchor_in("d0456def").key())
            .is_none()
    );
    assert!(
        acquired
            .store
            .snapshot_for_test(&anchor_in("d0123abc").key())
            .is_some_and(|snapshot| snapshot.wakes[0].pending.is_some())
    );
    assert!(due(&store, SystemTime::now()).is_empty());
    let mut offered = due(&store, SystemTime::now() + Duration::from_secs(31));
    assert_eq!(offered.len(), 1);
    let offered = offered.pop().unwrap();
    assert!(offered.fired.into_inbound().text.contains("rejected"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restored_delivery_is_admitted_when_serving_starts() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&path);
        let (store, _replay) = restored(&acquired, now);
        once(&store, anchor(), "replayed", now);
        drop(due(&store, now + Duration::from_secs(2)));
    }
    let acquired = acquire(&path);
    let (store, replay) = restored(&acquired, now);
    assert_eq!(replay.wakes.len(), 1);
    let driver = Arc::new(RecordingDriver::default());
    let drivers: BTreeMap<String, Arc<dyn ChatDriver>> =
        BTreeMap::from([("dev".to_owned(), Arc::clone(&driver) as Arc<dyn ChatDriver>)]);
    let mut runner = Arc::into_inner(runner(
        ResolvedBroker {
            socket_path: directory.path().join("absent.sock"),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        ModelScript::forbidden(),
        4,
    ))
    .unwrap();
    runner.wakes = Some(Arc::clone(&store));
    let (_sender, receiver) = mpsc::channel(4);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(
        crate::gateway::Gateway::new(
            Arc::new(runner),
            Arc::new(crate::routes::RoutingTable::default()),
            Arc::default(),
            Arc::new(drivers),
            Arc::default(),
        )
        .with_durable(Some(Arc::clone(&acquired.store)), None, Some(replay))
        .serve(
            receiver,
            async move {
                drop(shutdown_rx.await);
            },
            Duration::from_secs(1),
            crate::collection::Collector::new(&[], 4),
        ),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while acquired.store.snapshot_for_test(&anchor().key()).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the replayed delivery settles");
    shutdown_tx.send(()).unwrap();
    serving.await.unwrap();
    assert_eq!(driver.replies(), ["replayed"]);
}

#[test]
fn a_subscription_synchronize_while_pending_keeps_the_pending_delivery() {
    let directory = temporary();
    let now = SystemTime::now();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let (store, _replay) = restored(&acquired, now);
    let anchor = subscriptions::subscription_anchor();
    let target = crate::pull_request_links::PullRequestLinkContext::parse(
        "https://github.com/acme/widget/pull/17",
    )
    .unwrap()
    .targets
    .remove(0);
    store.subscribe(anchor.clone(), target, now).unwrap();
    let event = subscriptions::synchronize_event("acme/widget", 17);
    let first = store
        .deliver_subscriptions(anchor.agent(), &event, now)
        .unwrap();
    assert_eq!(first.fired.len(), 1);
    let second = store
        .deliver_subscriptions(anchor.agent(), &event, now)
        .unwrap();
    assert!(second.matched);
    assert!(second.fired.is_empty());
    let snapshot = acquired.store.snapshot_for_test(&anchor.key()).unwrap();
    assert!(snapshot.wakes[0].pending.is_some());
    drop(first);
}

#[test]
fn an_interrupted_job_notice_replays_after_a_crash_before_its_acknowledgement() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    {
        let acquired = acquire(&path);
        acquired
            .store
            .commit(vec![(
                anchor().key(),
                Change::JobStarted(job(&anchor(), 2, crate::store::millis(now))),
            )])
            .unwrap();
    }
    {
        let acquired = acquire(&path);
        let (_store, replay) = restored(&acquired, now);
        assert_eq!(replay.jobs.len(), 1);
        drop(replay);
    }
    {
        let acquired = acquire(&path);
        let (_store, replay) = restored(&acquired, now);
        assert_eq!(replay.jobs.len(), 1);
        for notice in replay.jobs {
            notice.ticket.settle(DeliveryDisposition::Handled).unwrap();
        }
    }
    let acquired = acquire(&path);
    let (_store, replay) = restored(&acquired, now);
    assert!(replay.jobs.is_empty());
}

#[test]
fn a_tick_result_keeps_its_wake_id_on_every_outcome() {
    let directory = temporary();
    let now = SystemTime::now();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let (store, _replay) = restored(&acquired, now);
    let broker = ResolvedBroker {
        socket_path: directory.path().join("absent.sock"),
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let id = store
        .register(
            anchor(),
            "watch".into(),
            now,
            Schedule::Watch {
                probe: Probe::scripted_for_test("echo pass"),
                every: Duration::from_secs(60),
                until: Duration::from_secs(600),
            },
        )
        .unwrap()
        .id;
    let tick = store
        .take_due(now + Duration::from_secs(61))
        .unwrap()
        .ticks
        .pop()
        .unwrap();
    let ended = crate::wake::run_tick(tick, &store, &broker, runtime.handle(), None, None);
    assert_eq!(ended.wake, id);
    assert!(matches!(ended.result, crate::wake::TickResult::Fired(_)));
    store.cancel(anchor().subject(), id).unwrap();
    let gone = crate::wake::run_tick(
        crate::wake::Tick::for_test(id, anchor()),
        &store,
        &broker,
        runtime.handle(),
        None,
        None,
    );
    assert_eq!(gone.wake, id);
    assert!(matches!(gone.result, crate::wake::TickResult::Gone));
}

fn holder(path: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "tests::durable_store::durable_store_holder_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER, path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

type ChildLines = std::io::Lines<BufReader<std::process::ChildStdout>>;

fn outcome(child: &mut std::process::Child) -> (String, ChildLines) {
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let found = lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|line| {
            ["store-held", "store-owned"]
                .into_iter()
                .find(|outcome| line.ends_with(outcome))
                .map(str::to_owned)
        })
        .expect("the holder reports its outcome");
    (found, lines)
}

#[test]
fn two_gateway_processes_started_at_once_leave_exactly_one_owner() {
    for _ in 0..10 {
        let directory = temporary();
        let path = directory.path().join("state.sqlite");
        let mut children = [holder(&path), holder(&path)];
        let mut reports: Vec<(String, ChildLines)> = children.iter_mut().map(outcome).collect();
        let mut outcomes: Vec<&str> = reports.iter().map(|(found, _)| found.as_str()).collect();
        outcomes.sort_unstable();
        assert_eq!(outcomes, ["store-held", "store-owned"]);
        for (child, (_, lines)) in children.iter_mut().zip(&mut reports) {
            drop(child.stdin.take());
            lines.by_ref().for_each(drop);
            assert!(child.wait().unwrap().success());
        }
    }
}

#[test]
fn a_revoke_differing_only_in_case_removes_the_persisted_claim() {
    let directory = temporary();
    let acquired = acquire(&directory.path().join("state.sqlite"));
    let key = anchor().key();
    let claimed = ThreadClaim::Slack {
        team_id: "T0123ABC".to_owned(),
        channel_id: "C0123ABC".to_owned(),
        thread_ts: "1700000000.000001".to_owned(),
        user_id: "U0123ABC".to_owned(),
    };
    acquired
        .store
        .commit(vec![
            (key.clone(), Change::Claim(claimed)),
            (key.clone(), Change::Claim(claim("1700000000.000001"))),
        ])
        .unwrap();
    assert_eq!(
        acquired
            .store
            .snapshot_for_test(&key)
            .unwrap()
            .ownership
            .len(),
        1
    );
    acquired
        .store
        .commit(vec![(
            key.clone(),
            Change::Revoke(claim("1700000000.000001")),
        )])
        .unwrap();
    assert!(acquired.store.snapshot_for_test(&key).is_none());
}

#[tokio::test]
async fn restored_ownership_follows_the_recall_policy_of_its_conversations_route() {
    for (memory, kept) in [
        (json!({"mode": "oneShot"}), 0),
        (
            json!({"mode": "persistent", "recall": "journal", "summaries": "off"}),
            1,
        ),
    ] {
        let directory = temporary();
        let path = directory.path().join("state").join("state.sqlite");
        let key = ConversationKey::of(
            &"dev".parse().unwrap(),
            &dekopon_broker_protocol::Conversation {
                kind: ConversationKind::DirectMessage,
                container: None,
                id: "d0123abc".to_owned(),
                thread: None,
            },
        );
        {
            let acquired = acquire(&path);
            acquired
                .store
                .commit(vec![(
                    key.clone(),
                    Change::Claim(claim("1700000000.000003")),
                )])
                .unwrap();
        }
        let mut input = document(directory.path());
        input["transports"][0] = json!({"name": "dev", "kind": "slackSocketMode", "appTokenEnv": "APP", "botTokenEnv": "BOT"});
        input["routes"][0]["conversation"] = json!({"kind": ["thread"]});
        input["routes"][0]["memory"] = memory;
        input["sessions"] = json!({
            "durable": {"path": path},
            "journal": {"path": directory.path().join("journal")},
            "assets": {"path": directory.path().join("assets")},
        });
        let config = resolved(directory.path(), &input).await;
        let routes = RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap();
        let (_durable, _fatal, restored) = crate::acquire_store(&config, &routes).await.unwrap();
        assert_eq!(restored.ownership.len(), kept);
    }
}

fn slack_thread(kind: ConversationKind) -> dekopon_broker_protocol::Conversation {
    dekopon_broker_protocol::Conversation {
        kind,
        container: Some("t0123abc".to_owned()),
        id: "c0123abc".to_owned(),
        thread: Some("1700000000.000001".to_owned()),
    }
}

#[test]
fn a_channel_opening_and_its_thread_replies_share_one_conversation_key() {
    let transport = "workspace".parse().unwrap();
    assert_eq!(
        ConversationKey::of(&transport, &slack_thread(ConversationKind::Channel)),
        ConversationKey::of(&transport, &slack_thread(ConversationKind::Thread)),
    );
}

#[test]
fn a_claim_on_the_opening_revoked_from_a_reply_is_not_owned_after_a_restart() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let transport = "workspace".parse().unwrap();
    let opening = ConversationKey::of(&transport, &slack_thread(ConversationKind::Channel));
    let reply = ConversationKey::of(&transport, &slack_thread(ConversationKind::Thread));
    {
        let acquired = acquire(&path);
        acquired
            .store
            .commit(vec![(opening, Change::Claim(claim("1700000000.000001")))])
            .unwrap();
        assert_eq!(acquired.store.records_for_test().len(), 1);
        acquired
            .store
            .commit(vec![(reply, Change::Revoke(claim("1700000000.000001")))])
            .unwrap();
    }
    let acquired = acquire(&path);
    assert!(
        acquired
            .store
            .restore(SystemTime::now())
            .unwrap()
            .ownership
            .is_empty()
    );
    assert!(acquired.store.records_for_test().is_empty());
}

#[test]
fn a_second_ticketed_job_notice_queues_and_replays_until_its_own_turn_settles() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let route = jobs_e2e::job_route(Duration::from_secs(10), Duration::from_secs(10));
    let person = wakes::slack_message("person");
    let anchor = Anchor::for_job(&person, &journal_route()).unwrap();
    {
        let acquired = acquire(&path);
        acquired
            .store
            .commit(vec![
                (
                    anchor.key(),
                    Change::JobStarted(job(&anchor, 21, crate::store::millis(now))),
                ),
                (
                    anchor.key(),
                    Change::JobStarted(job(&anchor, 22, crate::store::millis(now))),
                ),
            ])
            .unwrap();
    }
    {
        let acquired = acquire(&path);
        let (_store, replay) = restored(&acquired, now);
        let gate = crate::session::SessionGate::new(1);
        let crate::session::Admit::Admitted(admission, _, _) = gate.admit(
            &route,
            person.clone(),
            crate::collection::Dispositions::new(Vec::new()),
        ) else {
            panic!("the running turn is admitted");
        };
        let mut notices = replay.jobs;
        notices.sort_by_key(|notice| notice.row.id.get());
        for notice in notices {
            let message = Anchor::restored(notice.row.anchor.clone())
                .unwrap()
                .job_inbound(notice.row.id, "interrupted".to_owned(), None);
            assert!(matches!(
                gate.admit(
                    &route,
                    message,
                    crate::collection::Dispositions::new(Vec::new())
                        .with_delivery(Some(notice.ticket)),
                ),
                crate::session::Admit::Queued
            ));
        }
        let (admission, mut first) = admission
            .next_or_release()
            .expect("the first notice follows");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(first.receipts.settle(DeliveryDisposition::Handled));
        drop((admission, gate));
    }
    let acquired = acquire(&path);
    let (_store, replay) = restored(&acquired, now);
    let replayed: Vec<u64> = replay
        .jobs
        .iter()
        .map(|notice| notice.row.id.get())
        .collect();
    assert_eq!(replayed, [22]);
}

#[test]
fn a_live_job_notice_without_a_ticket_still_steers_the_running_turn() {
    let gate = crate::session::SessionGate::new(1);
    let route = jobs_e2e::job_route(Duration::from_secs(10), Duration::from_secs(10));
    let person = message("person");
    let crate::session::Admit::Admitted(_admission, _, _) = gate.admit(
        &route,
        person.clone(),
        crate::collection::Dispositions::new(Vec::new()),
    ) else {
        panic!("the running turn is admitted");
    };
    let notice = Anchor::for_job(&person, &journal_route())
        .unwrap()
        .job_inbound(JobId::new(5), "finished".to_owned(), None);
    assert!(matches!(
        gate.admit(
            &route,
            notice,
            crate::collection::Dispositions::new(Vec::new())
        ),
        crate::session::Admit::Steered(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_job_whose_anchor_cannot_be_restored_settles_as_orphaned() {
    let directory = temporary();
    let path = directory.path().join("state.sqlite");
    let now = SystemTime::now();
    let mut local = job(&anchor(), 31, crate::store::millis(now));
    local.anchor.reply = ReplyTarget::Local { connection: 0 };
    let key = anchor().key();
    {
        let acquired = acquire(&path);
        acquired
            .store
            .commit(vec![(key.clone(), Change::JobStarted(local))])
            .unwrap();
    }
    let acquired = acquire(&path);
    let (store, replay) = restored(&acquired, now);
    assert_eq!(replay.jobs.len(), 1);
    let mut runner = Arc::into_inner(runner(
        ResolvedBroker {
            socket_path: directory.path().join("absent.sock"),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        ModelScript::forbidden(),
        4,
    ))
    .unwrap();
    runner.wakes = Some(Arc::clone(&store));
    let (_sender, receiver) = mpsc::channel(4);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(
        crate::gateway::Gateway::new(
            Arc::new(runner),
            Arc::new(crate::routes::RoutingTable::default()),
            Arc::default(),
            Arc::default(),
            Arc::default(),
        )
        .with_durable(Some(Arc::clone(&acquired.store)), None, Some(replay))
        .serve(
            receiver,
            async move {
                drop(shutdown_rx.await);
            },
            Duration::from_secs(1),
            crate::collection::Collector::new(&[], 4),
        ),
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while acquired.store.snapshot_for_test(&key).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the unrestorable notice settles as orphaned");
    shutdown_tx.send(()).unwrap();
    serving.await.unwrap();
    assert!(store.next_at().is_none());
}

#[tokio::test]
async fn restored_follows_keep_only_rows_a_journal_recall_follow_route_keyed() {
    for (follow, kept) in [(false, 0), (true, 1)] {
        let directory = temporary();
        let path = directory.path().join("state").join("state.sqlite");
        let mut input = document(directory.path());
        input["transports"][0] = json!({"name": "discord", "kind": "discordGateway", "botTokenEnv": "DEKOPON_GATEWAYD_DISCORD_BOT_TOKEN", "messageContent": true});
        input["routes"][0]["transport"] = json!("discord");
        input["routes"][0]["conversation"] = json!({"kind": ["channel"]});
        input["routes"][0]["memory"] =
            json!({"mode": "persistent", "recall": "journal", "summaries": "off"});
        input["routes"][0]["follow"] = json!(true);
        input["sessions"] = json!({
            "durable": {"path": path},
            "journal": {"path": directory.path().join("journal")},
            "assets": {"path": directory.path().join("assets")},
        });
        let config = resolved(directory.path(), &input).await;
        let bound = RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap();
        let route = &bound.routes()[0];
        let conversation = dekopon_broker_protocol::Conversation {
            kind: ConversationKind::Channel,
            container: None,
            id: "c0123abc".to_owned(),
            thread: None,
        };
        let row_key = crate::follow::follow_key(
            &route.transport,
            route.agent.as_str(),
            &route.conversation,
            &conversation.key(),
        );
        let key = ConversationKey::of(&"discord".parse().unwrap(), &conversation);
        {
            let acquired = acquire(&path);
            acquired
                .store
                .commit(vec![(
                    key,
                    Change::Follow(follow_row(&row_key, SystemTime::now())),
                )])
                .unwrap();
        }
        if !follow {
            input["routes"][0]["follow"] = json!(false);
        }
        let config = resolved(directory.path(), &input).await;
        let routes = RoutingTable::bind(&config, &catalog(true, Some("reasoning"))).unwrap();
        let (_durable, _fatal, restored) = crate::acquire_store(&config, &routes).await.unwrap();
        assert_eq!(restored.follows.len(), kept);
    }
}
