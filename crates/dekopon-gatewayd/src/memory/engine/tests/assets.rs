use super::*;
use crate::{
    asset::{
        AssetAccess, AssetFence, AssetSourceRef, PendingAsset, SessionAssets, retention_tests,
    },
    config::memory::{AttachmentSummaries, Toggle},
    conversation::ConversationKey,
    memory::{
        journal::{Entry, Keep, records::Header},
        record::AssetSideLine,
    },
};
use dekopon_agent::attachment::ChatAssetSource;
use dekopon_core::asset::AssetKind;

struct World {
    dir: Arc<tempfile::TempDir>,
    disk: Arc<tempfile::TempDir>,
    journal: Arc<Journal>,
    store: Arc<AssetStore>,
    access: AssetAccess,
    fence: Arc<AssetFence>,
    stem: String,
}
struct Live {
    access: AssetAccess,
    fence: Arc<AssetFence>,
    stem: String,
}
impl World {
    fn new() -> Self {
        let (dir, journal) = journal();
        let disk = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Arc::into_inner(retention_tests::store(8 * 1024 * 1024))
                .unwrap()
                .with_storage(disk.path())
                .unwrap(),
        );
        store.attach_journal(&journal).unwrap();
        let Live {
            access,
            fence,
            stem,
        } = Self::access("assets");
        Self {
            dir: Arc::new(dir),
            disk: Arc::new(disk),
            journal,
            store,
            access,
            fence,
            stem,
        }
    }
    fn access(channel: &str) -> Live {
        let key = ConversationKey::private(
            &"reviewer".parse().unwrap(),
            "dev",
            channel,
            &"tel.15550000001".parse().unwrap(),
        );
        let stem = key.journal_stem();
        let fence = Arc::new(AssetFence::new().with_grant(fixture().grant.hex()));
        Live {
            access: AssetAccess::persistent(key, 1, Arc::clone(&fence)),
            fence,
            stem,
        }
    }
    fn another(&self) -> Self {
        let Live {
            access,
            fence,
            stem,
        } = Self::access("other");
        Self {
            dir: Arc::clone(&self.dir),
            disk: Arc::clone(&self.disk),
            journal: Arc::clone(&self.journal),
            store: Arc::clone(&self.store),
            access,
            fence,
            stem,
        }
    }
    fn arrive(&self, mimes: &[&str], resident: Option<&[u8]>, parts: usize) -> Header {
        let pending = mimes
            .iter()
            .map(|mime| PendingAsset {
                name: "sender-name-canary".into(),
                mime: (*mime).into(),
                size: resident.map(|bytes| bytes.len() as u64),
                source: Some(AssetSourceRef::Telegram {
                    file_id: "source-canary".into(),
                }),
            })
            .collect();
        self.arrive_pending(pending, resident, parts)
    }
    fn arrive_pending(
        &self,
        pending: Vec<PendingAsset>,
        resident: Option<&[u8]>,
        parts: usize,
    ) -> Header {
        let registered = self
            .store
            .assets_for_access(&self.access, pending, Instant::now());
        if let Some(bytes) = resident {
            for number in &registered.arrived {
                drop(self.store.admit(&self.access, *number, bytes).unwrap());
            }
        }
        let mut plan = fixture().plan.unwrap();
        plan.parts.truncate(parts);
        self.journal
            .append(
                &self.stem,
                &Entry {
                    at: SystemTime::now(),
                    grant: &fixture().grant,
                    captured: &crate::tests::captured("conversation-canary", Some("reply-canary")),
                    keep: Keep::Everything,
                    plan: Some(plan),
                    attachments: AttachmentSummaries {
                        images: Toggle::Summarize,
                        documents: Toggle::Summarize,
                    },
                    arrivals: &[registered.arrived],
                    inventory: &registered.inventory,
                    next_asset_id: self.access.next_asset_id(),
                },
            )
            .unwrap()
            .unwrap()
    }
    fn task(&self, header: &Header, index: usize) -> SlotTask {
        SlotTask {
            stem: self.stem.clone(),
            record: header.id,
            subject: Subject::Asset(header.arrived[index].number),
            part: 1.try_into().unwrap(),
            access: Some(self.access.clone()),
        }
    }
    fn snapshot(&self) -> Snapshot {
        self.journal
            .record_snapshot(&self.stem, true)
            .unwrap()
            .unwrap()
    }
    fn slots(&self, header: &Header, index: usize) -> Vec<Slot> {
        let snapshot = self.snapshot();
        let head = snapshot
            .records
            .iter()
            .find(|head| head.header.id == header.id)
            .unwrap();
        head.header.arrived[index].join(&head.asset_lines).summaries
    }
    fn resources(&self, actions: impl IntoIterator<Item = Action>) -> (Resources, Arc<Script>) {
        let (mut resources, script) = resources(Arc::clone(&self.journal), actions, true);
        resources.assets = Arc::clone(&self.store);
        (resources, script)
    }
    fn sweep(&self, sender: &Sender) {
        let snapshot = self.snapshot();
        sender.sweep(
            &self.stem,
            &self.access,
            snapshot
                .records
                .iter()
                .map(|head| (&head.header, head.asset_lines.as_slice())),
        );
    }
    async fn run(
        &self,
        header: &Header,
        index: usize,
        actions: impl IntoIterator<Item = Action>,
    ) -> Arc<Script> {
        let (resources, script) = self.resources(actions);
        let _next = run(
            Arc::new(resources),
            Queued {
                task: self.task(header, index),
                since: Instant::now(),
            },
            watch::channel(false).1,
        )
        .await;
        script
    }
    fn write(&self, header: &Header, index: usize, part: u8, slot: Slot) {
        self.journal
            .write_slot(
                &self.stem,
                SlotLine {
                    at_ms: 2,
                    record: header.id,
                    subject: Subject::Asset(header.arrived[index].number),
                    part: part.try_into().unwrap(),
                    slot,
                },
            )
            .unwrap();
    }
    fn session(&self, fetcher: &Arc<retention_tests::Fetcher>) -> SessionAssets {
        SessionAssets::new(
            Arc::clone(&self.store),
            self.access.clone(),
            Some(Arc::clone(fetcher) as Arc<dyn crate::transport::AssetFetcher>),
            tokio::runtime::Handle::current(),
            true,
        )
    }
}

