use dekopon_capability::{StorageAccess, StorageInterface, StorageScope};
use dekopon_core::history::HistoryKey;
use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits};
use std::fs::File;

use super::*;
use crate::memory::{
    journal::{Entry, Journal, Keep},
    record::GrantDigest,
    retention::{Growth, HistoryError, HistoryRetention},
};

const OLDER: &str = "assistant/1111111111111111111111111111111111111111111111111111111111111111";
const NEWER: &str = "assistant/2222222222222222222222222222222222222222222222222222222222222222";
const BUDGET: u64 = 20_000_000_000;
const IDLE: Duration = Duration::from_secs(90 * 24 * 60 * 60);

struct Stores {
    directory: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    broker_task: Option<tokio::task::JoinHandle<()>>,
    broker: Option<ResolvedBroker>,
}

impl Stores {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(directory.path().join("storage")).unwrap();
        fs::set_permissions(
            directory.path().join("storage"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Self {
            directory,
            stop: None,
            broker_task: None,
            broker: None,
        }
    }

    fn storage(&self) -> PathBuf {
        self.directory
            .path()
            .canonicalize()
            .unwrap()
            .join("storage")
    }

    fn journal_dir(&self) -> PathBuf {
        self.directory.path().join("journal")
    }

    fn journal(&self) -> Arc<Journal> {
        let (journal, _boot) = Journal::boot(
            &self.journal_dir(),
            IDLE,
            IDLE,
            IDLE,
            std::num::NonZeroUsize::new(64).unwrap(),
            SystemTime::now(),
        )
        .unwrap();
        Arc::new(journal)
    }

    fn broker_namespace(&self, conversation: &str) -> PathBuf {
        let host = StorageHost::open(self.storage(), StorageLimits::default()).unwrap();
        let token = host
            .prepare_grant(chat_request(conversation, None))
            .unwrap()
            .namespace()
            .to_owned();
        self.storage().join("namespaces").join(token)
    }

    fn record(&self, journal: &Journal, stem: &str, conversation: &str, at: SystemTime) {
        let host = StorageHost::open(self.storage(), StorageLimits::default()).unwrap();
        let grant = host
            .grant(chat_request(
                conversation,
                Some(HistoryKey::new(stem.to_owned()).unwrap()),
            ))
            .unwrap();
        let mut handle = host.begin(grant).unwrap();
        handle
            .jsonl_append("turns.jsonl", 0, br#"{"turn":"synthetic"}"#)
            .unwrap();
        handle.commit().unwrap();
        let turn = captured("synthetic question", Some("synthetic answer"));
        journal
            .append(
                stem,
                &Entry {
                    at,
                    grant: &GrantDigest::of(&["probe.read".to_owned()]),
                    captured: &turn,
                    keep: Keep::Everything,
                    plan: None,
                    attachments: Default::default(),
                    arrivals: &[],
                    inventory: &[],
                    next_asset_id: 1,
                },
            )
            .unwrap()
            .expect("within the agent cap");
    }

    fn transcript(&self, journal: &Journal, stem: &str, at: SystemTime) {
        let turn = captured("synthetic question", Some("synthetic answer"));
        journal
            .append(
                stem,
                &Entry {
                    at,
                    grant: &GrantDigest::of(&["probe.read".to_owned()]),
                    captured: &turn,
                    keep: Keep::Everything,
                    plan: None,
                    attachments: Default::default(),
                    arrivals: &[],
                    inventory: &[],
                    next_asset_id: 1,
                },
            )
            .unwrap()
            .expect("within the agent cap");
    }

    async fn start_broker(&mut self) -> ResolvedBroker {
        self.start_broker_with(StorageLimits::default()).await
    }

    async fn start_broker_with(&mut self, limits: StorageLimits) -> ResolvedBroker {
        let socket = self.directory.path().join("broker.sock");
        let config_path = self.directory.path().join("broker.json");
        let policy_path = self.directory.path().join("policies.cedar");
        fs::write(
            &policy_path,
            r#"@id("prompt") permit(principal, action == Dekopon::Action::"agent.prompt", resource);"#,
        )
        .unwrap();
        let mut storage = serde_json::to_value(limits).unwrap();
        storage["rootPath"] = json!(self.storage());
        fs::write(
            &config_path,
            serde_json::to_vec(&json!({
                "apiVersion": dekopon_brokerd::CONFIG_API_VERSION,
                "socketPath": socket, "policiesPath": policy_path,
                "providers": [dekopon_test_support::provider_fixture("cli-probe-provider.wasm")],
                "capabilities": {"cli-probe": {"constraints": {"timeoutMs": 30000},
                    "capabilities": {"cli-probe.upper": {}}}},
                "storage": storage,
                "identities": [{"uid": crate::current_uid(), "principal": "dekopon-gatewayd",
                    "actor": {"type":"service", "principal":"dekopon-gatewayd"},
                    "attestor": {"namespaces":["drn:slack:*:*:*"]}}],
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            dekopon_brokerd::run(
                config_path,
                async move {
                    let _stopped = stopped.await;
                },
                dekopon_brokerd::ShutdownDeadline::default(),
            )
            .await
            .expect("broker runs");
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !socket.exists() {
                assert!(!task.is_finished(), "broker starts");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("broker socket ready");
        let broker = ResolvedBroker {
            socket_path: socket,
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        };
        self.stop = Some(stop);
        self.broker_task = Some(task);
        self.broker = Some(broker.clone());
        broker
    }

    async fn stop_broker(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _sent = stop.send(());
        }
        if let Some(task) = self.broker_task.take() {
            task.await.unwrap();
        }
        if let Err(error) = fs::remove_file(self.directory.path().join("broker.sock")) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
    }

    fn transcript_path(&self, stem: &str) -> PathBuf {
        self.journal_dir().join(format!("{stem}.jsonl"))
    }

    fn activity(&self, stem: &str) -> PathBuf {
        self.journal_dir().join(format!("{stem}.activity"))
    }

    fn intent(&self, stem: &str) -> PathBuf {
        self.journal_dir().join(format!("{stem}.reclaim"))
    }

    fn assert_present(&self, stem: &str, conversation: &str) {
        assert!(self.transcript_path(stem).exists(), "{stem} transcript");
        assert!(self.activity(stem).exists(), "{stem} clock");
        assert!(self.namespace_exists(conversation), "{stem} broker copy");
    }

    fn assert_gone(&self, stem: &str, conversation: &str) {
        assert!(!self.transcript_path(stem).exists(), "{stem} transcript");
        assert!(!self.activity(stem).exists(), "{stem} clock");
        assert!(!self.intent(stem).exists(), "{stem} intent");
        assert!(!self.namespace_exists(conversation), "{stem} broker copy");
    }

    fn namespace_exists(&self, conversation: &str) -> bool {
        let token = token(conversation);
        self.storage().join("namespaces").join(token).exists()
    }
}

fn token(conversation: &str) -> String {
    let directory = tempfile::tempdir().unwrap();
    let host = StorageHost::open(
        directory.path().canonicalize().unwrap().join("probe"),
        StorageLimits::default(),
    )
    .unwrap();
    host.prepare_grant(chat_request(conversation, None))
        .unwrap()
        .namespace()
        .to_owned()
}

fn chat_request(conversation: &str, history: Option<HistoryKey>) -> StorageGrantRequest {
    StorageGrantRequest::new(
        "history-invocation".parse().unwrap(),
        "memory-chat.record".parse().unwrap(),
        "memory-chat".parse().unwrap(),
        StorageInterface::Jsonl,
        StorageAccess::ReadWrite,
        StorageScope::PrivateConversation,
        "assistant".parse().unwrap(),
        "drn:slack:user:t0123abc:u0123abc".parse().unwrap(),
        "slack",
        "example-slack",
        "c0123abc",
        conversation,
        ContinuityPolicy::Relaxed,
        b"surface".to_vec(),
    )
    .with_history(history)
}

async fn two_chats(stores: &mut Stores) -> (Arc<Journal>, ResolvedBroker) {
    let journal = stores.journal();
    let now = SystemTime::now();
    stores.record(&journal, OLDER, "c-older", now - Duration::from_secs(60));
    stores.record(&journal, NEWER, "c-newer", now);
    let broker = stores.start_broker().await;
    (journal, broker)
}

async fn used(journal: &Arc<Journal>, broker: &ResolvedBroker) -> u64 {
    let usage = broker
        .leg_client()
        .unwrap()
        .history_usage(Vec::new(), Vec::new())
        .await
        .unwrap();
    journal.history_bytes().unwrap() + usage.chat_used
}

#[tokio::test]
async fn two_chats_in_both_stores_are_reclaimed_one_at_a_time() {
    let mut stores = Stores::new();
    let (journal, broker) = two_chats(&mut stores).await;
    let total = used(&journal, &broker).await;
    let budget = total + 1;
    let history = HistoryRetention::new(Arc::clone(&journal), broker.clone(), budget, IDLE);

    let admission = history.admit().await;
    admission
        .ensure("assistant/new", Growth::journal(2))
        .await
        .unwrap();
    drop(admission);
    stores.assert_gone(OLDER, "c-older");
    stores.assert_present(NEWER, "c-newer");

    let admission = history.admit().await;
    admission
        .ensure("assistant/new", Growth::journal(budget))
        .await
        .unwrap();
    drop(admission);
    stores.assert_gone(NEWER, "c-newer");
    assert_eq!(used(&journal, &broker).await, 0);
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_chat_in_use_is_skipped_and_the_next_one_reclaimed() {
    let mut stores = Stores::new();
    let (journal, broker) = two_chats(&mut stores).await;
    let total = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker.clone(), total + 1, IDLE);

    let in_use = history.begin_use(OLDER).await.unwrap();
    let admission = history.admit().await;
    admission
        .ensure("assistant/new", Growth::journal(2))
        .await
        .unwrap();
    drop(admission);
    stores.assert_present(OLDER, "c-older");
    stores.assert_gone(NEWER, "c-newer");

    let admission = history.admit().await;
    assert!(matches!(
        admission
            .ensure("assistant/new", Growth::journal(total + 1))
            .await,
        Err(HistoryError::Exhausted)
    ));
    drop(admission);
    stores.assert_present(OLDER, "c-older");
    drop(in_use);
    stores.stop_broker().await;
}

#[derive(Clone, Copy, Debug)]
enum Crash {
    AfterIntent,
    InsideBrokerDeletion,
    AfterBrokerDeletion,
    AfterTranscript,
    AfterClock,
}

#[tokio::test]
async fn a_reclaim_interrupted_at_any_step_finishes_at_the_next_boot() {
    for crash in [
        Crash::AfterIntent,
        Crash::InsideBrokerDeletion,
        Crash::AfterBrokerDeletion,
        Crash::AfterTranscript,
        Crash::AfterClock,
    ] {
        let mut stores = Stores::new();
        let journal = stores.journal();
        let now = SystemTime::now();
        stores.record(&journal, OLDER, "c-older", now - Duration::from_secs(60));
        stores.record(&journal, NEWER, "c-newer", now);
        journal.write_reclaim_intent(OLDER).unwrap();
        let namespace = stores.broker_namespace("c-older");
        match crash {
            Crash::AfterIntent => {}
            Crash::InsideBrokerDeletion => fs::rename(
                &namespace,
                namespace.with_file_name(format!(
                    "{}.reclaiming",
                    namespace.file_name().unwrap().to_str().unwrap()
                )),
            )
            .unwrap(),
            Crash::AfterBrokerDeletion | Crash::AfterTranscript | Crash::AfterClock => {
                fs::remove_dir_all(&namespace).unwrap();
            }
        }
        if matches!(crash, Crash::AfterTranscript | Crash::AfterClock) {
            fs::remove_file(stores.transcript_path(OLDER)).unwrap();
        }
        if matches!(crash, Crash::AfterClock) {
            fs::remove_file(stores.activity(OLDER)).unwrap();
        }
        drop(journal);

        let journal = stores.journal();
        let broker = stores.start_broker().await;
        let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);
        history.recover().await.unwrap();
        stores.assert_gone(OLDER, "c-older");
        stores.assert_present(NEWER, "c-newer");
        assert!(
            !namespace
                .with_file_name(format!(
                    "{}.reclaiming",
                    namespace.file_name().unwrap().to_str().unwrap()
                ))
                .exists(),
            "{crash:?}"
        );
        stores.stop_broker().await;
    }
}

#[tokio::test]
async fn a_chat_idle_past_the_window_expires_and_a_late_append_keeps_it() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    let start = SystemTime::now() - IDLE - Duration::from_secs(3600);
    stores.record(&journal, OLDER, "c-older", start);
    stores.record(&journal, NEWER, "c-newer", start);
    journal
        .note_activity(NEWER, start + IDLE - Duration::from_secs(60))
        .unwrap();
    let broker = stores.start_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);

    let read = history.begin_use(NEWER).await.unwrap();
    drop(read);
    assert_eq!(history.expire(start + IDLE).await.unwrap(), 0);
    assert_eq!(
        history
            .expire(start + IDLE + Duration::from_secs(1))
            .await
            .unwrap(),
        1
    );
    stores.assert_gone(OLDER, "c-older");
    stores.assert_present(NEWER, "c-newer");
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_transcript_without_a_clock_takes_its_newest_record_time_at_boot() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    let start = SystemTime::now() - IDLE - Duration::from_secs(3600);
    stores.record(&journal, OLDER, "c-older", start);
    fs::remove_file(stores.activity(OLDER)).unwrap();
    let broker = stores.start_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);

    history.recover().await.unwrap();
    assert!(stores.activity(OLDER).exists());
    assert_eq!(history.expire(SystemTime::now()).await.unwrap(), 1);
    stores.assert_gone(OLDER, "c-older");
    stores.stop_broker().await;
}

#[test]
fn a_measured_turn_bounds_the_bytes_its_append_writes() {
    let directory = tempfile::tempdir().unwrap();
    let (journal, _boot) = Journal::boot(
        directory.path(),
        IDLE,
        IDLE,
        IDLE,
        std::num::NonZeroUsize::new(64).unwrap(),
        SystemTime::now(),
    )
    .unwrap();
    let turn = captured("synthetic question", Some("synthetic answer"));
    let grant = GrantDigest::of(&["probe.read".to_owned()]);
    let entry = Entry {
        at: SystemTime::now(),
        grant: &grant,
        captured: &turn,
        keep: Keep::Everything,
        plan: None,
        attachments: Default::default(),
        arrivals: &[],
        inventory: &[],
        next_asset_id: 1,
    };
    let measured = crate::memory::journal::measure(&entry).unwrap();
    journal.append(OLDER, &entry).unwrap().unwrap();
    assert!(measured >= journal.history_bytes().unwrap());
}

fn stem(index: usize) -> String {
    format!("assistant/{index:064x}")
}

#[tokio::test]
async fn a_pass_reclaims_at_most_sixty_four_chats_then_checks_once_more() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    let start = SystemTime::now() - Duration::from_secs(3600);
    for index in 0..65 {
        stores.transcript(
            &journal,
            &stem(index),
            start + Duration::from_secs(index as u64),
        );
    }
    let broker = stores.start_broker().await;
    let budget = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, budget, IDLE);

