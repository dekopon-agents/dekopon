use dekopon_capability::{StorageAccess, StorageInterface, StorageScope};
use dekopon_core::history::HistoryKey;
use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits};

use super::*;
use crate::memory::{
    journal::{Entry, Journal, Keep},
    record::GrantDigest,
    retention::{HistoryError, HistoryRetention},
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

    // Both copies of a chat: the broker namespace through the real storage host, the transcript
    // through the real journal.
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

    async fn start_broker(&mut self) -> ResolvedBroker {
        let socket = self.directory.path().join("broker.sock");
        let config_path = self.directory.path().join("broker.json");
        let policy_path = self.directory.path().join("policies.cedar");
        fs::write(
            &policy_path,
            r#"@id("prompt") permit(principal, action == Dekopon::Action::"agent.prompt", resource);"#,
        )
        .unwrap();
        let mut storage = serde_json::to_value(StorageLimits::default()).unwrap();
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

    fn transcript(&self, stem: &str) -> PathBuf {
        self.journal_dir().join(format!("{stem}.jsonl"))
    }

    fn activity(&self, stem: &str) -> PathBuf {
        self.journal_dir().join(format!("{stem}.activity"))
    }

    fn intent(&self, stem: &str) -> PathBuf {
        self.journal_dir().join(format!("{stem}.reclaim"))
    }

    fn assert_present(&self, stem: &str, conversation: &str) {
        assert!(self.transcript(stem).exists(), "{stem} transcript");
        assert!(self.activity(stem).exists(), "{stem} clock");
        assert!(self.namespace_exists(conversation), "{stem} broker copy");
    }

    fn assert_gone(&self, stem: &str, conversation: &str) {
        assert!(!self.transcript(stem).exists(), "{stem} transcript");
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
    let usage = broker.leg_client().unwrap().history_usage().await.unwrap();
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
    admission.ensure("assistant/new", 2).await.unwrap();
    drop(admission);
    stores.assert_gone(OLDER, "c-older");
    stores.assert_present(NEWER, "c-newer");

    let admission = history.admit().await;
    admission.ensure("assistant/new", budget).await.unwrap();
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

    let in_use = history.begin_use(OLDER).await;
    let admission = history.admit().await;
    admission.ensure("assistant/new", 2).await.unwrap();
    drop(admission);
    stores.assert_present(OLDER, "c-older");
    stores.assert_gone(NEWER, "c-newer");

    let admission = history.admit().await;
    assert!(matches!(
        admission.ensure("assistant/new", total + 1).await,
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
            fs::remove_file(stores.transcript(OLDER)).unwrap();
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

    let read = history.begin_use(NEWER).await;
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