async fn idle(sender: &Sender) {
    sender.wait_assets().await;
}

#[tokio::test(start_paused = true)]
async fn a_part_in_flight_is_never_enqueued_twice() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 1);
    let gate = Gate::new();
    let (resources, script) = world.resources([Action::Hold(Arc::clone(&gate))]);
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        8.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    world.sweep(&sender);
    gate.started.notified().await;
    world.sweep(&sender);
    assert_eq!(script.calls.load(Ordering::Relaxed), 1);
    assert_eq!(sender.in_flight.lock().len(), 1);
    gate.release();
    idle(&sender).await;
    assert_eq!(world.slots(&header, 0), [Slot::Generated("held".into())]);
    let before = std::fs::read(world.dir.path().join(format!("{}.jsonl", world.stem))).unwrap();
    let stale = world.run(&header, 0, []).await;
    assert_eq!(stale.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        std::fs::read(world.dir.path().join(format!("{}.jsonl", world.stem))).unwrap(),
        before
    );
    owned.shutdown().await;
    assert!(sender.in_flight.lock().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_retired_generation_stops_a_queued_asset_task() {
    let (capture, _guard) = crate::tests::capture_spans();
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 1);
    let before = std::fs::read(world.dir.path().join(format!("{}.jsonl", world.stem))).unwrap();
    world.fence.deactivate();
    let script = world.run(&header, 0, []).await;
    assert_eq!(script.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        std::fs::read(world.dir.path().join(format!("{}.jsonl", world.stem))).unwrap(),
        before
    );
    assert!(capture.text().contains("revoked"), "{}", capture.text());
    assert_eq!(world.slots(&header, 0), [Slot::Queued]);
}

#[tokio::test(start_paused = true)]
async fn the_sweep_probe_never_audits() {
    let (capture, _guard) = crate::tests::capture_spans();
    let world = World::new();
    let header = world.arrive(&["image/png"; 5], Some(b"pixels"), 1);
    for asset in &header.arrived {
        retention_tests::reclaim(&world.store, &world.access, asset.number.into());
    }
    let before = std::fs::read(world.disk.path().join("index.json")).unwrap();
    let (sender, _dispatcher) = channel(
        Arc::clone(&world.journal),
        Arc::clone(&world.store),
        Arc::default(),
        8.try_into().unwrap(),
        1.try_into().unwrap(),
    );
    for _ in 0..10 {
        world.sweep(&sender);
    }
    assert!(sender.in_flight.lock().is_empty());
    assert_eq!(sender.counts.asset_enqueued.load(Ordering::Relaxed), 0);
    assert_eq!(
        std::fs::read(world.disk.path().join("index.json")).unwrap(),
        before
    );
    assert!(
        !capture.text().contains("gateway.asset.retention_miss"),
        "{}",
        capture.text()
    );
    assert_eq!(world.slots(&header, 0), [Slot::Queued]);
}