    let admission = history.admit().await;
    assert!(matches!(
        admission
            .ensure("assistant/new", Growth::journal(budget))
            .await,
        Err(HistoryError::Exhausted)
    ));
    drop(admission);
    let left = (0..65)
        .filter(|index| stores.transcript_path(&stem(*index)).exists())
        .count();
    assert_eq!(left, 1, "one pass reclaims at most 64 chats");
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_journal_append_does_not_need_broker_root_space() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.transcript(&journal, OLDER, SystemTime::now());
    let probe = stores.start_broker().await;
    let usage = probe
        .leg_client()
        .unwrap()
        .history_usage(Vec::new(), Vec::new())
        .await
        .unwrap();
    stores.stop_broker().await;
    let used_root = StorageLimits::default().max_root_bytes - usage.root_free_bytes;
    let tight = StorageLimits {
        max_root_bytes: used_root + 1,
        max_namespace_bytes: used_root + 1,
        max_file_bytes: used_root + 1,
        ..StorageLimits::default()
    };
    let broker = stores.start_broker_with(tight).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);

    let admission = history.admit().await;
    admission
        .ensure("assistant/new", Growth::journal(4_096))
        .await
        .unwrap();
    drop(admission);
    assert!(
        stores.transcript_path(OLDER).exists(),
        "no chat was reclaimed"
    );
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_broker_recording_waits_for_a_published_clock() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.transcript(&journal, OLDER, SystemTime::now());
    fs::remove_file(stores.activity(OLDER)).unwrap();
    fs::create_dir(stores.activity(OLDER)).unwrap();
    let broker = stores.start_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);

    assert!(matches!(
        history.begin_record(OLDER, SystemTime::now()).await,
        Err(HistoryError::Clock)
    ));
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_recording_bound_larger_than_the_budget_is_refused() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    let broker = stores.start_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, 10_000, IDLE);

    assert!(matches!(
        history.begin_record(OLDER, SystemTime::now()).await,
        Err(HistoryError::TooLarge { .. })
    ));
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_chat_with_an_unfinished_reclaim_stays_fenced_until_it_completes() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.record(&journal, OLDER, "c-older", SystemTime::now());
    journal.write_reclaim_intent(OLDER).unwrap();
    let broker = stores.start_broker().await;
    stores.stop_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);

    assert!(matches!(
        history.begin_use(OLDER).await,
        Err(HistoryError::Pending)
    ));
    stores.transcript(&journal, NEWER, SystemTime::now());
    let turn = captured("late question", Some("late answer"));
    assert!(
        journal
            .append(
                OLDER,
                &Entry {
                    at: SystemTime::now(),
                    grant: &GrantDigest::of(&["probe.read".to_owned()]),
                    captured: &turn,
                    keep: Keep::Everything,
                    plan: None,
                    attachments: Default::default(),
                    arrivals: &[],
                    inventory: &[],
                    next_asset_id: 1,
                },
            )
            .is_err(),
        "no new history lands on a chat being reclaimed"
    );

    let broker = stores.start_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);
    drop(history.begin_use(OLDER).await.unwrap());
    stores.assert_gone(OLDER, "c-older");
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_replay_blocked_by_a_leased_member_fails_boot_and_keeps_its_intent() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.record(&journal, OLDER, "c-older", SystemTime::now());
    stores.record(&journal, OLDER, "c-older-second", SystemTime::now());
    journal.write_reclaim_intent(OLDER).unwrap();
    let lease = File::open(stores.broker_namespace("c-older-second").join("base.lock")).unwrap();
    lease.lock().unwrap();
    let broker = stores.start_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);

    assert!(matches!(
        history.recover().await,
        Err(HistoryError::ReplayBlocked)
    ));
    assert!(stores.intent(OLDER).exists());
    assert!(stores.transcript_path(OLDER).exists());
    drop(lease);
    history.recover().await.unwrap();
    stores.assert_gone(OLDER, "c-older");
    assert!(!stores.namespace_exists("c-older-second"));
    stores.stop_broker().await;
}

