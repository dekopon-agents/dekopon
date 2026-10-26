use super::*;
use crate::{session::TaskFailure, transport::TransportError};
use dekopon_agent::attachment::GeneratedAssetStore;
use futures_util::future::BoxFuture;
use std::sync::atomic::AtomicUsize;

struct Gate {
    fetches: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    bytes: &'static [u8],
}

impl Gate {
    fn open(bytes: &'static [u8]) -> Arc<Self> {
        Self::with_permits(bytes, tokio::sync::Semaphore::MAX_PERMITS)
    }

    fn stalled(bytes: &'static [u8]) -> Arc<Self> {
        Self::with_permits(bytes, 0)
    }

    fn with_permits(bytes: &'static [u8], permits: usize) -> Arc<Self> {
        Arc::new(Self {
            fetches: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(permits),
            bytes,
        })
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

impl AssetFetcher for Gate {
    fn fetch(&self, _: &AssetSourceRef, _: u64) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            self.entered.notify_one();
            self.release
                .acquire()
                .await
                .expect("gate stays open")
                .forget();
            Ok(self.bytes.to_vec())
        })
    }
}

fn access(name: &str) -> AssetAccess {
    AssetAccess::persistent(
        ConversationKey::private(
            &"reviewer".parse().unwrap(),
            "dev",
            name,
            &"tel.15550000001".parse().unwrap(),
        ),
        1,
        Arc::new(AssetFence::new()),
    )
}

fn store(budget: usize) -> Arc<AssetStore> {
    Arc::new(AssetStore::with_retention(
        8,
        Duration::from_secs(600),
        budget,
    ))
}

fn register(store: &AssetStore, access: &AssetAccess, size: Option<u64>) -> u64 {
    store
        .assets_for_access(
            access,
            vec![PendingAsset {
                name: "input.png".into(),
                mime: "image/png".into(),
                size,
                source: Some(AssetSourceRef::Telegram {
                    file_id: "file".into(),
                }),
            }],
            Instant::now(),
        )
        .arrived[0]
}

fn session(store: &Arc<AssetStore>, access: &AssetAccess, gate: &Arc<Gate>) -> SessionAssets {
    SessionAssets::new(
        Arc::clone(store),
        access.clone(),
        Some(Arc::clone(gate) as Arc<dyn AssetFetcher>),
        Handle::current(),
        true,
    )
}

fn received(bytes: &[u8], encoding: AssetEncoding) -> (OwnedFd, NewAsset) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, bytes).unwrap();
    (
        std::fs::File::open(file.path()).unwrap().into(),
        NewAsset {
            descriptor: 0,
            content_type: "image/png".to_owned(),
            encoding,
            bytes: bytes.len() as u64,
            sha256: "0".repeat(64),
        },
    )
}

fn downloading(store: &AssetStore) -> usize {
    store
        .entries
        .lock()
        .values()
        .flat_map(|entry| &entry.assets)
        .filter(|row| match row.state {
            Attachment::Downloading(_) => true,
            Attachment::Registered
            | Attachment::Resident(_)
            | Attachment::Evicted
            | Attachment::NotStored(_)
            | Attachment::PlatformGone => false,
        })
        .count()
}

async fn settle(store: &AssetStore) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let mut tasks = store.downloads.tasks.lock();
                while tasks.try_join_next().is_some() {}
                if tasks.is_empty() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("download tasks end");
}

async fn assert_idle(store: &AssetStore, bytes: usize) {
    settle(store).await;
    assert_eq!(
        store.downloads.permits.available_permits(),
        MAX_CONCURRENT_DOWNLOADS
    );
    assert_eq!(downloading(store), 0);
    assert_eq!(store.retention.lock().bytes, bytes);
}

async fn stays_pending(future: &mut ResolveFuture<'_>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(20), future)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cold_asset_resolves_from_a_scoped_thread_outside_the_runtime() {
    let store = store(1024);
    let access = access("cold");
    let fetched_id = register(&store, &access, Some(10));
    let input_id = register(&store, &access, Some(10));
    let gate = Gate::open(b"pixels");
    let session = session(&store, &access, &gate);
    tokio::task::spawn_blocking(move || {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let fetched = session.fetch(fetched_id).expect("a cold model fetch");
                    assert_eq!(fetched.data.read().unwrap(), b"pixels");
                    let resolved = session
                        .runtime
                        .block_on(session.resolve_input(input_id))
                        .expect("a cold provider input");
                    assert_eq!(resolved.row.bytes, Some(6));
                    assert_eq!(resolved.blob.read().unwrap(), b"pixels");
                })
                .join()
                .unwrap();
        });
    })
    .await
    .unwrap();
    assert_eq!(gate.fetches(), 2);
    assert_idle(&store, 6).await;
}