#[tokio::test(start_paused = true)]
async fn a_stale_task_panic_never_overwrites_a_generated_part() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 3);
    world.write(&header, 0, 1, Slot::Generated("kept".into()));
    let (resources, script) = world.resources([]);
    resources
        .counts
        .panic_before_walk
        .store(true, Ordering::Relaxed);
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        8.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    let task = world.task(&header, 0);
    sender
        .in_flight
        .lock()
        .insert((task.stem.clone(), task.record, header.arrived[0].number));
    sender
        .queue
        .try_send(Queued {
            task,
            since: Instant::now(),
        })
        .unwrap_or_else(|_| panic!("queue has room"));
    idle(&sender).await;
    let slots = world.slots(&header, 0);
    assert_eq!(slots[0], Slot::Generated("kept".into()));
    assert!(
        slots[1..]
            .iter()
            .all(|slot| failed(slot, SummaryFailure::Panicked))
    );
    assert_eq!(script.calls.load(Ordering::Relaxed), 0);
    owned.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn asset_parts_never_take_more_than_half_the_queue() {
    let world = Arc::new(World::new());
    let other = Arc::new(world.another());
    assert!(
        world
            .journal
            .record_snapshot(&world.stem, true)
            .unwrap()
            .is_none()
    );
    let header = world.arrive(&["image/png"; 12], Some(b"pixels"), 1);
    let other_header = other.arrive(&["image/png"; 12], Some(b"pixels"), 1);
    let (sender, dispatcher) = channel(
        Arc::clone(&world.journal),
        Arc::clone(&world.store),
        Arc::default(),
        8.try_into().unwrap(),
        1.try_into().unwrap(),
    );
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let mut sweeps = JoinSet::new();
    for (world, header) in [
        (Arc::clone(&world), header.clone()),
        (Arc::clone(&other), other_header.clone()),
    ] {
        let sender = sender.clone();
        let barrier = Arc::clone(&barrier);
        sweeps.spawn_blocking(move || {
            barrier.wait();
            sender.sweep(
                &world.stem,
                &world.access,
                std::iter::once((&header, &[][..])),
            );
        });
    }
    while let Some(joined) = sweeps.join_next().await {
        joined.unwrap();
    }
    assert_eq!(sender.in_flight.lock().len(), 4);
    assert_eq!(sender.counts.asset_enqueued.load(Ordering::Relaxed), 4);
    for (world, header) in [(&world, &header), (&other, &other_header)] {
        sender
            .enqueue(SlotTask {
                subject: Subject::Record,
                access: None,
                ..world.task(header, 0)
            })
            .await;
    }
    assert_eq!(sender.counts.overflowed.load(Ordering::Relaxed), 0);
    let (resources, script) = world.resources((0..26).map(|_| Action::Text("done")));
    let mut owned = JoinSet::new();
    dispatcher.start(
        resources.current,
        resources.models,
        resources.thread_owner,
        resources.shell_threads,
        &mut owned,
    );
    idle(&sender).await;
    for _ in 0..6 {
        world.sweep(&sender);
        other.sweep(&sender);
        assert!(sender.in_flight.lock().len() <= 4);
        idle(&sender).await;
    }
    assert_eq!(sender.counts.asset_enqueued.load(Ordering::Relaxed), 24);
    assert!(sender.counts.asset_deferred.load(Ordering::Relaxed) > 0);
    for (world, header) in [(&world, &header), (&other, &other_header)] {
        assert!(
            world
                .slots(header, 11)
                .iter()
                .all(|slot| matches!(slot, Slot::Generated(_)))
        );
    }
    assert!(script.calls.load(Ordering::Relaxed) >= 24);
    owned.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn no_summary_fetches_and_nothing_off_the_allowlist_is_read() {
    let world = World::new();
    let disallowed = world.arrive(
        &[
            "video/quicktime",
            "application/zip",
            "text/x-python",
            "application/yaml",
            "image/svg+xml",
            "image/heic",
        ],
        Some(b"opaque"),
        1,
    );
    assert!(
        disallowed
            .arrived
            .iter()
            .all(|asset| asset.kind == AssetKind::Other && asset.summaries.is_empty())
    );
    let generated = world.arrive_pending(
        vec![PendingAsset {
            name: "generated.png".into(),
            mime: "image/png".into(),
            size: Some(6),
            source: Some(AssetSourceRef::Generated {
                capability: "image.generate".into(),
                invocation: "synthetic".into(),
            }),
        }],
        Some(b"pixels"),
        1,
    );
    assert!(generated.arrived.is_empty());
    let unfetched = world.arrive(&["image/png"], None, 1);
    let fetcher = Arc::new(retention_tests::Fetcher::default());
    let _session = world.session(&fetcher);
    let text = vec![b'x'; 1024 * 1024];
    let text_header = world.arrive(&["text/plain"], Some(&text), 1);
    let _permits = retention_tests::hold_downloads(&world.store).await;
    let (resources, script) = world.resources([Action::Text("text")]);
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        8.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    for _ in 0..10 {
        world.sweep(&sender);
        idle(&sender).await;
    }
    assert_eq!(script.calls.load(Ordering::Relaxed), 1);
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 0);
    assert_eq!(fetcher.bytes.load(Ordering::Relaxed), 0);
    assert_eq!(world.slots(&unfetched, 0), [Slot::Queued]);
    assert_eq!(
        world.slots(&text_header, 0),
        [Slot::Generated("text".into())]
    );
    {
        let inputs = script.inputs.lock();
        assert!(inputs[0].ends_with(&"x".repeat(128 * 1024)));
        assert!(!inputs[0].contains(&"x".repeat(128 * 1024 + 1)));
    }
    match world
        .store
        .summary_input(
            &world.access,
            text_header.arrived[0].number,
            AssetKind::TextDocument,
        )
        .unwrap()
    {
        crate::asset::SummaryInput::Text(text) => assert_eq!(text.len(), 256 * 1024),
        crate::asset::SummaryInput::Image { .. } | crate::asset::SummaryInput::Binary { .. } => {
            panic!("text input")
        }
    }
    owned.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_evicted_asset_keeps_its_summary_and_waits_for_its_bytes() {
    let world = World::new();
    let first = world.arrive(&["image/png"], Some(b"pixels"), 1);
    world.run(&first, 0, [Action::Text("caption")]).await;
    retention_tests::reclaim(&world.store, &world.access, first.arrived[0].number.into());
    assert_eq!(world.slots(&first, 0), [Slot::Generated("caption".into())]);
    let second = world.arrive(&["image/png", "image/png"], Some(b"more pixels"), 1);
    for asset in &second.arrived {
        retention_tests::reclaim(&world.store, &world.access, asset.number.into());
    }
    let fetcher = Arc::new(retention_tests::Fetcher::default());
    let session = world.session(&fetcher);
    let (resources, script) = world.resources([Action::Text("back")]);
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        8.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    world.sweep(&sender);
    assert!(sender.in_flight.lock().is_empty());
    assert_eq!(script.calls.load(Ordering::Relaxed), 0);
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 0);
    drop(
        session
            .resolve_input(second.arrived[0].number.into())
            .await
            .unwrap(),
    );
    world.sweep(&sender);
    idle(&sender).await;
    assert_eq!(world.slots(&second, 0), [Slot::Generated("back".into())]);
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
    fetcher.gone.store(true, Ordering::Relaxed);
    assert!(
        session
            .resolve_input(second.arrived[1].number.into())
            .await
            .is_err()
    );
    world.sweep(&sender);
    assert_eq!(world.slots(&second, 1), [Slot::Queued]);
    assert_eq!(script.calls.load(Ordering::Relaxed), 1);
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 2);
    owned.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_asset_chain_stop_leaves_the_record_chain_running() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 2);
    world.run(&header, 0, [Action::Refused]).await;
    assert!(
        world
            .slots(&header, 0)
            .iter()
            .all(|slot| failed(slot, SummaryFailure::Refused))
    );
    let (resources, script) = world.resources([Action::Text("record1"), Action::Text("record2")]);
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        8.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    sender
        .enqueue(SlotTask {
            subject: Subject::Record,
            access: None,
            ..world.task(&header, 0)
        })
        .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let joined = sender.counts.joined.notified();
            if world.snapshot().records[0]
                .header
                .summaries
                .iter()
                .all(|slot| !slot.is_pending())
            {
                break;
            }
            joined.await;
        }
    })
    .await
    .unwrap();
    assert_eq!(script.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        world.snapshot().records[0].header.summaries[1],
        Slot::Generated("record2".into())
    );
    owned.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_restart_mid_asset_summary_resumes_on_the_next_turn() {
    let mut world = World::new();
    let header = world.arrive(&["image/png"], None, 1);
    let fetcher = Arc::new(retention_tests::Fetcher::default());
    drop(
        world
            .session(&fetcher)
            .resolve_input(header.arrived[0].number.into())
            .await
            .unwrap(),
    );
    let (endpoint, seen, closed, mut server) = held_listener().await;
    let mut resources = live_resources(Arc::clone(&world.journal), &endpoint, 60_000);
    resources.assets = Arc::clone(&world.store);
    let mut owned = JoinSet::new();
    let sender = spawn(
        resources,
        8.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    world.sweep(&sender);
    seen.await.unwrap();
    assert_eq!(world.slots(&header, 0), [Slot::Running]);
    owned.shutdown().await;
    closed.await.unwrap();
    server.join_next().await.unwrap().unwrap();
    assert!(sender.in_flight.lock().is_empty());
    drop(sender);
    let (journal, boot) = Journal::open_records(
        world.dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
    )
    .unwrap();
    assert_eq!(boot.requeued, 1);
    assert!(
        boot.tasks
            .iter()
            .all(|task| task.subject == Subject::Record)
    );
    world.journal = Arc::new(journal);
    world.store = Arc::new(
        Arc::into_inner(retention_tests::store(8 * 1024 * 1024))
            .unwrap()
            .with_storage(world.disk.path())
            .unwrap(),
    );
    world.store.attach_journal(&world.journal).unwrap();
    let recalled = world
        .journal
        .recall(
            &world.stem,
            &fixture().grant,
            retention_tests::durable_window(),
            SystemTime::now(),
        )
        .unwrap();
    world.store.restore(
        &world.access,
        recalled.assets,
        recalled.next_asset_id,
        Instant::now(),
    );
    assert_eq!(world.slots(&header, 0), [Slot::Queued]);
    let (resources, script) = world.resources([Action::Text("resumed")]);
    let sender = spawn(
        resources,
        8.try_into().unwrap(),
        1.try_into().unwrap(),
        &mut owned,
    );
    assert_eq!(script.calls.load(Ordering::Relaxed), 0);
    let before = world.snapshot();
    assert!(
        matches!(&before.records[0].asset_lines[..], [AssetSideLine::Slot(_), AssetSideLine::Slot(line)] if line.slot == Slot::Queued)
    );
    world.sweep(&sender);
    idle(&sender).await;
    assert_eq!(script.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        &*script.parts.lock(),
        &[SeenPart::Image {
            mime: "image/png".into(),
            bytes: b"aaa".to_vec()
        }]
    );
    assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
    let bytes = asset_lines(&world);
    world
        .journal
        .maintain(&world.stem, SystemTime::now(), world.access.next_asset_id());
    assert_eq!(asset_lines(&world), bytes);
    assert_eq!(
        serde_json::to_value(&world.snapshot().records[0].header.arrived).unwrap(),
        serde_json::to_value(&before.records[0].header.arrived).unwrap()
    );
    owned.shutdown().await;
}

fn asset_lines(world: &World) -> Vec<String> {
    std::fs::read_to_string(world.dir.path().join(format!("{}.jsonl", world.stem)))
        .unwrap()
        .lines()
        .filter(|line| line.contains("\"subject\""))
        .map(str::to_owned)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_running_asset_part_at_boot_is_written_queued() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 1);
    world.write(&header, 0, 1, Slot::Running);
    let (journal, boot) = Journal::open_records(
        world.dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
    )
    .unwrap();
    assert_eq!(boot.requeued, 1);
    assert!(
        boot.tasks
            .iter()
            .all(|task| task.subject == Subject::Record)
    );
    let snapshot = journal.record_snapshot(&world.stem, true).unwrap().unwrap();
    assert!(
        matches!(snapshot.records[0].asset_lines.last(), Some(AssetSideLine::Slot(line)) if line.slot == Slot::Queued)
    );
    assert_eq!(world.slots(&header, 0), [Slot::Queued]);
    let (sender, mut dispatcher) = channel(
        Arc::clone(&world.journal),
        Arc::clone(&world.store),
        Arc::default(),
        8.try_into().unwrap(),
        1.try_into().unwrap(),
    );
    world.sweep(&sender);
    let queued = dispatcher.receiver.try_recv().unwrap();
    assert_eq!(
        queued.task.subject,
        Subject::Asset(header.arrived[0].number)
    );
    assert_eq!(queued.task.part.get(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_asset_send_releases_its_reservation() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 1);
    let (sender, dispatcher) = channel(
        Arc::clone(&world.journal),
        Arc::clone(&world.store),
        Arc::default(),
        2.try_into().unwrap(),
        1.try_into().unwrap(),
    );
    for _ in 0..2 {
        sender
            .queue
            .try_send(Queued {
                task: world.task(&header, 0),
                since: Instant::now(),
            })
            .unwrap_or_else(|_| panic!("queue has room"));
    }
    world.sweep(&sender);
    assert!(sender.in_flight.lock().is_empty());
    assert_eq!(world.slots(&header, 0), [Slot::Queued]);
    drop(dispatcher);
    world.sweep(&sender);
    assert!(sender.in_flight.lock().is_empty());
    assert_eq!(sender.counts.overflowed.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn binary_summaries_use_only_the_canonical_name_and_earlier_generated_parts() {
    let world = World::new();
    let header = world.arrive(&["application/pdf"], Some(b"%PDF-1.4 whole file"), 3);
    world.write(&header, 0, 1, Slot::Generated("short".into()));
    world.write(
        &header,
        0,
        2,
        error_slot(SummaryFailure::Empty, "error-canary"),
    );
    let (resources, script) = world.resources([Action::Text("longer description")]);
    let task = SlotTask {
        part: 3.try_into().unwrap(),
        ..world.task(&header, 0)
    };
    let _next = run(
        Arc::new(resources),
        Queued {
            task,
            since: Instant::now(),
        },
        watch::channel(false).1,
    )
    .await;
    assert_eq!(
        &*script.parts.lock(),
        &[SeenPart::File {
            name: format!("chat-asset-{}", header.arrived[0].number),
            mime: "application/pdf".into(),
            bytes: b"%PDF-1.4 whole file".to_vec()
        }]
    );
    let inputs = script.inputs.lock();
    assert!(inputs[0].contains("Earlier, shorter descriptions:\nshort"));
    for canary in [
        "sender-name-canary",
        "source-canary",
        "conversation-canary",
        "error-canary",
    ] {
        assert!(!inputs[0].contains(canary));
    }
    assert_eq!(
        world.slots(&header, 0)[2],
        Slot::Generated("longer d".into())
    );
}

#[tokio::test(start_paused = true)]
async fn an_image_model_removed_by_reload_stops_only_nonfinal_asset_parts() {
    let world = World::new();
    let header = world.arrive(&["image/png"], Some(b"pixels"), 3);
    world.write(&header, 0, 3, Slot::Generated("kept".into()));
    let (mut resources, script) = world.resources([]);
    let mut routes = RoutingTable::default();
    routes.models.insert("summary".into(), Arc::new(serde_json::from_value(serde_json::json!({"kind":"openaiCompatible","name":"summary","endpoint":"http://127.0.0.1:9","model":"fixture","timeoutMs":1000})).unwrap()));
    resources.current = CurrentGeneration::boot(Arc::new(routes), Arc::default());
    let _next = run(
        Arc::new(resources),
        Queued {
            task: world.task(&header, 0),
            since: Instant::now(),
        },
        watch::channel(false).1,
    )
    .await;
    let slots = world.slots(&header, 0);
    assert!(
        slots[..2]
            .iter()
            .all(|slot| failed(slot, SummaryFailure::Unconfigured))
    );
    assert_eq!(slots[2], Slot::Generated("kept".into()));
    assert_eq!(script.calls.load(Ordering::Relaxed), 0);
}