#[test]
fn a_summary_line_moves_the_chat_clock() {
    let stores = Stores::new();
    let journal = stores.journal();
    let start = SystemTime::now() - Duration::from_secs(3600);
    let turn = captured("synthetic question", Some("synthetic answer"));
    let plan = crate::memory::record::SummaryPlan {
        model: "summary".to_owned(),
        parts: vec![std::num::NonZeroU32::new(8).unwrap()],
        max_input_bytes: std::num::NonZeroU32::new(1024).unwrap(),
    };
    let header = journal
        .append(
            OLDER,
            &Entry {
                at: start,
                grant: &GrantDigest::of(&["probe.read".to_owned()]),
                captured: &turn,
                keep: Keep::Everything,
                plan: Some(plan),
                attachments: Default::default(),
                arrivals: &[],
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap();
    let before = journal.activity(OLDER).unwrap().unwrap();
    let written = journal
        .write_slot(
            OLDER,
            crate::memory::record::SlotLine {
                at_ms: 1,
                record: header.id,
                part: std::num::NonZeroU8::MIN,
                slot: crate::memory::record::Slot::Running,
                subject: crate::memory::record::Subject::Record,
            },
        )
        .unwrap();
    assert!(written);
    assert!(journal.activity(OLDER).unwrap().unwrap() > before);
}

#[tokio::test]
async fn an_aborted_persistence_keeps_the_chat_claimed_until_its_native_job_ends() {
    let mut stores = Stores::new();
    let journal_dir = stores.journal_dir();
    let broker = stores.start_broker().await;
    let runner =
        super::journaled_runner(broker.clone(), Arc::new(ModelScript::new([])), &journal_dir);
    let journal = Arc::clone(runner.journal.as_ref().unwrap());
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);
    let deferred = crate::memory::generation::Deferred {
        corrupt: Some(crate::memory::journal::CorruptStem(OLDER.to_owned())),
        history: Some(history.begin_use(OLDER).await.unwrap()),
        ..Default::default()
    };
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, unblock) = std::sync::mpsc::sync_channel::<()>(1);
    let held_journal = Arc::clone(&journal);
    let blocker = tokio::task::spawn_blocking(move || {
        let _held = held_journal.hold_io_for_test();
        locked.send(()).unwrap();
        unblock
            .recv_timeout(Duration::from_secs(10))
            .expect("journal lock released");
    });
    ready.await.unwrap();
    let drained = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { crate::memory::generation::drain(&runner, deferred).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    drained.abort();
    assert!(drained.await.unwrap_err().is_cancelled());
    assert!(history.claimed(OLDER), "the native job still owns the chat");
    release.send(()).unwrap();
    blocker.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while history.claimed(OLDER) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the claim ends with the native job");
    stores.stop_broker().await;
}

#[tokio::test]
async fn an_unreadable_chat_marker_is_its_own_unit_and_reclaimed_under_pressure() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.record(&journal, OLDER, "c-older", SystemTime::now());
    fs::remove_file(stores.transcript_path(OLDER)).unwrap();
    fs::remove_file(stores.activity(OLDER)).unwrap();
    fs::write(
        stores.broker_namespace("c-older").join("chat"),
        b"not a key",
    )
    .unwrap();
    let broker = stores.start_broker().await;
    let total = used(&journal, &broker).await;
    assert!(total > 0, "the unit's bytes stay counted");
    let history = HistoryRetention::new(Arc::clone(&journal), broker.clone(), total + 1, IDLE);

    let admission = history.admit().await;
    admission
        .ensure("assistant/new", Growth::journal(2))
        .await
        .unwrap();
    drop(admission);
    assert!(!stores.namespace_exists("c-older"));
    assert_eq!(used(&journal, &broker).await, 0);
    stores.stop_broker().await;
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "the socket is test-owned; a failed reply shows up as the test's own broker error"
)]
async fn usage_broker(
    directory: &Path,
    usage: Arc<parking_lot::Mutex<dekopon_broker_protocol::HistoryUsage>>,
) -> ResolvedBroker {
    let socket = directory.join("usage.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut stream = DescriptorStream::new(stream);
            let Ok((request, _inputs)) = stream
                .read_frame::<RequestEnvelope>(FrameLimits::default())
                .await
            else {
                continue;
            };
            let response = match request.request {
                BrokerRequest::HistoryUsage { .. } => {
                    dekopon_broker_protocol::BrokerResponse::History {
                        usage: usage.lock().clone(),
                    }
                }
                BrokerRequest::ReclaimChat { .. }
                | BrokerRequest::Capabilities { .. }
                | BrokerRequest::Run { .. }
                | BrokerRequest::Invoke { .. }
                | BrokerRequest::RecordDeliveredTurn { .. }
                | BrokerRequest::UpcallResult { .. }
                | BrokerRequest::PrepareGeneration { .. }
                | BrokerRequest::CommitGeneration { .. }
                | BrokerRequest::AbortGeneration { .. }
                | BrokerRequest::ObserveGeneration => {
                    dekopon_broker_protocol::BrokerResponse::ChatReclaimed {
                        outcome: dekopon_broker_protocol::ChatReclaim::Reclaimed {
                            namespaces: 0,
                            bytes: 0,
                        },
                    }
                }
            };
            let _ = stream
                .write_frame(
                    &ResponseEnvelope {
                        api_version: dekopon_broker_protocol::ProtocolVersion::V1Alpha3,
                        response,
                    },
                    &[],
                    FrameLimits::default(),
                )
                .await;
        }
    });
    ResolvedBroker {
        socket_path: socket,
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    }
}