#[tokio::test(start_paused = true)]
async fn a_stalled_download_from_a_scoped_thread_times_out_with_the_fetch_timeout_text() {
    let store = store(1024);
    let access = access("deadline");
    let id = register(&store, &access, None);
    let gate = Gate::stalled(b"late");
    let session = session(&store, &access, &gate);
    let fetch = tokio::task::spawn_blocking(move || {
        std::thread::scope(|scope| scope.spawn(|| session.fetch(id)).join().unwrap())
    });
    gate.entered.notified().await;
    tokio::time::advance(ASSET_FETCH_TIMEOUT).await;
    assert_eq!(
        fetch.await.unwrap().unwrap_err(),
        format!("chat-asset:{id} could not be read (fetch-timeout).")
    );
    assert_idle(&store, 0).await;
    assert!(store.pin(&access, id, Recency::Keep).unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn cancelling_the_sole_waiter_while_it_waits_for_a_permit_returns_the_entry_to_registered() {
    let store = store(1024);
    let access = access("permit");
    let id = register(&store, &access, None);
    let gate = Gate::open(b"bytes");
    let session = session(&store, &access, &gate);
    let held = Arc::clone(&store.downloads.permits)
        .acquire_many_owned(MAX_CONCURRENT_DOWNLOADS as u32)
        .await
        .unwrap();
    let mut pending = session.resolve_input(id);
    stays_pending(&mut pending).await;
    assert_eq!(downloading(&store), 1);
    drop(pending);
    settle(&store).await;
    drop(held);
    assert_idle(&store, 0).await;
    assert_eq!(gate.fetches(), 0);
}

#[tokio::test(start_paused = true)]
async fn cancelling_the_sole_waiter_mid_download_aborts_the_task_and_frees_its_permit() {
    let store = store(1024);
    let access = access("streaming");
    let id = register(&store, &access, None);
    let gate = Gate::stalled(b"bytes");
    let session = session(&store, &access, &gate);
    let pending = session.resolve_input(id);
    let mut pending = pending;
    stays_pending(&mut pending).await;
    gate.entered.notified().await;
    assert_eq!(
        store.downloads.permits.available_permits(),
        MAX_CONCURRENT_DOWNLOADS - 1
    );
    drop(pending);
    assert_idle(&store, 0).await;
    assert_eq!(gate.fetches(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_waiter_leaves_the_other_waiter_its_download() {
    let store = store(1024);
    let access = access("two");
    let id = register(&store, &access, None);
    let gate = Gate::stalled(b"shared");
    let session = session(&store, &access, &gate);
    let mut first = session.resolve_input(id);
    let mut second = session.resolve_input(id);
    stays_pending(&mut first).await;
    stays_pending(&mut second).await;
    drop(first);
    gate.release.add_permits(1);
    let resolved = second.await.unwrap();
    assert_eq!(resolved.blob.read().unwrap(), b"shared");
    assert_eq!(resolved.row.bytes, Some(6));
    assert_eq!(gate.fetches(), 1);
    drop(resolved);
    assert_idle(&store, 6).await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_every_waiter_lets_a_later_resolve_download_again() {
    let store = store(1024);
    let access = access("retry");
    let id = register(&store, &access, None);
    let gate = Gate::stalled(b"again");
    let session = session(&store, &access, &gate);
    let mut first = session.resolve_input(id);
    let mut second = session.resolve_input(id);
    stays_pending(&mut first).await;
    stays_pending(&mut second).await;
    drop((first, second));
    assert_idle(&store, 0).await;
    gate.release.add_permits(1);
    let resolved = session.resolve_input(id).await.unwrap();
    assert_eq!(resolved.blob.read().unwrap(), b"again");
    assert_eq!(gate.fetches(), 2);
    drop(resolved);
    assert_idle(&store, 5).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_racing_a_completion_never_publishes_twice_or_strands_a_download() {
    let store = store(1024);
    let access = access("race");
    let gate = Gate::open(b"race");
    let session = session(&store, &access, &gate);
    for round in 0..32_usize {
        let id = register(&store, &access, None);
        let before = gate.fetches();
        let mut pending = session.resolve_input(id);
        let _raced =
            tokio::time::timeout(Duration::from_micros(round as u64 * 50), &mut pending).await;
        drop(pending);
        let resolved = session.resolve_input(id).await.unwrap();
        assert_eq!(resolved.row.bytes, Some(4));
        assert!((1..=2).contains(&(gate.fetches() - before)));
    }
    assert_idle(&store, 4).await;
}

#[tokio::test]
async fn an_idle_one_shot_releases_its_bytes_before_a_live_resident_is_evicted() {
    let store = store(6);
    let expired = AssetAccess::one_shot(access("expired").key.conversation);
    let live = access("live");
    let kept = register(&store, &live, Some(3));
    drop(store.admit(&live, kept, b"kkk").unwrap());
    let stale = register(&store, &expired, Some(3));
    drop(store.admit(&expired, stale, b"sss").unwrap());
    let now = Instant::now();
    store.assets_for_access(&live, Vec::new(), now + Duration::from_secs(300));
    store.assets_for_access(&live, Vec::new(), now + Duration::from_secs(600));
    assert!(store.get_access(&expired, stale, Instant::now()).is_none());
    let generated = SessionAssets::new(
        Arc::clone(&store),
        live.clone(),
        None,
        Handle::current(),
        true,
    );
    let (descriptor, metadata) = received(b"ggg", AssetEncoding::Identity);
    generated
        .register(descriptor, &metadata, "image.edit", "invocation")
        .unwrap();
    assert!(store.pin(&live, kept, Recency::Keep).unwrap().is_some());
    assert_eq!(store.retention.lock().bytes, 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn five_generated_base64_images_spend_their_decoded_length_against_the_model_budget() {
    use super::retention_tests::{FetchScript, NoScripts};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use dekopon_agent::prompt::{History, PromptLimits, SessionInputs, run_prompt_session};
    use tracing_subscriber::prelude::*;

    let store = store(64 * 1024 * 1024);
    let session = Arc::new(SessionAssets::new(
        Arc::clone(&store),
        access("decoded"),
        None,
        Handle::current(),
        true,
    ));
    let raw = vec![0; dekopon_model::asset::MAX_ATTACHMENT_BYTES];
    let encoded = STANDARD.encode(&raw);
    for _ in 0..5 {
        let (descriptor, metadata) = received(encoded.as_bytes(), AssetEncoding::Base64);
        session
            .register(descriptor, &metadata, "image.edit", "invocation")
            .unwrap();
    }
    assert!(5 * raw.len() <= dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES);
    assert!(4 * encoded.len() > dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES);
    tokio::task::spawn_blocking(move || {
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let model = FetchScript {
            calls: AtomicUsize::new(0),
            last: Mutex::new(vec![]),
        };
        run_prompt_session(
            &model,
            &NoScripts,
            SessionInputs::new(
                "show each image",
                PromptLimits {
                    max_steps: 11,
                    max_capability_calls: 0,
                },
            )
            .with_assets(session.as_ref()),
            &mut History::default(),
        )
        .unwrap();
        let charged = raw.len().to_string();
        for id in 1..=5_u64 {
            let id = id.to_string();
            assert_eq!(
                capture
                    .events_where(
                        "dekopon_agent::audit",
                        &[
                            ("audit.event", "agent.asset.fetched"),
                            ("asset.id", &id),
                            ("asset.bytes", &charged)
                        ]
                    )
                    .len(),
                1
            );
            assert!(
                capture
                    .events_where(
                        "dekopon_agent::audit",
                        &[("audit.event", "agent.asset.refused"), ("asset.id", &id)]
                    )
                    .is_empty()
            );
        }
    })
    .await
    .unwrap();
}

#[test]
fn a_download_spawned_after_runtime_shutdown_refuses_instead_of_deadlocking() {
    use std::task::{Context, Poll, Waker};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.handle().clone();
    drop(runtime);
    let store = store(1024);
    let access = access("shutdown");
    let id = register(&store, &access, None);
    let session = SessionAssets::new(
        Arc::clone(&store),
        access,
        Some(Gate::open(b"never") as Arc<dyn AssetFetcher>),
        handle,
        true,
    );
    let (done, finished) = std::sync::mpsc::channel();
    let _detached = std::thread::spawn(move || {
        let mut pending = session.resolve_input(id);
        let polled = pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        done.send(matches!(polled, Poll::Ready(Err(_)))).unwrap();
    });
    assert!(
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("resolve returns instead of deadlocking")
    );
}

#[test]
fn a_second_completion_keeps_the_resident_length_and_charges_once() {
    let store = store(1024);
    let access = access("twice");
    let id = register(&store, &access, Some(3));
    drop(store.admit(&access, id, b"aaa").unwrap());
    let second = store.admit(&access, id, b"bbbbb").unwrap();
    assert_eq!(second.read().unwrap(), b"aaa");
    assert_eq!(store.get_inventory(&access)[0].size, Some(3));
    assert_eq!(store.retention.lock().bytes, 3);
}

#[tokio::test]
async fn a_panicking_session_task_surfaces_as_panicked_with_its_cause() {
    let error = tokio::task::spawn(async { panic!("asset store exploded") })
        .await
        .unwrap_err();
    assert!(matches!(
        TaskFailure::from(error),
        TaskFailure::Panicked { cause } if cause == "asset store exploded"
    ));
}

fn evict_with_pressure(store: &AssetStore, access: &AssetAccess, id: u64) {
    drop(store.admit(access, id, b"aaa").unwrap());
    let pressure = register(store, access, Some(3));
    drop(store.admit(access, pressure, b"bbb").unwrap());
    assert_eq!(
        store.pin(access, id, Recency::Keep),
        Err(BlobError::Reclaimed)
    );
}

struct FencedFetcher {
    access: AssetAccess,
    inner: Arc<dyn AssetFetcher>,
    calls: AtomicUsize,
}

impl AssetFetcher for FencedFetcher {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        limit: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        assert_eq!(
            self.access
                .with_active(|key| (key.generation, key.grant.as_str().to_owned())),
            Some((Some(1), "current-grant".into()))
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.fetch(source, limit)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_platform_refetch_uses_the_full_slack_uri_and_live_grant_once() {
    use crate::{
        config::SlackExperience,
        transport::{ChatTransport, slack::SlackTransport},
    };
    let peer = dekopon_test_support::LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\naaa",
    );
    let uri = format!("{}/prefix/files/F-private?download=1", peer.url());
    let transport = SlackTransport::new(
        "slack".into(),
        format!("{}/prefix", peer.url()),
        "fixture-app".into(),
        "fixture-bot".into(),
        SlackExperience::Classic,
    )
    .unwrap();
    let store = store(3);
    let mut access = access("slack-refetch");
    access.fence = Some(Arc::new(
        AssetFence::new().with_grant("current-grant".into()),
    ));
    access.key.grant = Arc::clone(&access.fence.as_ref().unwrap().grant);
    let id = store
        .assets_for_access(
            &access,
            vec![PendingAsset {
                name: "photo.png".into(),
                mime: "image/png".into(),
                size: Some(3),
                source: Some(AssetSourceRef::Slack {
                    file_id: "F-private".into(),
                    url: uri.clone(),
                }),
            }],
            Instant::now(),
        )
        .arrived[0];
    evict_with_pressure(&store, &access, id);
    let guarded = Arc::new(FencedFetcher {
        access: access.clone(),
        inner: transport.asset_fetcher().unwrap(),
        calls: AtomicUsize::new(0),
    });
    let session = SessionAssets::new(
        store,
        access,
        Some(Arc::clone(&guarded) as Arc<dyn AssetFetcher>),
        Handle::current(),
        true,
    );
    tokio::task::spawn_blocking(move || {
        let fetched = session.fetch(id).unwrap();
        assert_eq!(fetched.data.id(), id);
        assert_eq!(fetched.data.read().unwrap(), b"aaa");
    })
    .await
    .unwrap();
    assert_eq!(guarded.calls.load(Ordering::SeqCst), 1);
    let request = peer.request_text();
    let target = request
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    assert_eq!(format!("{}{}", peer.url(), target), uri);
    assert!(peer.recorded().is_empty());
    peer.join();
}

#[tokio::test]
async fn two_evicted_uses_join_one_download_and_keep_is_only_a_probe() {
    let store = store(3);
    let access = access("join-refetch");
    let id = register(&store, &access, Some(3));
    evict_with_pressure(&store, &access, id);
    let gate = Gate::stalled(b"aaa");
    let session = session(&store, &access, &gate);
    let used = store.retention.lock().index_snapshot();
    assert_eq!(
        store.pin(&access, id, Recency::Keep),
        Err(BlobError::Reclaimed)
    );
    assert_eq!(store.retention.lock().index_snapshot(), used);
    assert_eq!(gate.fetches(), 0);
    let first = session.resolve_input(id);
    let second = session.resolve_input(id);
    let release = async {
        gate.entered.notified().await;
        gate.release.add_permits(1);
    };
    let (first, second, ()) = tokio::join!(first, second, release);
    assert_eq!(first.unwrap().blob.read().unwrap(), b"aaa");
    assert_eq!(second.unwrap().blob.read().unwrap(), b"aaa");
    assert_eq!(gate.fetches(), 1);
}

#[tokio::test]
async fn a_retired_refetch_refuses_revoked_without_downloading() {
    let store = store(3);
    let access = access("revoked-refetch");
    let id = register(&store, &access, Some(3));
    evict_with_pressure(&store, &access, id);
    let gate = Gate::open(b"aaa");
    let session = session(&store, &access, &gate);
    access.fence.as_ref().unwrap().deactivate();
    assert_eq!(
        session
            .resolve_input(id)
            .await
            .map(|resolved| resolved.blob)
            .unwrap_err(),
        ChatAssetRefusal::Revoked
    );
    assert_eq!(gate.fetches(), 0);
}

struct ServiceFailure {
    code: &'static str,
    calls: AtomicUsize,
}
impl ServiceFailure {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            calls: AtomicUsize::new(0),
        }
    }
}
impl AssetFetcher for ServiceFailure {
    fn fetch(&self, _: &AssetSourceRef, _: u64) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(TransportError::Service {
                code: self.code.into(),
            })
        })
    }
}

#[tokio::test]
async fn platform_refusal_codes_name_platform_gone_and_other_codes_stay_retryable() {
    for code in [
        "404", "410", "401", "403", "http-404", "http-410", "http-401", "http-403", "500",
    ] {
        let store = store(3);
        let access = access(code);
        let id = register(&store, &access, Some(3));
        evict_with_pressure(&store, &access, id);
        let session = SessionAssets::new(
            Arc::clone(&store),
            access.clone(),
            Some(Arc::new(ServiceFailure::new(code))),
            Handle::current(),
            true,
        );
        let expected = if code == "500" {
            ChatAssetRefusal::Unavailable
        } else {
            ChatAssetRefusal::PlatformGone
        };
        assert_eq!(
            session
                .resolve_input(id)
                .await
                .map(|resolved| resolved.blob)
                .unwrap_err(),
            expected
        );
        assert_eq!(
            store.pin(&access, id, Recency::Keep),
            Err(BlobError::Reclaimed)
        );
        let gate = Gate::open(b"aaa");
        assert_eq!(
            self::session(&store, &access, &gate)
                .resolve_input(id)
                .await
                .unwrap()
                .blob
                .read()
                .unwrap(),
            b"aaa"
        );
        assert_eq!(gate.fetches(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn a_refetch_timeout_leaves_evicted_and_the_next_use_retries() {
    let store = store(3);
    let access = access("timeout-refetch");
    let id = register(&store, &access, Some(3));
    evict_with_pressure(&store, &access, id);
    let gate = Gate::stalled(b"aaa");
    let session = session(&store, &access, &gate);
    assert_eq!(
        session
            .resolve_input(id)
            .await
            .map(|resolved| resolved.blob)
            .unwrap_err(),
        ChatAssetRefusal::Unavailable
    );
    assert_eq!(
        store.pin(&access, id, Recency::Keep),
        Err(BlobError::Reclaimed)
    );
    gate.release.add_permits(1);
    assert_eq!(
        session
            .resolve_input(id)
            .await
            .unwrap()
            .blob
            .read()
            .unwrap(),
        b"aaa"
    );
    assert_eq!(gate.fetches(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_intake_is_silent_and_use_names_evicted_or_store_failed() {
    let store = store(3);
    let access = access("generated-refusal");
    let gate = Gate::open(b"aaa");
    let session = session(&store, &access, &gate);
    let (fd, metadata) = received(b"aaa", AssetEncoding::Identity);
    let generated = session
        .register(fd, &metadata, "asset.draw", "one")
        .unwrap();
    let pressure = register(&store, &access, Some(3));
    drop(store.admit(&access, pressure, b"bbb").unwrap());
    assert_eq!(
        session
            .resolve_input(generated)
            .await
            .map(|resolved| resolved.blob)
            .unwrap_err(),
        ChatAssetRefusal::Evicted
    );
    let (fd, metadata) = received(b"toolarge", AssetEncoding::Identity);
    let not_stored = session
        .register(fd, &metadata, "asset.draw", "two")
        .unwrap();
    assert_eq!(
        session
            .resolve_input(not_stored)
            .await
            .map(|resolved| resolved.blob)
            .unwrap_err(),
        ChatAssetRefusal::StoreFailed
    );
    let text = tokio::task::spawn_blocking(move || session.fetch(generated).unwrap_err())
        .await
        .unwrap();
    assert!(text.contains("evicted"), "{text}");
    assert_eq!(gate.fetches(), 0);
}

struct HistoryModel {
    inner: dekopon_model::blocking::BlockingModel,
    id: u64,
    calls: AtomicUsize,
}
impl dekopon_model::model::ChatModel for HistoryModel {
    fn complete(
        &self,
        messages: &[dekopon_model::model::ModelMessage],
        tools: &[dekopon_model::model::ModelTool],
        options: &dekopon_model::model::CompletionOptions,
        on_event: &mut (dyn FnMut(dekopon_model::TurnEvent) -> std::ops::ControlFlow<()> + Send),
    ) -> Result<dekopon_model::model::AssistantTurn, dekopon_model::error::InferenceError> {
        use dekopon_model::model::{AssistantTurn, ModelFunctionCall, ModelToolCall};
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (name, arguments) = match call {
            0 => ("fetch_chat_asset", serde_json::json!({"id": self.id})),
            1 => ("bash", serde_json::json!({"command": "evict"})),
            _ => return self.inner.complete(messages, tools, options, on_event),
        };
        Ok(AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: format!("call-{call}").into(),
                kind: "function".into(),
                function: ModelFunctionCall {
                    name: name.into(),
                    arguments: arguments.to_string(),
                },
            }],
            None,
        ))
    }
}

struct HistoryPressure {
    store: Arc<AssetStore>,
    access: AssetAccess,
    id: u64,
}
impl dekopon_agent::prompt::ScriptRuntime for HistoryPressure {
    fn capability_calls_used(&self) -> u32 {
        0
    }
    fn run_script(&self, script: &str) -> dekopon_shell::ScriptOutcome {
        assert_eq!(script, "evict");
        evict_with_pressure(&self.store, &self.access, self.id);
        dekopon_shell::ScriptOutcome {
            output: String::new(),
            exit_code: dekopon_process::ExitCode::SUCCESS,
            truncated: false,
            capability_calls: 0,
            steps: 1,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn encoding_a_history_image_refills_bytes_or_reports_platform_gone() {
    use dekopon_model::{blocking::BlockingModel, inference::ModelClient, openai::OpenAiClient};
    use tracing_subscriber::prelude::*;
    for gone in [false, true] {
        let store = store(3);
        let access = access("history-refetch");
        let id = register(&store, &access, Some(3));
        drop(store.admit(&access, id, b"aaa").unwrap());
        let gate = Gate::open(b"aaa");
        let mut session = session(&store, &access, &gate);
        let service = Arc::new(ServiceFailure::new("404"));
        if gone {
            session.fetcher = Some(Arc::clone(&service) as Arc<dyn AssetFetcher>);
        }
        let response = br#"{"choices":[{"message":{"role":"assistant","content":"seen"},"finish_reason":"stop"}]}"#;
        let response = [format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).into_bytes(), response.to_vec()].concat();
        let peer = dekopon_test_support::LoopbackServer::once(&response);
        let model = OpenAiClient::new(peer.url(), "fixture", None, Duration::from_secs(5))
            .unwrap()
            .with_streaming(false);
        let (_, cancel) = tokio::sync::watch::channel(false);
        let model = BlockingModel::new(
            Arc::new(ModelClient::OpenAiCompatible(model)),
            Handle::current(),
            cancel,
            Duration::from_secs(5),
        );
        tokio::task::spawn_blocking(move || {
            let capture = dekopon_test_support::CaptureLayer::workspace();
            let _guard = tracing_subscriber::registry()
                .with(capture.clone())
                .set_default();
            let model = HistoryModel {
                inner: model,
                id,
                calls: AtomicUsize::new(0),
            };
            let runtime = HistoryPressure { store, access, id };
            dekopon_agent::prompt::run_prompt_session(
                &model,
                &runtime,
                dekopon_agent::prompt::SessionInputs::new(
                    "show the photo, then use the script",
                    dekopon_agent::prompt::PromptLimits {
                        max_steps: 3,
                        max_capability_calls: 0,
                    },
                )
                .with_assets(&session),
                &mut dekopon_agent::prompt::History::default(),
            )
            .unwrap();
            assert_eq!(model.calls.load(Ordering::SeqCst), 3);
            let text = peer.request_text();
            let body: serde_json::Value =
                serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
            let part = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_array())
                .flatten()
                .find(|part| {
                    part["type"] == "image_url"
                        || part["text"]
                            .as_str()
                            .is_some_and(|text| text.contains("was released"))
                })
                .unwrap();
            if gone {
                assert_eq!(service.calls.load(Ordering::SeqCst), 1);
                assert_eq!(part["type"], "text");
                assert!(part["text"].as_str().unwrap().contains("was released"));
                assert!(capture.records().iter().any(|record| {
                    record.field("audit.event") == Some("agent.asset.refused")
                        && record
                            .field("reason")
                            .unwrap_or_default()
                            .contains("platform-gone")
                }));
                let events = capture.events_where(
                    "dekopon_gatewayd::audit",
                    &[
                        ("audit.event", "gateway.asset.refetch"),
                        ("outcome", "platform-gone"),
                    ],
                );
                assert_eq!(events.len(), 1);
            } else {
                assert_eq!(part["type"], "image_url");
                assert_eq!(part["image_url"]["url"], "data:image/png;base64,YWFh");
                assert_eq!(gate.fetches(), 1);
            }
            assert!(peer.recorded().is_empty());
            peer.join();
        })
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn inventory_accounting_matches_the_fitted_note_and_distinct_uses() {
    use dekopon_agent::prompt::{History, PromptLimits, SessionInputs, run_prompt_session};
    use tracing_subscriber::prelude::*;
    let store = store(6);
    let access = access("inventory-metrics");
    let first = register(&store, &access, Some(3));
    let second = register(&store, &access, Some(3));
    drop(store.admit(&access, first, b"aaa").unwrap());
    drop(store.admit(&access, second, b"bbb").unwrap());
    let failed = register(&store, &access, Some(7));
    assert_eq!(
        store.admit(&access, failed, b"ccccccc"),
        Err(BlobError::TooLarge)
    );
    let gate = Gate::open(b"aaa");
    let session = session(&store, &access, &gate);
    let registered = Registered {
        inventory: store.inventory(&access),
        arrived: vec![second],
    };
    let note = reference_note(&registered, true, TextReader::AssetCat, 16384);
    session.inventory.lock().note(&note, &registered);
    tokio::task::spawn_blocking(move || {
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let numeric = NumericEvents::default();
        let _guard = tracing_subscriber::registry()
            .with(numeric.clone())
            .with(capture.clone())
            .set_default();
        let model = dekopon_test_support::ScriptedStreamModel::scripted(
            vec![],
            dekopon_model::model::AssistantTurn::new(Some("seen".into()), vec![], None),
        );
        model.release_next();
        Handle::current()
            .block_on(session.resolve_input(first))
            .unwrap();
        Handle::current()
            .block_on(session.resolve_input(first))
            .unwrap();
        assert_eq!(
            Handle::current()
                .block_on(session.resolve_input(failed))
                .map(|resolved| resolved.blob)
                .unwrap_err(),
            ChatAssetRefusal::StoreFailed
        );
        let bytes = note.text.len();
        run_prompt_session(
            &model,
            &super::retention_tests::NoScripts,
            SessionInputs::new(
                &note.text,
                PromptLimits {
                    max_steps: 1,
                    max_capability_calls: 0,
                },
            )
            .with_assets(&session),
            &mut History::default(),
        )
        .unwrap();
        let records = capture.events_where(
            "dekopon_agent::audit",
            &[("audit.event", "accounting.model.turn")],
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].field("asset.inventory.count"), Some("3"));
        assert_eq!(
            records[0].field("asset.inventory.bytes"),
            Some(bytes.to_string().as_str())
        );
        assert_eq!(records[0].field("asset.inventory.referenced"), Some("1"));
        assert_eq!(records[0].field("asset.inventory.omitted"), Some("0"));
        let events = numeric.0.lock();
        let event = events
            .iter()
            .find(|event| event.name == "accounting.model.turn")
            .unwrap();
        assert_eq!(event.numbers.get("asset.inventory.count"), Some(&3));
        assert_eq!(
            event.numbers.get("asset.inventory.bytes"),
            Some(&(bytes as u64))
        );
        assert_eq!(event.numbers.get("asset.inventory.referenced"), Some(&1));
        assert_eq!(event.numbers.get("asset.inventory.omitted"), Some(&0));
    })
    .await
    .unwrap();
}

#[derive(Clone, Default)]
struct NumericEvents(Arc<Mutex<Vec<NumericEvent>>>);
#[derive(Default)]
struct NumericEvent {
    name: String,
    numbers: std::collections::BTreeMap<String, u64>,
}
impl tracing::field::Visit for NumericEvent {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "audit.event" {
            self.name = value.to_owned();
        }
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.numbers.insert(field.name().into(), value);
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.numbers
            .insert(field.name().into(), u64::try_from(value).unwrap());
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for NumericEvents {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = NumericEvent::default();
        event.record(&mut fields);
        self.0.lock().push(fields);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn asset_mutation_records_are_once_numeric_and_exclude_slack_sources() {
    use tracing_subscriber::prelude::*;
    let store = store(3);
    let access = access("store-records");
    let gate = Gate::open(b"aaa");
    let session = session(&store, &access, &gate);
    tokio::task::spawn_blocking(move || {
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let numeric = NumericEvents::default();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .with(numeric.clone())
            .set_default();
        let secret_url = "https://files.slack.com/files-pri/T-private/F-secret?token=bearer";
        let id = store
            .assets_for_access(
                &access,
                vec![PendingAsset {
                    name: "private.png".into(),
                    mime: "image/png".into(),
                    size: Some(3),
                    source: Some(AssetSourceRef::Slack {
                        file_id: "F-secret".into(),
                        url: secret_url.into(),
                    }),
                }],
                Instant::now(),
            )
            .arrived[0];
        evict_with_pressure(&store, &access, id);
        let resolved = Handle::current()
            .block_on(session.resolve_input(id))
            .unwrap();
        assert_eq!(resolved.blob.read().unwrap(), b"aaa");
        let (fd, metadata) = received(b"oversize", AssetEncoding::Identity);
        session
            .register(fd, &metadata, "asset.draw", "one")
            .unwrap();
        for (name, count) in [
            ("gateway.asset.evicted", 2),
            ("gateway.asset.refetch", 1),
            ("gateway.asset.store_failed", 1),
        ] {
            let records = capture.events_where("dekopon_gatewayd::audit", &[("audit.event", name)]);
            assert_eq!(records.len(), count, "{name}");
            let events = numeric.0.lock();
            let events: Vec<_> = events.iter().filter(|event| event.name == name).collect();
            assert_eq!(events.len(), count);
            for event in events {
                assert_eq!(event.numbers.get("asset.store.budget_bytes"), Some(&3));
                assert!(event.numbers.contains_key("asset.store.bytes"));
                if name != "gateway.asset.refetch" {
                    assert!(event.numbers.contains_key("asset.bytes"));
                }
            }
        }
        assert_eq!(
            capture.events_where(
                "dekopon_gatewayd::audit",
                &[("audit.event", "gateway.asset.refetch")]
            )[0]
            .field("outcome"),
            Some("refetched")
        );
        for sensitive in [secret_url, "F-secret", "T-private", "bearer"] {
            assert!(!capture.text().contains(sensitive));
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_refetch_readmission_names_store_failed_and_does_not_retry_storage() {
    let store = store(3);
    let access = access("readmission");
    let id = register(&store, &access, Some(3));
    evict_with_pressure(&store, &access, id);
    let pressure = store.inventory(&access).last().unwrap().id;
    let pin = store
        .pin(&access, pressure, Recency::Keep)
        .unwrap()
        .unwrap();
    let gate = Gate::open(b"aaa");
    let session = session(&store, &access, &gate);
    for _ in 0..2 {
        assert_eq!(
            session
                .resolve_input(id)
                .await
                .map(|resolved| resolved.blob)
                .unwrap_err(),
            ChatAssetRefusal::StoreFailed
        );
    }
    assert_eq!(gate.fetches(), 1);
    assert_eq!(pin.read().unwrap(), b"bbb");
}

#[tokio::test]
async fn a_refetch_waiting_for_a_permit_rechecks_revocation_before_the_transport() {
    let store = store(3);
    let access = access("queued-revocation");
    let id = register(&store, &access, Some(3));
    evict_with_pressure(&store, &access, id);
    let gate = Gate::open(b"aaa");
    let session = session(&store, &access, &gate);
    let permits = store.downloads.permits.acquire_many(2).await.unwrap();
    let mut use_asset = session.resolve_input(id);
    assert!(futures_util::poll!(&mut use_asset).is_pending());
    tokio::task::yield_now().await;
    access.fence.as_ref().unwrap().deactivate();
    drop(permits);
    assert_eq!(
        use_asset.await.map(|resolved| resolved.blob).unwrap_err(),
        ChatAssetRefusal::Revoked
    );
    assert_eq!(gate.fetches(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_larger_refill_cannot_sneak_duplicate_history_images_past_the_request_budget() {
    use dekopon_model::model::{ContentPart, ModelMessage};
    let store = store(64 * 1024 * 1024);
    let access = access("changed-length");
    let id = register(&store, &access, Some(3));
    drop(store.admit(&access, id, b"aaa").unwrap());
    let gate = Gate::open(b"aaaa");
    let session = session(&store, &access, &gate);
    tokio::task::spawn_blocking(move || {
        let original = session.fetch(id).unwrap();
        let mut parts = Vec::new();
        for index in 0..5 {
            let size = dekopon_model::asset::MAX_ATTACHMENT_BYTES - if index == 0 { 3 } else { 0 };
            let carried = register(&store, &access, Some(size as u64));
            drop(
                store
                    .admit(&access, carried, &vec![index as u8; size])
                    .unwrap(),
            );
            let image = session.fetch(carried).unwrap();
            parts.push(ContentPart::Image {
                mime: image.mime,
                data: image.data,
            });
        }
        {
            let mut entries = store.entries.lock();
            let row = row_mut(&mut entries, &access.key, id).unwrap();
            store
                .retention
                .lock()
                .evict((access.key.clone(), id), row)
                .unwrap();
        }
        for _ in 0..2 {
            parts.push(ContentPart::Image {
                mime: original.mime.clone(),
                data: original.data.clone(),
            });
        }
        let mut messages = vec![ModelMessage::user_with_parts(parts)];
        let request = session.prepare_request(&mut messages, 0);
        let parts = messages[0].parts().unwrap();
        for part in &parts[5..] {
            match part {
                ContentPart::Text(text) => assert!(text.contains("40 MiB decoded asset limit")),
                ContentPart::Image { .. } | ContentPart::File { .. } => {
                    panic!("the larger refill must not cross the byte budget")
                }
            }
        }
        assert_eq!(request.pins.len(), 5);
        assert_eq!(gate.fetches(), 1);
    })
    .await
    .unwrap();
}
