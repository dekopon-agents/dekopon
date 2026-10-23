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
            Attachment::Registered | Attachment::Resident(_) | Attachment::Evicted => false,
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
    assert_idle(&store, 12).await;
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
    assert_idle(&store, 32 * 4).await;
}

#[tokio::test]
async fn an_expired_conversation_releases_its_bytes_before_a_live_resident_is_evicted() {
    let store = store(6);
    let (expired, live) = (access("expired"), access("live"));
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
async fn a_generated_base64_image_reference_spends_its_decoded_length() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let store = store(1024);
    let access = access("decoded");
    let session = Arc::new(SessionAssets::new(
        Arc::clone(&store),
        access,
        None,
        Handle::current(),
        true,
    ));
    let raw = b"\x89PNG\r\n\x1a\ngenerated pixels";
    let (descriptor, metadata) = received(STANDARD.encode(raw).as_bytes(), AssetEncoding::Base64);
    let id = session
        .register(descriptor, &metadata, "image.edit", "invocation")
        .unwrap();
    let fetched = tokio::task::spawn_blocking(move || session.fetch(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.data.len(), raw.len());
    assert_eq!(fetched.data.read().unwrap(), raw);
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