#[tokio::test]
async fn an_unanswered_recording_stays_debited_across_a_restart_until_none_is_in_flight() {
    let stores = Stores::new();
    let journal = stores.journal();
    let usage = Arc::new(parking_lot::Mutex::new(
        dekopon_broker_protocol::HistoryUsage {
            root_free_bytes: u64::MAX,
            root_free_entries: u64::MAX,
            record_bound: 10_000,
            recording: 1,
            ..Default::default()
        },
    ));
    let broker = usage_broker(stores.directory.path(), Arc::clone(&usage)).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker.clone(), 15_000, IDLE);
    history
        .begin_record(OLDER, SystemTime::now())
        .await
        .unwrap()
        .answered()
        .await;
    drop(
        history
            .begin_record(OLDER, SystemTime::now())
            .await
            .unwrap(),
    );
    assert!(matches!(
        history.begin_record(NEWER, SystemTime::now()).await,
        Err(HistoryError::Exhausted)
    ));
    drop(history);

    let restarted = HistoryRetention::new(Arc::clone(&journal), broker, 15_000, IDLE);
    restarted.recover().await.unwrap();
    assert!(matches!(
        restarted.begin_record(NEWER, SystemTime::now()).await,
        Err(HistoryError::Exhausted)
    ));
    usage.lock().recording = 0;
    restarted
        .begin_record(NEWER, SystemTime::now())
        .await
        .unwrap()
        .answered()
        .await;
}

#[tokio::test]
async fn a_live_retry_keeps_an_earlier_reclaim_decision() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.record(&journal, OLDER, "c-older", SystemTime::now());
    stores.record(&journal, OLDER, "c-older-second", SystemTime::now());
    journal.write_reclaim_intent(OLDER).unwrap();
    let lease = File::open(stores.broker_namespace("c-older-second").join("base.lock")).unwrap();
    lease.lock().unwrap();
    let broker = stores.start_broker().await;
    let total = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, total + 1, IDLE);

    let admission = history.admit().await;
    assert!(matches!(
        admission.ensure("assistant/new", Growth::journal(2)).await,
        Err(HistoryError::Exhausted)
    ));
    drop(admission);
    assert!(stores.intent(OLDER).exists(), "the decision survives Busy");
    drop(lease);
    history.recover().await.unwrap();
    stores.assert_gone(OLDER, "c-older");
    stores.stop_broker().await;
}

#[tokio::test]
async fn an_aborted_reclaim_keeps_its_claim_until_the_intent_is_published() {
    let mut stores = Stores::new();
    let (journal, broker) = two_chats(&mut stores).await;
    let total = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, total + 1, IDLE);
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, unblock) = std::sync::mpsc::sync_channel::<()>(1);
    let held_journal = Arc::clone(&journal);
    let blocker = tokio::task::spawn_blocking(move || {
        let _held = held_journal.hold_io_for_test();
        locked.send(()).unwrap();
        unblock
            .recv_timeout(Duration::from_secs(10))
            .expect("journal lock released");
    });
    ready.await.unwrap();
    let admitting = {
        let history = Arc::clone(&history);
        tokio::spawn(async move {
            let admission = history.admit().await;
            admission.ensure("assistant/new", Growth::journal(2)).await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !history.claimed(OLDER) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the reclaim claims the oldest chat");
    tokio::time::sleep(Duration::from_millis(100)).await;
    admitting.abort();
    assert!(admitting.await.unwrap_err().is_cancelled());
    assert!(
        history.claimed(OLDER),
        "the claim waits for the intent's publication"
    );
    release.send(()).unwrap();
    blocker.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while history.claimed(OLDER) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the claim ends once the intent is published");
    assert!(stores.intent(OLDER).exists());
    stores.stop_broker().await;
}

#[tokio::test]
async fn a_busy_oldest_legacy_unit_does_not_hide_the_next_one() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    for (stem, conversation) in [(OLDER, "c-first"), (NEWER, "c-second")] {
        stores.record(&journal, stem, conversation, SystemTime::now());
        fs::remove_file(stores.transcript_path(stem)).unwrap();
        fs::remove_file(stores.activity(stem)).unwrap();
        fs::write(
            stores.broker_namespace(conversation).join("chat"),
            b"not a key",
        )
        .unwrap();
    }
    let broker = stores.start_broker().await;
    let oldest = broker
        .leg_client()
        .unwrap()
        .history_usage(Vec::new(), Vec::new())
        .await
        .unwrap()
        .oldest_unlinked
        .unwrap()
        .history;
    let token = oldest.as_str().trim_start_matches("unlinked/").to_owned();
    let leased = stores.storage().join("namespaces").join(&token);
    let lease = File::open(leased.join("base.lock")).unwrap();
    lease.lock().unwrap();
    let total = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker.clone(), total + 1, IDLE);

    let admission = history.admit().await;
    admission
        .ensure("assistant/new", Growth::journal(2))
        .await
        .unwrap();
    drop(admission);
    assert!(leased.exists(), "the leased unit is kept");
    drop(lease);
    stores.stop_broker().await;
}

#[tokio::test]
async fn unmeasured_broker_history_refuses_admission() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.record(&journal, OLDER, "c-older", SystemTime::now());
    std::os::unix::fs::symlink(
        "/nonexistent",
        stores.broker_namespace("c-older").join("stray"),
    )
    .unwrap();
    let broker = stores.start_broker().await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);

    let admission = history.admit().await;
    assert!(matches!(
        admission.ensure("assistant/new", Growth::journal(2)).await,
        Err(HistoryError::Unmeasured)
    ));
    drop(admission);
    stores.stop_broker().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_summary_transition_waits_for_room_or_is_not_written() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    let turn = captured("synthetic question", Some("synthetic answer"));
    let header = journal
        .append(
            OLDER,
            &Entry {
                at: SystemTime::now(),
                grant: &GrantDigest::of(&["probe.read".to_owned()]),
                captured: &turn,
                keep: Keep::Everything,
                plan: Some(crate::memory::record::SummaryPlan {
                    model: "summary".to_owned(),
                    parts: vec![std::num::NonZeroU32::new(8).unwrap()],
                    max_input_bytes: std::num::NonZeroU32::new(1024).unwrap(),
                }),
                attachments: Default::default(),
                arrivals: &[],
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap();
    let broker = stores.start_broker().await;
    let full = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, full, IDLE);
    let task = crate::memory::engine::SlotTask {
        stem: OLDER.to_owned(),
        record: header.id,
        part: std::num::NonZeroU8::MIN,
        subject: crate::memory::record::Subject::Record,
        access: None,
    };

    let written = {
        let journal = Arc::clone(&journal);
        tokio::task::spawn_blocking(move || {
            crate::memory::engine::write_slot(
                &journal,
                Some(&history),
                &task,
                &crate::memory::record::Slot::Queued,
                crate::memory::record::Slot::Running,
                None,
            )
        })
        .await
        .unwrap()
    };
    assert!(written.is_err(), "a full budget refuses the start line");
    assert_eq!(journal.history_bytes().unwrap(), full);
    stores.stop_broker().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_settlement_never_overwrites_a_later_debit() {
    let stores = Stores::new();
    let journal = stores.journal();
    let usage = Arc::new(parking_lot::Mutex::new(
        dekopon_broker_protocol::HistoryUsage {
            root_free_bytes: u64::MAX,
            root_free_entries: u64::MAX,
            record_bound: 1_000,
            recording: 1,
            ..Default::default()
        },
    ));
    let broker = usage_broker(stores.directory.path(), usage).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, BUDGET, IDLE);
    let first = history
        .begin_record(OLDER, SystemTime::now())
        .await
        .unwrap();
    let (release, gate) = std::sync::mpsc::sync_channel::<()>(1);
    let (written, settled) = std::sync::mpsc::sync_channel::<()>(1);
    journal.hold_next_debit_for_test((gate, written));
    let settling = tokio::spawn(first.answered());
    tokio::time::sleep(Duration::from_millis(100)).await;
    settling.abort();
    assert!(settling.await.unwrap_err().is_cancelled());

    let second = {
        let history = Arc::clone(&history);
        tokio::spawn(async move { history.begin_record(NEWER, SystemTime::now()).await })
    };
    // Fixed, the later recording waits for the held write; broken, it writes first.
    let _raced = tokio::time::timeout(Duration::from_secs(2), async {
        while journal.activity(NEWER).unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    })
    .await;
    release.send(()).unwrap();
    let second = second.await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || settled.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the held write finishes");
    assert_eq!(
        journal.debit().unwrap(),
        1_000,
        "the live recording stays debited"
    );
    second.answered().await;
}

#[tokio::test]
async fn a_legacy_namespace_bound_by_a_read_hands_over_its_clock_and_stays_claimed() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    stores.record(&journal, OLDER, "c-older", SystemTime::now());
    fs::remove_file(stores.transcript_path(OLDER)).unwrap();
    fs::remove_file(stores.activity(OLDER)).unwrap();
    let legacy_ms =
        crate::memory::journal::millis(SystemTime::now() - Duration::from_secs(10 * 24 * 3600));
    let adopted = stores.broker_namespace("c-older").join("adopted");
    fs::write(&adopted, legacy_ms.to_string()).unwrap();
    fs::set_permissions(&adopted, fs::Permissions::from_mode(0o600)).unwrap();
    let broker = stores.start_broker().await;
    let total = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, total + 1, IDLE);

    let in_use = history.begin_use(OLDER).await.unwrap();
    let admission = history.admit().await;
    assert!(matches!(
        admission.ensure("assistant/new", Growth::journal(2)).await,
        Err(HistoryError::Exhausted)
    ));
    drop(admission);
    assert!(
        stores.namespace_exists("c-older"),
        "the reading turn keeps it"
    );
    assert_eq!(journal.activity(OLDER).unwrap(), Some(legacy_ms));
    drop(in_use);

    let admission = history.admit().await;
    admission
        .ensure("assistant/new", Growth::journal(2))
        .await
        .unwrap();
    drop(admission);
    assert!(!stores.namespace_exists("c-older"));
    stores.stop_broker().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boot_transition_waits_for_room_or_is_not_written() {
    let mut stores = Stores::new();
    let journal = stores.journal();
    let turn = captured("synthetic question", Some("synthetic answer"));
    let header = journal
        .append(
            OLDER,
            &Entry {
                at: SystemTime::now(),
                grant: &GrantDigest::of(&["probe.read".to_owned()]),
                captured: &turn,
                keep: Keep::Everything,
                plan: Some(crate::memory::record::SummaryPlan {
                    model: "summary".to_owned(),
                    parts: vec![std::num::NonZeroU32::new(8).unwrap()],
                    max_input_bytes: std::num::NonZeroU32::new(1024).unwrap(),
                }),
                attachments: Default::default(),
                arrivals: &[],
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .unwrap();
    journal
        .write_slot(
            OLDER,
            crate::memory::record::SlotLine {
                at_ms: 1,
                record: header.id,
                part: std::num::NonZeroU8::MIN,
                slot: crate::memory::record::Slot::Running,
                subject: crate::memory::record::Subject::Record,
            },
        )
        .unwrap();
    let before = journal.history_bytes().unwrap();
    drop(journal);

    let (journal, boot) = Journal::boot(
        &stores.journal_dir(),
        IDLE,
        IDLE,
        IDLE,
        std::num::NonZeroUsize::new(64).unwrap(),
        SystemTime::now(),
    )
    .unwrap();
    let journal = Arc::new(journal);
    assert_eq!(boot.transitions.len(), 1, "the running slot is requeued");
    assert_eq!(
        journal.history_bytes().unwrap(),
        before,
        "boot writes nothing"
    );
    let broker = stores.start_broker().await;
    let full = used(&journal, &broker).await;
    let history = HistoryRetention::new(Arc::clone(&journal), broker, full, IDLE);
    {
        let journal = Arc::clone(&journal);
        tokio::task::spawn_blocking(move || {
            crate::memory::engine::settle_boot(&journal, Some(&history), boot.transitions);
        })
        .await
        .unwrap();
    }
    assert_eq!(
        journal.history_bytes().unwrap(),
        before,
        "a full budget refuses it"
    );
    stores.stop_broker().await;
}
