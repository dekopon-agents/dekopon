//! This lives beside transport credentials in the gateway, not the broker, because resolving an
//! attachment reference is reading already-authenticated data, not deciding whether an effect may
//! happen.

use parking_lot::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    io::Read,
    os::fd::OwnedFd,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use dekopon_agent::attachment::GeneratedImage;
use dekopon_agent::{
    attachment::{ChatAssetRefusal, ChatAssetSource, ResolveFuture, ResolvedInput},
    prompt::{AssetSource, FetchedAsset},
};
use dekopon_broker_protocol::{AssetEncoding, AssetRow, NewAsset};
use dekopon_model::asset::{BlobError, BlobReference, BlobSource, DiskBlob};
use serde::{Deserialize, Serialize};
use tokio::{
    runtime::Handle,
    sync::{Semaphore, watch},
    task::{AbortHandle, JoinSet},
};

pub(crate) const MAX_SENDS_PER_TURN: u8 = 4;

use crate::{conversation::ConversationKey, transport::AssetFetcher};

pub(crate) const MAX_ASSETS_PER_CONVERSATION: usize = dekopon_broker_protocol::MAX_ASSET_ROWS;

/// Debug omits the source field because Slack private URLs and Discord signed CDN URLs function as
/// bearer capabilities, not safe-to-log metadata.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AssetRef {
    pub id: u64,
    pub name: String,
    pub mime: String,
    pub size: Option<u64>,
    pub source: Option<AssetSourceRef>,
    pub arrived: SystemTime,
    encoding: AssetEncoding,
    sent: bool,
}

impl fmt::Debug for AssetRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssetRef")
            .field("id", &self.id)
            .field("mime", &self.mime)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum AssetSourceRef {
    Generated {
        capability: String,
        invocation: String,
    },
    Slack {
        file_id: String,
        url: String,
    },
    Discord {
        attachment_id: String,
        channel_id: String,
        message_id: String,
        url: String,
    },
    WhatsApp {
        media_id: String,
        mime: String,
    },
    Telegram {
        file_id: String,
    },
}

impl fmt::Debug for AssetSourceRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generated { .. } => formatter.write_str("Generated"),
            Self::Slack { file_id, .. } => formatter
                .debug_struct("Slack")
                .field("file_id", file_id)
                .finish_non_exhaustive(),
            Self::Discord { attachment_id, .. } => formatter
                .debug_struct("Discord")
                .field("attachment_id", attachment_id)
                .finish_non_exhaustive(),
            Self::WhatsApp { media_id, .. } => formatter
                .debug_struct("WhatsApp")
                .field("media_id", media_id)
                .finish_non_exhaustive(),
            Self::Telegram { file_id } => formatter
                .debug_struct("Telegram")
                .field("file_id", file_id)
                .finish(),
        }
    }
}

/// Asset publication and lookup share the same gate as invalidation, so an asset operation either
/// finishes before invalidation or observes the retired generation afterward, never a mix.
pub(crate) struct AssetFence {
    gate: Mutex<()>,
    active: AtomicBool,
    next_asset_id: AtomicU64,
}

impl AssetFence {
    pub fn new() -> Self {
        Self {
            gate: Mutex::new(()),
            active: AtomicBool::new(true),
            next_asset_id: AtomicU64::new(1),
        }
    }

    pub fn deactivate(&self) {
        let _gate = self.gate.lock();
        self.active.store(false, Ordering::Release);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// A generation number is never reused for the life of its conversation and asset stores, so an
/// asset id from a retired generation can never alias one from its replacement.
#[derive(Clone, Eq, Hash, PartialEq)]
struct AssetStateKey {
    conversation: ConversationKey,
    generation: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct AssetAccess {
    key: AssetStateKey,
    fence: Option<Arc<AssetFence>>,
}

impl AssetAccess {
    pub fn one_shot(conversation: ConversationKey) -> Self {
        Self {
            key: AssetStateKey {
                conversation,
                generation: None,
            },
            fence: None,
        }
    }

    pub fn persistent(
        conversation: ConversationKey,
        generation: u64,
        fence: Arc<AssetFence>,
    ) -> Self {
        Self {
            key: AssetStateKey {
                conversation,
                generation: Some(generation),
            },
            fence: Some(fence),
        }
    }

    pub fn next_asset_id(&self) -> u64 {
        self.fence
            .as_ref()
            .map_or(1, |fence| fence.next_asset_id.load(Ordering::Acquire))
    }

    fn with_active<T>(&self, operation: impl FnOnce(&AssetStateKey) -> T) -> Option<T> {
        let Some(fence) = self.fence.as_ref() else {
            return Some(operation(&self.key));
        };
        let _gate = fence.gate.lock();
        fence.is_active().then(|| operation(&self.key))
    }

    fn is_active(&self) -> bool {
        self.with_active(|_| ()).is_some()
    }

    pub(crate) fn generation(&self) -> Option<u64> {
        self.key.generation
    }

    fn weak_fence(&self) -> Option<Weak<AssetFence>> {
        self.fence.as_ref().map(Arc::downgrade)
    }

    fn allocate_id(&self, next_one_shot_id: &AtomicU64) -> u64 {
        if let Some(fence) = self.fence.as_ref() {
            return fence
                .next_asset_id
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
                .expect("conversation asset identifier space exhausted");
        }
        next_one_shot_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .expect("one-shot asset identifier space exhausted")
    }
}

pub(crate) struct AssetStore {
    conversations: usize,
    idle_timeout: Duration,
    entries: Mutex<HashMap<AssetStateKey, ConversationAssets>>,
    retention: Mutex<Retention>,
    downloads: Downloads,
    next_one_shot_id: AtomicU64,
}

struct ConversationAssets {
    assets: Vec<Row>,
    touched: Instant,
    fence: Option<Weak<AssetFence>>,
    delivery_failed: bool,
}

impl ConversationAssets {
    fn new(fence: Option<Weak<AssetFence>>, now: Instant) -> Self {
        Self {
            assets: Vec::new(),
            touched: now,
            fence,
            delivery_failed: false,
        }
    }
}

struct Row {
    asset: AssetRef,
    state: Attachment,
}

/// A row that leaves the table is Gone: its id survives only as a bounded release tombstone.
enum Attachment {
    Registered,
    Downloading(Downloading),
    Resident(Resident),
    Evicted,
}

struct Downloading {
    attempt: u64,
    task: AbortHandle,
    interests: usize,
    outcome: watch::Receiver<Option<Completion>>,
}

type Fetched = Result<DiskBlob, AssetFailure>;
type Completion = Result<(), AssetFailure>;

const MAX_CONCURRENT_DOWNLOADS: usize = 2;

struct Downloads {
    permits: Arc<Semaphore>,
    tasks: Mutex<JoinSet<()>>,
    attempts: AtomicU64,
}

impl Downloads {
    fn spawn(
        &self,
        task: impl Future<Output = ()> + Send + 'static,
        runtime: &Handle,
    ) -> AbortHandle {
        let mut tasks = self.tasks.lock();
        while let Some(finished) = tasks.try_join_next() {
            if let Err(error) = finished
                && error.is_panic()
            {
                tracing::error!(event = "gateway_asset_download_failed", %error);
            }
        }
        tasks.spawn_on(task, runtime)
    }
}

fn row_mut<'a>(
    entries: &'a mut HashMap<AssetStateKey, ConversationAssets>,
    key: &AssetStateKey,
    id: u64,
) -> Option<&'a mut Row> {
    entries
        .get_mut(key)?
        .assets
        .iter_mut()
        .find(|row| row.asset.id == id)
}

impl AssetStore {
    #[cfg(test)]
    pub fn new(conversations: usize, idle_timeout: Duration) -> Self {
        Self::with_retention(
            conversations,
            idle_timeout,
            crate::config::DEFAULT_ASSET_RETENTION_BYTES,
        )
    }

    pub fn with_retention(conversations: usize, idle_timeout: Duration, budget: usize) -> Self {
        Self {
            conversations,
            idle_timeout,
            entries: Mutex::new(HashMap::new()),
            retention: Mutex::new(Retention::new(budget)),
            downloads: Downloads {
                permits: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
                tasks: Mutex::new(JoinSet::new()),
                attempts: AtomicU64::new(1),
            },
            next_one_shot_id: AtomicU64::new(1),
        }
    }

    pub async fn shutdown(&self) {
        let mut tasks = std::mem::take(&mut *self.downloads.tasks.lock());
        tasks.shutdown().await;
    }

    #[cfg(test)]
    pub fn assets_for(
        &self,
        conversation: &ConversationKey,
        arriving: Vec<PendingAsset>,
        now: Instant,
    ) -> Registered {
        self.assets_for_access(&AssetAccess::one_shot(conversation.clone()), arriving, now)
    }

    pub fn assets_for_access(
        &self,
        access: &AssetAccess,
        arriving: Vec<PendingAsset>,
        now: Instant,
    ) -> Registered {
        access
            .with_active(|state_key| {
                let mut entries = self.entries.lock();
                let mut retention = self.retention.lock();
                Self::expire(&mut entries, &mut retention, self.idle_timeout, now);
                let mut arrived = Vec::with_capacity(arriving.len());
                if !arriving.is_empty() {
                    let entry = entries
                        .entry(state_key.clone())
                        .or_insert_with(|| ConversationAssets::new(access.weak_fence(), now));
                    entry.touched = now;
                    for pending in arriving {
                        let asset = AssetRef {
                            id: access.allocate_id(&self.next_one_shot_id),
                            name: pending.name,
                            mime: pending.mime,
                            size: pending.size,
                            source: pending.source,
                            arrived: SystemTime::now(),
                            encoding: AssetEncoding::Identity,
                            sent: false,
                        };
                        arrived.push(asset.id);
                        entry.assets.push(Row {
                            asset,
                            state: Attachment::Registered,
                        });
                    }
                    Self::trim(entry, &mut retention, state_key);
                    Self::enforce_ceiling(&mut entries, &mut retention, self.conversations);
                }

                let inventory = entries.get_mut(state_key).map_or_else(Vec::new, |entry| {
                    entry.touched = now;
                    entry.assets.iter().map(|row| row.asset.clone()).collect()
                });
                Registered { inventory, arrived }
            })
            .unwrap_or_else(Registered::empty)
    }

    // Recalled ids are kept because replayed turns already name them as `chat-asset:N`.
    pub fn restore(
        &self,
        access: &AssetAccess,
        recalled: Vec<RecalledAsset>,
        next_id: u64,
        now: Instant,
    ) {
        if let Some(fence) = access.fence.as_ref() {
            fence.next_asset_id.fetch_max(next_id, Ordering::AcqRel);
        }
        if recalled.is_empty() {
            return;
        }
        access.with_active(|state_key| {
            let mut entries = self.entries.lock();
            let mut retention = self.retention.lock();
            Self::expire(&mut entries, &mut retention, self.idle_timeout, now);
            let entry = entries
                .entry(state_key.clone())
                .or_insert_with(|| ConversationAssets::new(access.weak_fence(), now));
            entry.touched = now;
            entry
                .assets
                .extend(recalled.into_iter().map(|recalled| Row {
                    asset: AssetRef {
                        id: recalled.id,
                        name: recalled.asset.name,
                        mime: recalled.asset.mime,
                        size: recalled.asset.size,
                        source: recalled.asset.source,
                        arrived: recalled.arrived,
                        encoding: AssetEncoding::Identity,
                        sent: false,
                    },
                    state: Attachment::Registered,
                }));
            entry.assets.sort_by_key(|row| row.asset.id);
            Self::trim(entry, &mut retention, state_key);
            Self::enforce_ceiling(&mut entries, &mut retention, self.conversations);
        });
    }

    pub fn inventory(&self, access: &AssetAccess) -> Vec<AssetRef> {
        self.get_inventory(access)
    }

    #[cfg(test)]
    pub fn get(&self, conversation: &ConversationKey, id: u64, now: Instant) -> Option<AssetRef> {
        self.get_access(&AssetAccess::one_shot(conversation.clone()), id, now)
    }

    pub fn get_access(&self, access: &AssetAccess, id: u64, now: Instant) -> Option<AssetRef> {
        access
            .with_active(|state_key| {
                let mut entries = self.entries.lock();
                Self::expire(
                    &mut entries,
                    &mut self.retention.lock(),
                    self.idle_timeout,
                    now,
                );
                let entry = entries.get_mut(state_key)?;
                entry.touched = now;
                entry
                    .assets
                    .iter()
                    .find(|row| row.asset.id == id)
                    .map(|row| row.asset.clone())
            })
            .flatten()
    }

    fn expire(
        entries: &mut HashMap<AssetStateKey, ConversationAssets>,
        retention: &mut Retention,
        idle_timeout: Duration,
        now: Instant,
    ) {
        let expired = entries.extract_if(|_, entry| {
            let active = entry
                .fence
                .as_ref()
                .is_none_or(|fence| fence.upgrade().is_some_and(|fence| fence.is_active()));
            !active || now.saturating_duration_since(entry.touched) >= idle_timeout
        });
        for (key, entry) in expired {
            retention.release_all(&key, entry.assets);
        }
    }

    fn enforce_ceiling(
        entries: &mut HashMap<AssetStateKey, ConversationAssets>,
        retention: &mut Retention,
        capacity: usize,
    ) {
        while entries.len() > capacity {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = entries.remove(&oldest) {
                retention.release_all(&oldest, entry.assets);
            }
        }
    }

    fn trim(entry: &mut ConversationAssets, retention: &mut Retention, key: &AssetStateKey) {
        let excess = entry
            .assets
            .len()
            .saturating_sub(MAX_ASSETS_PER_CONVERSATION);
        retention.release_all(key, entry.assets.drain(..excess));
    }
}

impl fmt::Debug for AssetStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.lock();
        formatter
            .debug_struct("AssetStore")
            .field("conversations", &entries.len())
            .field("capacity", &self.conversations)
            .field(
                "assets",
                &entries
                    .values()
                    .map(|entry| entry.assets.len())
                    .sum::<usize>(),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingAsset {
    pub name: String,
    pub mime: String,
    pub size: Option<u64>,
    pub source: Option<AssetSourceRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecalledAsset {
    pub id: u64,
    pub asset: PendingAsset,
    pub arrived: SystemTime,
}

const READABLE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

const READABLE_DOCUMENT_TYPES: [&str; 13] = [
    "application/pdf",
    "text/plain",
    "text/markdown",
    "text/csv",
    "text/html",
    "text/xml",
    "application/json",
    "application/xml",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/rtf",
];

pub(crate) fn is_readable(mime: &str) -> bool {
    is_image(mime) || READABLE_DOCUMENT_TYPES.contains(&mime)
}

pub(crate) fn is_image(mime: &str) -> bool {
    READABLE_IMAGE_TYPES.contains(&mime)
}

pub(crate) fn attached_marker(id: u64, name: &str) -> String {
    format!("\n[gateway: attached chat-asset:{id} — {name}]")
}

pub(crate) fn arrival_markers(registered: &Registered) -> String {
    registered
        .inventory
        .iter()
        .filter(|asset| registered.arrived.contains(&asset.id))
        .map(|asset| attached_marker(asset.id, &asset.name))
        .collect()
}

pub(crate) fn reference_note(registered: &Registered, images_supported: bool) -> Option<String> {
    if registered.inventory.is_empty() {
        return None;
    }
    let mut note = String::from("[gateway: files in this conversation");
    let mut any_fetchable = false;
    for asset in &registered.inventory {
        let size = asset
            .size
            .map_or_else(|| "size unknown".to_owned(), kibibytes);
        let name = &asset.name;
        let mime = &asset.mime;
        let arrived = if registered.arrived.contains(&asset.id) {
            " — attached to this message"
        } else {
            ""
        };
        if asset.is_fetchable(images_supported) {
            any_fetchable = true;
            note.push_str(&format!(
                "\n  chat-asset:{} — {name} ({mime}, {size}){arrived}",
                asset.id
            ));
        } else {
            let why = asset.unreadable_reason(images_supported);
            note.push_str(&format!("\n  {name} ({mime}, {size}) — {why}{arrived}"));
        }
    }
    if any_fetchable {
        note.push_str("\n  Call fetch_chat_asset with the number to look at one.");
    }
    note.push(']');
    Some(note)
}

impl AssetRef {
    pub fn is_fetchable(&self, images_supported: bool) -> bool {
        self.source.is_some()
            && is_readable(&self.mime)
            && (images_supported || !is_image(&self.mime))
    }

    pub fn unreadable_reason(&self, images_supported: bool) -> &'static str {
        if self.source.is_none() {
            "the gateway cannot see this file at all"
        } else if !is_readable(&self.mime) {
            "not a type the gateway can show you"
        } else if is_image(&self.mime) && !images_supported {
            "this agent's model cannot be shown images"
        } else {
            "unavailable"
        }
    }

    fn row(&self, bytes: Option<u64>) -> AssetRow {
        AssetRow {
            id: self.id,
            content_type: self.mime.clone(),
            encoding: self.encoding,
            bytes,
            origin: match &self.source {
                Some(AssetSourceRef::Generated { capability, .. }) => {
                    format!("provider:{capability}")
                }
                Some(
                    AssetSourceRef::Slack { .. }
                    | AssetSourceRef::Discord { .. }
                    | AssetSourceRef::WhatsApp { .. }
                    | AssetSourceRef::Telegram { .. },
                )
                | None => "chat".to_owned(),
            },
            sent: self.sent,
        }
    }
}

fn kibibytes(size: u64) -> String {
    if size < 1024 {
        return format!("{size} B");
    }
    let kib = size / 1024;
    if kib < 1024 {
        format!("{kib} KB")
    } else {
        format!("{}.{} MB", kib / 1024, (kib % 1024) * 10 / 1024)
    }
}

pub(crate) struct Registered {
    pub inventory: Vec<AssetRef>,
    pub arrived: Vec<u64>,
}

impl Registered {
    fn empty() -> Self {
        Self {
            inventory: Vec::new(),
            arrived: Vec::new(),
        }
    }
}

const MAX_ASSET_BYTES: u64 = dekopon_model::asset::MAX_ATTACHMENT_BYTES as u64;

const ASSET_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct SessionAssets {
    store: Arc<AssetStore>,
    access: AssetAccess,
    fetcher: Option<Arc<dyn AssetFetcher>>,
    runtime: Handle,
    images_supported: bool,
}

impl SessionAssets {
    pub fn new(
        store: Arc<AssetStore>,
        access: AssetAccess,
        fetcher: Option<Arc<dyn AssetFetcher>>,
        runtime: Handle,
        images_supported: bool,
    ) -> Self {
        Self {
            store,
            access,
            fetcher,
            runtime,
            images_supported,
        }
    }
}

impl AssetSource for SessionAssets {
    fn offers_fetch(&self) -> bool {
        self.access.is_active() && self.store.retains()
    }

    fn fetch(&self, id: u64) -> Result<FetchedAsset, String> {
        let asset = self.asset(id).map_err(|failure| failure.for_model(id))?;
        if !asset.is_fetchable(self.images_supported) {
            return Err(AssetFailure::Unreadable {
                reason: asset.unreadable_reason(self.images_supported),
            }
            .for_model(id));
        }
        let data = self
            .runtime
            .block_on(self.resolve(&asset, Recency::Keep))
            .map_err(|failure| failure.for_model(id))?;
        let decoded = GeneratedImage::new(data, asset.mime.clone(), asset.encoding)
            .decoded_len()
            .map_err(|error| AssetFailure::Storage(error).for_model(id))?;
        let reference = BlobReference::new(
            Arc::new(ScopedBlob {
                store: Arc::downgrade(&self.store),
                access: self.access.clone(),
                id,
            }),
            decoded,
            id,
        );
        Ok(FetchedAsset {
            name: asset.name,
            mime: asset.mime,
            data: reference,
        })
    }
}

impl ChatAssetSource for SessionAssets {
    fn resolve_input(&self, id: u64) -> ResolveFuture<'_> {
        Box::pin(async move {
            let asset = self.asset(id).map_err(AssetFailure::for_capability)?;
            let blob = self
                .resolve(&asset, Recency::Touch)
                .await
                .map_err(AssetFailure::for_capability)?;
            Ok(ResolvedInput {
                row: asset.row(Some(blob.len() as u64)),
                blob,
            })
        })
    }

    fn rows(&self) -> Vec<AssetRow> {
        self.store
            .get_inventory(&self.access)
            .iter()
            .map(|asset| asset.row(asset.size))
            .collect()
    }
}

enum Located {
    Resident(DiskBlob),
    Remote {
        source: AssetSourceRef,
        fetcher: Arc<dyn AssetFetcher>,
    },
}

#[derive(Clone, Copy)]
enum Recency {
    Keep,
    Touch,
}

impl SessionAssets {
    fn asset(&self, id: u64) -> Result<AssetRef, AssetFailure> {
        self.store
            .get_access(&self.access, id, Instant::now())
            .ok_or_else(|| match self.store.pin(&self.access, id, Recency::Keep) {
                Err(BlobError::Unknown) | Ok(_) => AssetFailure::Unknown,
                Err(error) => AssetFailure::Storage(error),
            })
    }

    fn locate(&self, asset: &AssetRef, recency: Recency) -> Result<Located, AssetFailure> {
        if let Some(data) = self
            .store
            .pin(&self.access, asset.id, recency)
            .map_err(AssetFailure::Storage)?
        {
            return Ok(Located::Resident(data));
        }
        if let Some(size) = asset.size
            && size > MAX_ASSET_BYTES
        {
            return Err(AssetFailure::TooLarge { size });
        }
        self.store
            .check_size(asset.id, asset.size.unwrap_or_default() as usize)
            .map_err(AssetFailure::Storage)?;
        let (Some(fetcher), Some(source)) = (self.fetcher.as_ref(), asset.source.as_ref()) else {
            return Err(AssetFailure::Unavailable);
        };
        Ok(Located::Remote {
            source: source.clone(),
            fetcher: Arc::clone(fetcher),
        })
    }

    async fn resolve(&self, asset: &AssetRef, recency: Recency) -> Fetched {
        let (source, fetcher) = match self.locate(asset, recency)? {
            Located::Resident(data) => return Ok(data),
            Located::Remote { source, fetcher } => (source, fetcher),
        };
        match AssetStore::join(
            &self.store,
            &self.access,
            asset.id,
            source,
            fetcher,
            &self.runtime,
        )? {
            Download::Ready(data) => Ok(data),
            Download::Pending(waiter) => waiter.wait().await,
        }
    }
}

enum Download {
    Ready(DiskBlob),
    Pending(Waiter),
}

#[derive(Clone)]
struct Ticket {
    store: Weak<AssetStore>,
    access: AssetAccess,
    id: u64,
    attempt: u64,
}

/// Dropping the task's attempt, by completion, abort or panic, is how the store observes that the
/// download ended.
struct Attempt(Ticket);

impl Attempt {
    async fn download(
        &self,
        source: AssetSourceRef,
        fetcher: Arc<dyn AssetFetcher>,
        permits: Arc<Semaphore>,
    ) -> Completion {
        let _permit = permits
            .acquire()
            .await
            .map_err(|_closed| AssetFailure::Unavailable)?;
        let data =
            tokio::time::timeout(ASSET_FETCH_TIMEOUT, fetcher.fetch(&source, MAX_ASSET_BYTES))
                .await
                .map_err(|_elapsed| AssetFailure::TimedOut)?
                .map_err(|error| AssetFailure::Transport {
                    // Only the transport's error category goes into the prompt, never its message,
                    // since a transport error can carry arbitrary service text.
                    category: error.category(),
                })?;
        let ticket = self.0.clone();
        tokio::task::spawn_blocking(move || {
            ticket
                .store
                .upgrade()
                .ok_or(AssetFailure::Unknown)?
                .complete(&ticket, &data)
        })
        .await
        .map_err(|_join| AssetFailure::Unavailable)?
    }

    fn reset(&self) {
        if let Some(store) = self.0.store.upgrade() {
            store.reset(&self.0);
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.reset();
    }
}

struct Waiter {
    ticket: Ticket,
    outcome: watch::Receiver<Option<Completion>>,
}

impl Waiter {
    async fn wait(mut self) -> Fetched {
        let completion = match self.outcome.wait_for(Option::is_some).await {
            Ok(completion) => completion.unwrap_or(Err(AssetFailure::Unavailable)),
            Err(_closed) => Err(AssetFailure::Unavailable),
        };
        completion?;
        let store = self.ticket.store.upgrade().ok_or(AssetFailure::Unknown)?;
        match store.pin(&self.ticket.access, self.ticket.id, Recency::Keep) {
            Ok(Some(data)) => Ok(data),
            Ok(None) | Err(BlobError::Reclaimed) => {
                Err(AssetFailure::Storage(BlobError::Reclaimed))
            }
            Err(error) => Err(AssetFailure::Storage(error)),
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        if let Some(store) = self.ticket.store.upgrade() {
            store.release_interest(&self.ticket);
        }
    }
}

impl AssetStore {
    fn join(
        self: &Arc<Self>,
        access: &AssetAccess,
        id: u64,
        source: AssetSourceRef,
        fetcher: Arc<dyn AssetFetcher>,
        runtime: &Handle,
    ) -> Result<Download, AssetFailure> {
        access
            .with_active(|key| {
                let mut entries = self.entries.lock();
                let row = row_mut(&mut entries, key, id).ok_or(AssetFailure::Unknown)?;
                let ticket = |attempt| Ticket {
                    store: Arc::downgrade(self),
                    access: access.clone(),
                    id,
                    attempt,
                };
                match &mut row.state {
                    Attachment::Resident(resident) => {
                        return Ok(Download::Ready(resident.data.clone()));
                    }
                    Attachment::Evicted => return Err(AssetFailure::Storage(BlobError::Reclaimed)),
                    Attachment::Downloading(downloading) if downloading.interests > 0 => {
                        downloading.interests += 1;
                        return Ok(Download::Pending(Waiter {
                            ticket: ticket(downloading.attempt),
                            outcome: downloading.outcome.clone(),
                        }));
                    }
                    Attachment::Downloading(_) | Attachment::Registered => (),
                }
                let attempt = self.downloads.attempts.fetch_add(1, Ordering::Relaxed);
                let (sender, outcome) = watch::channel(None);
                let attempt_ticket = ticket(attempt);
                let permits = Arc::clone(&self.downloads.permits);
                let task = self.downloads.spawn(
                    async move {
                        // Built on first poll: a runtime that drops the unpolled future inside
                        // `spawn_on` while `entries` is locked must not run `Attempt::drop`.
                        let task = Attempt(attempt_ticket);
                        let completion = task.download(source, fetcher, permits).await;
                        // The entry leaves Downloading before waiters wake, so a retry never joins
                        // a finished attempt.
                        task.reset();
                        sender.send_replace(Some(completion));
                    },
                    runtime,
                );
                row.state = Attachment::Downloading(Downloading {
                    attempt,
                    task,
                    interests: 1,
                    outcome: outcome.clone(),
                });
                Ok(Download::Pending(Waiter {
                    ticket: ticket(attempt),
                    outcome,
                }))
            })
            .unwrap_or(Err(AssetFailure::Storage(BlobError::Unauthorized)))
    }

    fn release_interest(&self, ticket: &Ticket) {
        ticket.access.with_active(|key| {
            let mut entries = self.entries.lock();
            let Some(row) = row_mut(&mut entries, key, ticket.id) else {
                return;
            };
            match &mut row.state {
                Attachment::Downloading(downloading) => {
                    if downloading.attempt == ticket.attempt {
                        downloading.interests -= 1;
                        if downloading.interests == 0 {
                            downloading.task.abort();
                        }
                    }
                }
                Attachment::Registered | Attachment::Resident(_) | Attachment::Evicted => (),
            }
        });
    }

    fn reset(&self, ticket: &Ticket) {
        ticket.access.with_active(|key| {
            let mut entries = self.entries.lock();
            if let Some(row) = row_mut(&mut entries, key, ticket.id)
                && row.state.is_attempt(ticket.attempt)
            {
                row.state = Attachment::Registered;
            }
        });
    }

    fn complete(&self, ticket: &Ticket, bytes: &[u8]) -> Completion {
        ticket
            .access
            .with_active(|key| {
                let mut entries = self.entries.lock();
                let mut retention = self.retention.lock();
                Self::expire(
                    &mut entries,
                    &mut retention,
                    self.idle_timeout,
                    Instant::now(),
                );
                let row = row_mut(&mut entries, key, ticket.id).ok_or(AssetFailure::Unknown)?;
                if !row.state.is_attempt(ticket.attempt) {
                    return Err(AssetFailure::Unavailable);
                }
                Self::admit_locked(&mut entries, &mut retention, key, ticket.id, bytes)
                    .map(drop)
                    .map_err(AssetFailure::Storage)
            })
            .unwrap_or(Err(AssetFailure::Unknown))
    }
}

impl Attachment {
    const fn is_attempt(&self, attempt: u64) -> bool {
        match self {
            Self::Downloading(downloading) => downloading.attempt == attempt,
            Self::Registered | Self::Resident(_) | Self::Evicted => false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum AssetFailure {
    Storage(dekopon_model::asset::BlobError),
    Unknown,
    Unreadable { reason: &'static str },
    TooLarge { size: u64 },
    Unavailable,
    TimedOut,
    Transport { category: &'static str },
}

impl AssetFailure {
    fn for_model(self, id: u64) -> String {
        match self {
            Self::Storage(error) => format!(
                "chat-asset:{id} is unavailable: {error}. No automatic refetch or fallback was performed."
            ),
            Self::Unknown => format!(
                "There is no chat-asset:{id} in this conversation. The reference lines in the messages above name the ones there are."
            ),
            Self::Unreadable { reason } => {
                format!("chat-asset:{id} cannot be opened: {reason}.")
            }
            Self::TooLarge { size } => format!(
                "chat-asset:{id} is {} which is over the {} the gateway will read.",
                kibibytes(size),
                kibibytes(MAX_ASSET_BYTES)
            ),
            Self::Unavailable => format!("chat-asset:{id} cannot be opened."),
            Self::TimedOut => format!("chat-asset:{id} could not be read (fetch-timeout)."),
            Self::Transport { category } => {
                format!("chat-asset:{id} could not be read ({category}).")
            }
        }
    }

    const fn for_capability(self) -> ChatAssetRefusal {
        match self {
            Self::Unknown | Self::Storage(BlobError::Unknown) => ChatAssetRefusal::UnknownAsset,
            Self::Unreadable { .. } => ChatAssetRefusal::UnsupportedMedia,
            Self::Storage(BlobError::Reclaimed) => ChatAssetRefusal::Reclaimed,
            Self::Storage(BlobError::Unauthorized) => ChatAssetRefusal::Unauthorized,
            Self::Storage(_)
            | Self::TooLarge { .. }
            | Self::Transport { .. }
            | Self::TimedOut
            | Self::Unavailable => ChatAssetRefusal::Unavailable,
        }
    }
}

// A consumer's DiskBlob clone is only a temporary pin; the AssetStore alone owns residency and its
// actual cleanup.
const MAX_RELEASE_TOMBSTONES: usize = 1024;
type RetentionKey = (AssetStateKey, u64);
struct Resident {
    data: DiskBlob,
    used: u64,
}
struct Retention {
    budget: usize,
    bytes: usize,
    clock: u64,
    released: VecDeque<RetentionKey>,
    orphans: Vec<DiskBlob>,
}
impl Retention {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            bytes: 0,
            clock: 0,
            released: VecDeque::new(),
            orphans: Vec::new(),
        }
    }
    fn miss(&self, id: u64, bytes: usize, reason: &'static str) {
        tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.retention_miss",
            asset.id = id, asset.bytes = bytes, asset.budget = self.budget, reason },
            "chat asset retention refused");
    }
    fn check_size(&self, id: u64, bytes: usize) -> Result<(), BlobError> {
        if self.budget == 0 {
            self.miss(id, bytes, "disabled");
            return Err(BlobError::Disabled);
        }
        if bytes.max(1) > self.budget || bytes > dekopon_model::asset::MAX_STORED_ATTACHMENT_BYTES {
            self.miss(id, bytes, "oversized");
            return Err(BlobError::TooLarge);
        }
        Ok(())
    }
    fn charge(&mut self, data: DiskBlob) -> Resident {
        self.bytes += data.len().max(1);
        self.clock += 1;
        Resident {
            data,
            used: self.clock,
        }
    }
    fn tombstone(&mut self, key: RetentionKey) {
        self.released.push_back(key);
        while self.released.len() > MAX_RELEASE_TOMBSTONES {
            self.released.pop_front();
        }
    }
    fn evict(&mut self, key: RetentionKey, row: &mut Row) -> Result<(), BlobError> {
        match std::mem::replace(&mut row.state, Attachment::Evicted) {
            Attachment::Resident(resident) => {
                if let Err(error) = resident.data.reclaim() {
                    row.state = Attachment::Resident(resident);
                    return Err(error);
                }
                self.bytes -= resident.data.len().max(1);
                self.tombstone(key);
                Ok(())
            }
            other @ (Attachment::Registered | Attachment::Downloading(_) | Attachment::Evicted) => {
                row.state = other;
                Ok(())
            }
        }
    }
    fn release_all(&mut self, key: &AssetStateKey, rows: impl IntoIterator<Item = Row>) {
        for row in rows {
            match row.state {
                Attachment::Resident(resident) => {
                    self.tombstone((key.clone(), row.asset.id));
                    self.retire(row.asset.id, resident.data);
                }
                Attachment::Downloading(downloading) => downloading.task.abort(),
                Attachment::Registered | Attachment::Evicted => (),
            }
        }
    }
    fn retire(&mut self, id: u64, data: DiskBlob) {
        match data.reclaim() {
            Ok(()) => self.bytes -= data.len().max(1),
            Err(BlobError::Capacity) => self.orphans.push(data),
            Err(error) => {
                tracing::warn!(asset.id = id, %error, "could not reclaim evicted asset");
                self.orphans.push(data);
            }
        }
    }
    fn sweep(&mut self) {
        for data in std::mem::take(&mut self.orphans) {
            match data.reclaim() {
                Ok(()) => self.bytes -= data.len().max(1),
                Err(_) => self.orphans.push(data),
            }
        }
    }
}

impl AssetStore {
    fn get_inventory(&self, access: &AssetAccess) -> Vec<AssetRef> {
        self.assets_for_access(access, Vec::new(), Instant::now())
            .inventory
    }
    fn check_size(&self, id: u64, bytes: usize) -> Result<(), BlobError> {
        self.retention.lock().check_size(id, bytes)
    }
    fn retains(&self) -> bool {
        self.retention.lock().budget > 0
    }
    fn pin(
        &self,
        access: &AssetAccess,
        id: u64,
        recency: Recency,
    ) -> Result<Option<DiskBlob>, BlobError> {
        access
            .with_active(|key| {
                let mut entries = self.entries.lock();
                let mut retention = self.retention.lock();
                Self::expire(
                    &mut entries,
                    &mut retention,
                    self.idle_timeout,
                    Instant::now(),
                );
                let Some(row) = row_mut(&mut entries, key, id) else {
                    let reclaimed = retention.released.contains(&(key.clone(), id));
                    retention.miss(id, 0, if reclaimed { "reclaimed" } else { "unknown" });
                    return Err(if reclaimed {
                        BlobError::Reclaimed
                    } else {
                        BlobError::Unknown
                    });
                };
                match &mut row.state {
                    Attachment::Resident(resident) => {
                        match recency {
                            Recency::Touch => {
                                retention.clock += 1;
                                resident.used = retention.clock;
                            }
                            Recency::Keep => (),
                        }
                        Ok(Some(resident.data.clone()))
                    }
                    Attachment::Registered | Attachment::Downloading(_) => Ok(None),
                    Attachment::Evicted => {
                        retention.miss(
                            id,
                            row.asset.size.unwrap_or_default() as usize,
                            "reclaimed",
                        );
                        Err(BlobError::Reclaimed)
                    }
                }
            })
            .unwrap_or_else(|| {
                self.retention.lock().miss(id, 0, "unauthorized");
                Err(BlobError::Unauthorized)
            })
    }
    #[cfg(test)]
    fn admit(&self, access: &AssetAccess, id: u64, bytes: &[u8]) -> Result<DiskBlob, BlobError> {
        access
            .with_active(|key| {
                let mut entries = self.entries.lock();
                let mut retention = self.retention.lock();
                Self::expire(
                    &mut entries,
                    &mut retention,
                    self.idle_timeout,
                    Instant::now(),
                );
                Self::admit_locked(&mut entries, &mut retention, key, id, bytes)
            })
            .unwrap_or(Err(BlobError::Unauthorized))
    }
    fn admit_locked(
        entries: &mut HashMap<AssetStateKey, ConversationAssets>,
        retention: &mut Retention,
        key: &AssetStateKey,
        id: u64,
        bytes: &[u8],
    ) -> Result<DiskBlob, BlobError> {
        let row = row_mut(entries, key, id).ok_or(BlobError::Unauthorized)?;
        match &mut row.state {
            Attachment::Resident(resident) => {
                retention.clock += 1;
                resident.used = retention.clock;
                return Ok(resident.data.clone());
            }
            Attachment::Registered | Attachment::Downloading(_) | Attachment::Evicted => (),
        }
        // The completed download is recorded before any fallible size, cleanup, or admission
        // check, since a successfully downloaded input must never silently be fetched twice.
        row.asset.size = Some(bytes.len() as u64);
        row.state = Attachment::Evicted;
        if bytes.len() > dekopon_model::asset::MAX_ATTACHMENT_BYTES {
            return Err(BlobError::TooLarge);
        }
        Self::reserve(entries, retention, id, bytes.len())?;
        // Capacity is reserved under this mutex before the one file creation, so no staging file
        // can escape the configured budget, and a failed construction never increments accounting.
        let data = DiskBlob::from_bytes(bytes).inspect_err(|_error| {
            retention.miss(id, bytes.len(), "storage");
        })?;
        let resident = retention.charge(data.clone());
        if let Some(row) = row_mut(entries, key, id) {
            row.state = Attachment::Resident(resident);
        }
        Ok(data)
    }
    fn reserve(
        entries: &mut HashMap<AssetStateKey, ConversationAssets>,
        retention: &mut Retention,
        id: u64,
        len: usize,
    ) -> Result<(), BlobError> {
        retention.check_size(id, len)?;
        retention.sweep();
        let charge = len.max(1);
        let needed = (retention.bytes + charge).saturating_sub(retention.budget);
        if needed == 0 {
            return Ok(());
        }
        let mut candidates: Vec<_> = entries
            .iter()
            .flat_map(|(key, entry)| {
                entry.assets.iter().filter_map(move |row| match &row.state {
                    Attachment::Resident(resident) => (!resident.data.is_pinned()).then(|| {
                        (
                            (key.clone(), row.asset.id),
                            resident.used,
                            resident.data.len().max(1),
                        )
                    }),
                    Attachment::Registered | Attachment::Downloading(_) | Attachment::Evicted => {
                        None
                    }
                })
            })
            .collect();
        candidates.sort_by_key(|(_, used, _)| *used);
        if candidates.iter().map(|(_, _, bytes)| *bytes).sum::<usize>() < needed {
            retention.miss(id, len, "all-pinned");
            return Err(BlobError::Capacity);
        }
        for (old, _, _) in candidates {
            if retention.bytes + charge <= retention.budget {
                break;
            }
            if let Some(row) = row_mut(entries, &old.0, old.1) {
                retention.evict(old, row)?;
            }
        }
        Ok(())
    }
}
struct ScopedBlob {
    store: Weak<AssetStore>,
    access: AssetAccess,
    id: u64,
}
impl BlobSource for ScopedBlob {
    fn pin(&self) -> Result<DiskBlob, BlobError> {
        let store = self.store.upgrade().ok_or(BlobError::Reclaimed)?;
        match store.pin(&self.access, self.id, Recency::Touch) {
            Ok(Some(data)) => Ok(data),
            Ok(None) | Err(BlobError::Unknown) => Err(BlobError::Reclaimed),
            Err(error) => Err(error),
        }
    }
    fn read(&self) -> Result<Vec<u8>, BlobError> {
        let data = self.pin()?;
        let store = self.store.upgrade().ok_or(BlobError::Reclaimed)?;
        let asset = store
            .get_access(&self.access, self.id, Instant::now())
            .ok_or(BlobError::Unauthorized)?;
        GeneratedImage::new(data, asset.mime, asset.encoding).bytes()
    }
}

impl dekopon_agent::attachment::GeneratedAssetStore for SessionAssets {
    fn register(
        &self,
        descriptor: OwnedFd,
        metadata: &NewAsset,
        capability: &str,
        invocation: &str,
    ) -> Result<u64, BlobError> {
        let len = usize::try_from(metadata.bytes).map_err(|_overflow| BlobError::TooLarge)?;
        self.store.check_size(0, len)?;
        let data = DiskBlob::from_descriptor(descriptor, len)?;
        let decoded = GeneratedImage::new(
            data.clone(),
            metadata.content_type.clone(),
            metadata.encoding,
        )
        .decoded_len()?;
        if decoded > dekopon_model::asset::MAX_ATTACHMENT_BYTES {
            return Err(BlobError::TooLarge);
        }
        // Only a decoded prefix is sniffed for the image signature; the declared media type stays
        // authoritative even when it does not match what was sniffed.
        let detected = sniff(&data, metadata.encoding)?;
        self.access.with_active(|key| {
            let mut entries = self.store.entries.lock();
            let mut retention = self.store.retention.lock();
            let id = self.access.allocate_id(&self.store.next_one_shot_id);
            AssetStore::reserve(&mut entries, &mut retention, id, len)?;
            let resident = retention.charge(data);
            let now = Instant::now();
            let entry = entries.entry(key.clone()).or_insert_with(|| ConversationAssets::new(self.access.weak_fence(), now));
            entry.touched = now;
            entry.assets.push(Row {
                asset: AssetRef { id, name: format!("asset-{id}"), mime: metadata.content_type.clone(), size: Some(metadata.bytes),
                    source: Some(AssetSourceRef::Generated { capability: capability.to_owned(), invocation: invocation.to_owned() }),
                    arrived: SystemTime::now(), encoding: metadata.encoding, sent: false,
                },
                state: Attachment::Resident(resident),
            });
            AssetStore::trim(entry, &mut retention, key);
            AssetStore::enforce_ceiling(&mut entries, &mut retention, self.store.conversations);
            if let Some(detected) = detected && detected != metadata.content_type {
                let label: String = metadata.content_type.chars().filter(|c| !c.is_control()).take(128).collect();
                tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.content_type_mismatch", asset.id = id, asset.content_type = label, asset.detected_type = detected, asset.bytes = metadata.bytes, asset.sha256 = metadata.sha256 }, "declared asset label differs from decoded prefix");
            }
            Ok(id)
        }).unwrap_or(Err(BlobError::Unauthorized))
    }
    fn remove(&self, id: u64) -> Result<(), BlobError> {
        self.access
            .with_active(|key| {
                let mut entries = self.store.entries.lock();
                let entry = entries.get_mut(key).ok_or(BlobError::Unknown)?;
                let index = entry
                    .assets
                    .iter()
                    .position(|row| row.asset.id == id)
                    .ok_or(BlobError::Unknown)?;
                if entry.assets[index].asset.sent {
                    return Err(BlobError::Unauthorized);
                }
                let mut retention = self.store.retention.lock();
                retention.evict((key.clone(), id), &mut entry.assets[index])?;
                retention.release_all(key, [entry.assets.remove(index)]);
                Ok(())
            })
            .unwrap_or(Err(BlobError::Unauthorized))
    }
    fn send(&self, id: u64) -> Result<Option<GeneratedImage>, BlobError> {
        self.access
            .with_active(|key| {
                let mut entries = self.store.entries.lock();
                let row = row_mut(&mut entries, key, id).ok_or(BlobError::Unknown)?;
                if row.asset.sent {
                    return Ok(None);
                }
                // Mark an asset sent once, even if its retained file becomes unavailable; never
                // retry the send implicitly.
                row.asset.sent = true;
                let data = match &row.state {
                    Attachment::Resident(resident) => resident.data.clone(),
                    Attachment::Registered | Attachment::Downloading(_) | Attachment::Evicted => {
                        return Err(BlobError::Reclaimed);
                    }
                };
                Ok(Some(GeneratedImage::new(
                    data,
                    row.asset.mime.clone(),
                    row.asset.encoding,
                )))
            })
            .unwrap_or(Err(BlobError::Unauthorized))
    }
    fn delivery_failed(&self) {
        self.access.with_active(|key| {
            if let Some(entry) = self.store.entries.lock().get_mut(key) {
                entry.delivery_failed = true;
            }
        });
    }
}
impl AssetStore {
    pub fn take_delivery_notice(&self, access: &AssetAccess) -> Option<&'static str> {
        access.with_active(|key| {
            let mut entries = self.entries.lock();
            let entry = entries.get_mut(key)?;
            std::mem::take(&mut entry.delivery_failed).then_some("[gateway: a previous asset send did not complete. Sent flags remain set; no automatic retry was made.]")
        }).flatten()
    }
}

fn sniff(blob: &DiskBlob, encoding: AssetEncoding) -> Result<Option<&'static str>, BlobError> {
    let mut stored = [0; 16];
    let count = blob.len().min(stored.len());
    blob.read_exact_at(&mut stored[..count], 0)?;
    let mut decoded = [0; 12];
    let count = match encoding {
        AssetEncoding::Identity => {
            let n = count.min(decoded.len());
            decoded[..n].copy_from_slice(&stored[..n]);
            n
        }
        AssetEncoding::Base64 => {
            let start = Instant::now();
            let span = tracing::info_span!(
                "asset.decode",
                bytes = count,
                duration_us = tracing::field::Empty
            );
            let result = span.in_scope(|| {
                let mut reader = dekopon_core::base64::DecoderReader::new(
                    &stored[..count],
                    &dekopon_core::base64::STANDARD,
                );
                let mut n = 0;
                while n < decoded.len() {
                    let read = reader.read(&mut decoded[n..])?;
                    if read == 0 {
                        break;
                    }
                    n += read;
                }
                Ok::<_, BlobError>(n)
            });
            span.record("duration_us", start.elapsed().as_micros() as u64);
            result?
        }
    };
    let prefix = &decoded[..count];
    Ok(if prefix.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if prefix.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if prefix.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else {
        None
    })
}

#[cfg(test)]
#[path = "tests/asset_resolve.rs"]
mod resolve_tests;

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn row_limit_matches_the_protocol_contract() {
        assert_eq!(
            MAX_ASSETS_PER_CONVERSATION,
            dekopon_broker_protocol::MAX_ASSET_ROWS
        );
    }

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use dekopon_agent::attachment::{ChatAssetInputs, GeneratedAssetStore, ReplyAttachments};
    use serde_json::json;

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
    fn register(store: &AssetStore, access: &AssetAccess) -> u64 {
        store
            .assets_for_access(
                access,
                vec![PendingAsset {
                    name: "input.png".into(),
                    mime: "image/png".into(),
                    size: Some(3),
                    source: Some(AssetSourceRef::Telegram {
                        file_id: "file".into(),
                    }),
                }],
                Instant::now(),
            )
            .arrived[0]
    }
    fn input(session: &SessionAssets, id: u64) -> Result<DiskBlob, ChatAssetRefusal> {
        Handle::current()
            .block_on(session.resolve_input(id))
            .map(|resolved| resolved.blob)
    }
    fn residents(store: &AssetStore) -> usize {
        let entries = store.entries.lock();
        let resident = entries
            .values()
            .flat_map(|entry| &entry.assets)
            .filter(|row| match row.state {
                Attachment::Resident(_) => true,
                Attachment::Registered | Attachment::Downloading(_) | Attachment::Evicted => false,
            })
            .count();
        resident + store.retention.lock().orphans.len()
    }
    fn reclaim(store: &AssetStore, access: &AssetAccess, id: u64) {
        let mut entries = store.entries.lock();
        let row = row_mut(&mut entries, &access.key, id).unwrap();
        store
            .retention
            .lock()
            .evict((access.key.clone(), id), row)
            .unwrap();
    }
    fn store(budget: usize) -> Arc<AssetStore> {
        Arc::new(AssetStore::with_retention(
            8,
            Duration::from_secs(600),
            budget,
        ))
    }

    struct OneByteFetcher;

    impl AssetFetcher for OneByteFetcher {
        fn fetch(
            &self,
            _: &AssetSourceRef,
            _: u64,
        ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, crate::transport::TransportError>>
        {
            Box::pin(async { Ok(vec![9]) })
        }
    }

    pub(super) struct FetchScript {
        pub(super) calls: std::sync::atomic::AtomicUsize,
        pub(super) last: Mutex<Vec<dekopon_model::model::ModelMessage>>,
    }

    impl dekopon_model::model::ChatModel for FetchScript {
        fn complete(
            &self,
            messages: &[dekopon_model::model::ModelMessage],
            _tools: &[dekopon_model::model::ModelTool],
            _options: &dekopon_model::model::CompletionOptions,
            _on_event: &mut (
                     dyn FnMut(dekopon_model::TurnEvent) -> std::ops::ControlFlow<()> + Send
                 ),
        ) -> Result<dekopon_model::model::AssistantTurn, dekopon_model::error::InferenceError>
        {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if call == 9 {
                *self.last.lock() = messages.to_vec();
                Ok(dekopon_model::model::AssistantTurn::new(
                    Some("seen".into()),
                    vec![],
                    None,
                ))
            } else {
                Ok(dekopon_model::model::AssistantTurn::new(
                    None,
                    vec![dekopon_model::model::ModelToolCall {
                        id: format!("fetch-{call}").into(),
                        kind: "function".into(),
                        function: dekopon_model::model::ModelFunctionCall {
                            name: "fetch_chat_asset".into(),
                            arguments: json!({"id": call + 1}).to_string(),
                        },
                    }],
                    None,
                ))
            }
        }
    }

    pub(super) struct NoScripts;

    impl dekopon_agent::prompt::ScriptRuntime for NoScripts {
        fn run_script(&self, _: &str) -> dekopon_shell::ScriptOutcome {
            panic!("this model only fetches chat assets")
        }
        fn capability_calls_used(&self) -> u32 {
            0
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_crossing_fetch_preserves_the_real_store_images_at_both_retention_budgets() {
        use dekopon_agent::prompt::{History, PromptLimits, SessionInputs, run_prompt_session};
        use dekopon_model::model::ContentPart;
        use tracing_subscriber::prelude::*;

        for budget in [40 * 1024 * 1024, 64 * 1024 * 1024] {
            let store = store(budget);
            let access = access("crossing");
            let ids: Vec<_> = (1..=8).map(|_| register(&store, &access)).collect();
            for (index, id) in ids.iter().enumerate() {
                drop(
                    store
                        .admit(&access, *id, &vec![(index + 1) as u8; 5 * 1024 * 1024])
                        .unwrap(),
                );
            }
            let crossing_id = store
                .assets_for_access(
                    &access,
                    vec![PendingAsset {
                        name: "ninth.png".into(),
                        mime: "image/png".into(),
                        size: Some(1),
                        source: Some(AssetSourceRef::Telegram {
                            file_id: "ninth".into(),
                        }),
                    }],
                    Instant::now(),
                )
                .arrived[0];
            assert_eq!(crossing_id, 9);
            let session = SessionAssets::new(
                store,
                access,
                Some(Arc::new(OneByteFetcher)),
                Handle::current(),
                true,
            );
            tokio::task::spawn_blocking(move || {
                let capture = dekopon_test_support::CaptureLayer::workspace();
                let _guard = tracing_subscriber::registry()
                    .with(capture.clone())
                    .set_default();
                let model = FetchScript {
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    last: Mutex::new(vec![]),
                };
                run_prompt_session(
                    &model,
                    &NoScripts,
                    SessionInputs::new(
                        "show each photo",
                        PromptLimits {
                            max_steps: 11,
                            max_capability_calls: 0,
                        },
                    )
                    .with_assets(&session),
                    &mut History::default(),
                )
                .unwrap();
                let last = model.last.lock();
                let images: Vec<_> = last
                    .iter()
                    .filter_map(|message| message.parts())
                    .flatten()
                    .filter_map(|part| match part {
                        ContentPart::Image { data, .. } => {
                            Some(data.read().expect("previous image remains resident"))
                        }
                        ContentPart::Text(_) | ContentPart::File { .. } => None,
                    })
                    .collect();
                assert_eq!(images.len(), 8);
                assert!(
                    !last
                        .iter()
                        .filter_map(|message| message.content())
                        .any(|text| text.contains("was released from disk retention"))
                );
                for (index, image) in images.iter().enumerate() {
                    assert_eq!(image.len(), 5 * 1024 * 1024);
                    assert!(image.iter().all(|byte| *byte == (index + 1) as u8));
                }
                let refusal = last
                    .iter()
                    .filter(|message| message.role() == "tool")
                    .filter_map(|message| message.content())
                    .find(|text| text.contains("chat-asset:9"))
                    .expect("crossing ID appears in the tool response");
                assert!(
                    refusal.contains(if budget == 40 * 1024 * 1024 {
                        "unavailable"
                    } else {
                        "will not be shown"
                    }),
                    "{refusal}"
                );
                assert_eq!(
                    capture
                        .events_where(
                            "dekopon_agent::audit",
                            &[("audit.event", "agent.asset.refused"), ("asset.id", "9")]
                        )
                        .len(),
                    1
                );
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_text_fetch_cannot_cross_the_model_request_byte_limit() {
        use dekopon_agent::prompt::{History, PromptLimits, SessionInputs, run_prompt_session};
        use tracing_subscriber::prelude::*;

        let store = store(64 * 1024 * 1024);
        let access = access("text-crossing");
        let ids: Vec<_> = (1..=8).map(|_| register(&store, &access)).collect();
        for (index, id) in ids.iter().enumerate() {
            drop(
                store
                    .admit(&access, *id, &vec![(index + 1) as u8; 5 * 1024 * 1024])
                    .unwrap(),
            );
        }
        let text_id = store
            .assets_for_access(
                &access,
                vec![PendingAsset {
                    name: "note.txt".into(),
                    mime: "text/plain".into(),
                    size: Some(1),
                    source: Some(AssetSourceRef::Telegram {
                        file_id: "note".into(),
                    }),
                }],
                Instant::now(),
            )
            .arrived[0];
        assert_eq!(text_id, 9);
        drop(store.admit(&access, text_id, b"x").unwrap());
        let session = SessionAssets::new(store, access, None, Handle::current(), true);
        tokio::task::spawn_blocking(move || {
            let capture = dekopon_test_support::CaptureLayer::workspace();
            let _guard = tracing_subscriber::registry()
                .with(capture.clone())
                .set_default();
            let model = FetchScript {
                calls: std::sync::atomic::AtomicUsize::new(0),
                last: Mutex::new(vec![]),
            };
            run_prompt_session(
                &model,
                &NoScripts,
                SessionInputs::new(
                    "show the note",
                    PromptLimits {
                        max_steps: 11,
                        max_capability_calls: 0,
                    },
                )
                .with_assets(&session),
                &mut History::default(),
            )
            .unwrap();
            let last = model.last.lock();
            assert_eq!(
                last.iter()
                    .filter_map(|message| message.parts())
                    .flatten()
                    .filter(|part| matches!(part, dekopon_model::model::ContentPart::Image { .. }))
                    .count(),
                8
            );
            let refusal = last
                .iter()
                .filter(|message| message.role() == "tool")
                .filter_map(|message| message.content())
                .find(|text| text.contains("chat-asset:9"))
                .expect("the unseen text ID is reported");
            assert!(refusal.contains("will not be shown"), "{refusal}");
            assert!(!last.iter().any(|message| message.content() == Some("x")));
            assert_eq!(
                capture
                    .events_where(
                        "dekopon_agent::audit",
                        &[("audit.event", "agent.asset.refused"), ("asset.id", "9")]
                    )
                    .len(),
                1
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eight_retained_photos_can_be_fetched_in_one_session() {
        let store = store(64);
        let access = access("photos");
        let ids: Vec<_> = (1..=8).map(|_| register(&store, &access)).collect();
        for (id, value) in ids.iter().zip(1..=8) {
            drop(store.admit(&access, *id, &[value; 3]).unwrap());
        }
        let session = SessionAssets::new(store, access, None, Handle::current(), true);
        tokio::task::spawn_blocking(move || {
            for (id, value) in ids.iter().zip(1..=8) {
                assert_eq!(session.fetch(*id).unwrap().data.read().unwrap(), [value; 3]);
            }
            assert_eq!(
                session.fetch(u64::MAX).unwrap_err(),
                AssetFailure::Unknown.for_model(u64::MAX)
            );
        })
        .await
        .unwrap();
    }

    #[test]
    fn lru_use_a_admit_c_evicts_b_and_listing_does_not_touch() {
        let store = store(6);
        let access = access("one");
        let a = register(&store, &access);
        let b = register(&store, &access);
        drop(store.admit(&access, a, b"aaa").unwrap());
        drop(store.admit(&access, b, b"bbb").unwrap());
        drop(store.pin(&access, a, Recency::Touch).unwrap());
        assert_eq!(store.get_inventory(&access).len(), 2);
        let c = register(&store, &access);
        drop(store.admit(&access, c, b"ccc").unwrap());
        assert!(store.pin(&access, a, Recency::Keep).unwrap().is_some());
        assert_eq!(
            store.pin(&access, b, Recency::Touch),
            Err(BlobError::Reclaimed)
        );
        assert!(store.pin(&access, c, Recency::Keep).unwrap().is_some());
        assert_eq!(store.retention.lock().bytes, 6);
    }

    #[test]
    fn chat_table_eviction_reclaims_unpinned_residency_but_preserves_delivery_pins() {
        for pinned in [false, true] {
            let store = store(1024);
            let access = access("one");
            let first = register(&store, &access);
            let data = store.admit(&access, first, b"aaa").unwrap();
            let pin = if pinned {
                Some(data)
            } else {
                drop(data);
                None
            };
            for _ in 1..MAX_ASSETS_PER_CONVERSATION {
                register(&store, &access);
            }
            assert_eq!(
                store.get_inventory(&access).len(),
                MAX_ASSETS_PER_CONVERSATION
            );
            assert_eq!(store.retention.lock().bytes, 3);
            register(&store, &access);
            assert_eq!(
                store.get_inventory(&access).len(),
                MAX_ASSETS_PER_CONVERSATION
            );
            assert!(store.get_access(&access, first, Instant::now()).is_none());
            assert_eq!(residents(&store), usize::from(pinned));
            assert_eq!(store.retention.lock().bytes, if pinned { 3 } else { 0 });
            if let Some(pin) = pin {
                assert_eq!(pin.read().unwrap(), b"aaa");
            }
        }
    }

    #[tokio::test]
    async fn generated_table_eviction_reclaims_unpinned_residency_but_preserves_delivery_pins() {
        for pinned in [false, true] {
            let store = store(1024);
            let access = access("one");
            let session = session(Arc::clone(&store), access.clone());
            let mut first = None;
            let mut pin = None;
            for _ in 0..MAX_ASSETS_PER_CONVERSATION {
                let (fd, metadata) = received(b"abc", "text/plain", AssetEncoding::Identity);
                let id = session
                    .register(fd, &metadata, "asset.attach", "invocation")
                    .unwrap();
                if first.is_none() {
                    first = Some(id);
                    if pinned {
                        pin = store.pin(&access, id, Recency::Keep).unwrap();
                    }
                }
            }
            assert_eq!(residents(&store), MAX_ASSETS_PER_CONVERSATION);
            let (fd, metadata) = received(b"abc", "text/plain", AssetEncoding::Identity);
            session
                .register(fd, &metadata, "asset.attach", "invocation")
                .unwrap();
            assert_eq!(session.rows().len(), MAX_ASSETS_PER_CONVERSATION);
            assert!(
                store
                    .get_access(&access, first.unwrap(), Instant::now())
                    .is_none()
            );
            let expected = MAX_ASSETS_PER_CONVERSATION + usize::from(pinned);
            assert_eq!(residents(&store), expected);
            assert_eq!(store.retention.lock().bytes, 3 * expected);
            if let Some(pin) = pin {
                assert_eq!(pin.read().unwrap(), b"abc");
            }
        }
    }

    #[test]
    fn pins_oversize_zero_and_failure_accounting_do_not_displace_useful_assets() {
        let store = store(3);
        let access = access("one");
        let a = register(&store, &access);
        let pin = store.admit(&access, a, b"aaa").unwrap();
        let b = register(&store, &access);
        assert_eq!(store.admit(&access, b, b"bbbb"), Err(BlobError::TooLarge));
        assert_eq!(store.admit(&access, b, b"bbb"), Err(BlobError::Capacity));
        assert_eq!(pin.read().unwrap(), b"aaa");
        assert_eq!(store.retention.lock().bytes, 3);
        drop(pin);
        drop(store.admit(&access, b, b"bbb").unwrap());
        assert_eq!(store.retention.lock().bytes, 3);
        let zero = AssetStore::with_retention(8, Duration::from_secs(600), 0);
        let c = register(&zero, &access);
        assert_eq!(zero.admit(&access, c, b""), Err(BlobError::Disabled));
        assert_eq!(zero.retention.lock().bytes, 0);
    }

    #[test]
    fn weak_references_do_not_pin_and_retired_pins_stay_accounted() {
        let store = store(3);
        let first = access("one");
        let a = register(&store, &first);
        let pin = store.admit(&first, a, b"aaa").unwrap();
        let weak = BlobReference::new(
            Arc::new(ScopedBlob {
                store: Arc::downgrade(&store),
                access: first.clone(),
                id: a,
            }),
            3,
            a,
        );
        first.fence.as_ref().unwrap().deactivate();
        assert_eq!(weak.read(), Err(BlobError::Unauthorized));
        let second = access("two");
        let b = register(&store, &second);
        assert_eq!(store.admit(&second, b, b"bbb"), Err(BlobError::Capacity));
        assert_eq!(
            pin.read().unwrap(),
            b"aaa",
            "already active pin survives retirement"
        );
        drop(pin);
        drop(store.admit(&second, b, b"bbb").unwrap());
        assert_eq!(store.retention.lock().bytes, 3);
        assert_eq!(
            store.pin(&second, a + 10, Recency::Touch),
            Err(BlobError::Unknown)
        );
    }

    #[test]
    fn tombstones_are_bounded_and_evicted_ids_never_redownload() {
        let store = store(1);
        let access = access("one");
        for _ in 0..MAX_RELEASE_TOMBSTONES + 5 {
            let id = register(&store, &access);
            drop(store.admit(&access, id, b"x").unwrap());
        }
        assert_eq!(residents(&store), 1);
        let cache = store.retention.lock();
        assert_eq!(cache.released.len(), MAX_RELEASE_TOMBSTONES);
        assert_eq!(cache.bytes, 1);
    }

    #[test]
    fn retention_write_failure_has_zero_charge_and_no_resident_entry() {
        const CHILD: &str = "DEKOPON_TEST_RETENTION_WRITE_FAILURE";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("not-a-directory");
            std::fs::write(&path, b"fixture").unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "asset::retention_tests::retention_write_failure_has_zero_charge_and_no_resident_entry"])
                .env(CHILD, "1").env("TMPDIR", &path).env("TMP", &path).env("TEMP", &path).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let store = store(3);
        let access = access("one");
        let id = register(&store, &access);
        assert!(matches!(
            store.admit(&access, id, b"aaa"),
            Err(BlobError::Io(_))
        ));
        assert_eq!(store.retention.lock().bytes, 0);
        assert_eq!(residents(&store), 0);
    }

    #[test]
    fn retention_miss_is_correlated_and_contains_only_bounded_metadata() {
        use tracing_subscriber::prelude::*;
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone().with_filter(
                tracing_subscriber::EnvFilter::try_new(crate::OTEL_TRACE_FILTER).unwrap(),
            ))
            .set_default();
        let span = tracing::info_span!("gateway.session");
        assert!(
            span.metadata()
                .unwrap()
                .target()
                .starts_with("dekopon_gatewayd::")
        );
        span.in_scope(|| {
            assert_eq!(Retention::new(3).check_size(7, 4), Err(BlobError::TooLarge));
            tracing::info!(target: "legacy_gateway::audit", "wrong audit target");
        });
        assert!(capture.records().iter().any(|record| matches!(record,
            dekopon_test_support::Record::Event { target, parent: Some(parent), .. }
            if target == "dekopon_gatewayd::audit" && record.field("audit.event") == Some("gateway.asset.retention_miss") && parent == "gateway.session")));
        assert!(capture.records().iter().any(|record| matches!(
            record,
            dekopon_test_support::Record::Span {
                name: "gateway.session",
                ..
            }
        )));
        assert!(!capture.events_text().contains("wrong audit target"));
        let text = capture.text();
        assert!(text.contains("gateway.asset.retention_miss"), "{text}");
        assert!(
            text.contains("asset.id=7")
                && text.contains("asset.bytes=4")
                && text.contains("asset.budget=3"),
            "{text}"
        );
        assert!(!text.contains("dekopon-assets-") && !text.contains("base64"));
    }

    struct Fetcher(std::sync::atomic::AtomicUsize);
    impl AssetFetcher for Fetcher {
        fn fetch(
            &self,
            _: &AssetSourceRef,
            _: u64,
        ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, crate::transport::TransportError>>
        {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(b"aaa".to_vec()) })
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_and_known_zero_lengths_stay_distinct_without_listing_downloads() {
        let store = store(16);
        let access = access("lengths");
        let registered = store.assets_for_access(
            &access,
            [None, Some(0), Some(2048)]
                .into_iter()
                .enumerate()
                .map(|(index, size)| PendingAsset {
                    name: format!("input-{index}.png"),
                    mime: "image/png".into(),
                    size,
                    source: Some(AssetSourceRef::WhatsApp {
                        media_id: "789".into(),
                        mime: "image/png".into(),
                    }),
                })
                .collect(),
            Instant::now(),
        );
        let note = reference_note(&registered, true).unwrap();
        assert!(note.contains("input-0.png (image/png, size unknown)"));
        assert!(note.contains("input-1.png (image/png, 0 B)"));
        assert!(note.contains("input-2.png (image/png, 2 KB)"));
        let fetcher = Arc::new(Fetcher(std::sync::atomic::AtomicUsize::new(0)));
        let session = SessionAssets::new(
            Arc::clone(&store),
            access.clone(),
            Some(Arc::clone(&fetcher) as Arc<dyn AssetFetcher>),
            Handle::current(),
            true,
        );
        assert_eq!(
            session
                .rows()
                .iter()
                .map(|row| row.bytes)
                .collect::<Vec<_>>(),
            [None, Some(0), Some(2048)]
        );
        assert_eq!(fetcher.0.load(Ordering::Relaxed), 0);
        tokio::task::spawn_blocking(move || {
            let id = registered.arrived[0];
            let blob = input(&session, id).unwrap();
            assert_eq!(blob.len(), 3);
            assert_eq!(session.rows()[0].bytes, Some(3));
            drop(blob);
            reclaim(&store, &access, id);
            assert_eq!(
                input(&session, id).unwrap_err(),
                ChatAssetRefusal::Reclaimed
            );
            assert_eq!(session.rows()[0].bytes, Some(3));
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
            let empty_id = registered.arrived[1];
            let empty = store.admit(&access, empty_id, b"").unwrap();
            assert_eq!(empty.len(), 0);
            assert_eq!(session.rows()[1].bytes, Some(0));
            drop(empty);
            reclaim(&store, &access, empty_id);
            assert_eq!(
                input(&session, empty_id).unwrap_err(),
                ChatAssetRefusal::Reclaimed
            );
            assert_eq!(session.rows()[1].bytes, Some(0));
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn downloaded_oversize_with_unknown_or_underreported_size_never_redownloads() {
        for reported_size in [None, Some(0), Some(1)] {
            let store = store(2);
            let access = access("one");
            let id = store
                .assets_for_access(
                    &access,
                    vec![PendingAsset {
                        name: "input.png".into(),
                        mime: "image/png".into(),
                        size: reported_size,
                        source: Some(AssetSourceRef::Telegram {
                            file_id: "file".into(),
                        }),
                    }],
                    Instant::now(),
                )
                .arrived[0];
            let fetcher = Arc::new(Fetcher(std::sync::atomic::AtomicUsize::new(0)));
            let session = SessionAssets::new(
                Arc::clone(&store),
                access,
                Some(Arc::clone(&fetcher) as Arc<dyn AssetFetcher>),
                Handle::current(),
                true,
            );
            tokio::task::spawn_blocking(move || {
                let first = session.fetch(id).unwrap_err();
                assert!(
                    first.contains("attachment exceeds the byte limit"),
                    "{first}"
                );
                assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
                let second = session.fetch(id).unwrap_err();
                assert!(
                    second.contains("released") && second.contains("ask the user to resend"),
                    "{second}"
                );
                assert!(second.contains("No automatic refetch"), "{second}");
                assert_eq!(
                    input(&session, id).unwrap_err(),
                    ChatAssetRefusal::Reclaimed
                );
                assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
                assert_eq!(session.rows()[0].bytes, Some(3));
                assert_eq!(store.retention.lock().bytes, 0);
                assert_eq!(residents(&store), 0);
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metadata_oversize_refuses_before_transport_download() {
        let store = store(2);
        let access = access("one");
        let id = register(&store, &access);
        let fetcher = Arc::new(Fetcher(std::sync::atomic::AtomicUsize::new(0)));
        let session = SessionAssets::new(
            Arc::clone(&store),
            access.clone(),
            Some(Arc::clone(&fetcher) as Arc<dyn AssetFetcher>),
            Handle::current(),
            true,
        );
        tokio::task::spawn_blocking(move || {
            for _ in 0..2 {
                assert!(
                    session
                        .fetch(id)
                        .unwrap_err()
                        .contains("attachment exceeds the byte limit")
                );
            }
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 0);
            assert!(store.pin(&access, id, Recency::Keep).unwrap().is_none());
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn whatsapp_png_validation_does_not_spool_outside_a_full_pinned_budget() {
        use crate::transport::{
            ChatTransport as _,
            whatsapp::tests_media::{MediaPeer, PNG, admitted_photo, bytes_reply, metadata},
        };
        use tracing_subscriber::prelude::*;

        let peer = MediaPeer::new(|origin, index| match index {
            0 => metadata(origin, "image/png", PNG),
            1 => bytes_reply(PNG),
            _ => panic!("unexpected redownload"),
        })
        .await;
        let (transport, message) = admitted_photo(&peer.origin, "image/png", None).await;
        let store = store(PNG.len());
        let access = access("one");
        let pinned_id = register(&store, &access);
        let id = store
            .assets_for_access(&access, message.assets, Instant::now())
            .arrived[0];
        let session = SessionAssets::new(
            Arc::clone(&store),
            access.clone(),
            transport.asset_fetcher(),
            Handle::current(),
            true,
        );
        tokio::task::spawn_blocking(move || {
            let capture = dekopon_test_support::CaptureLayer::workspace();
            let _guard = tracing_subscriber::registry()
                .with(capture.clone())
                .set_default();
            let pin = store.admit(&access, pinned_id, PNG).unwrap();
            let is_write = |record: &dekopon_test_support::Record| {
                matches!(record,
                dekopon_test_support::Record::Span { name: "asset.spool", .. }
                if record.field("operation") == Some("write"))
            };
            assert!(
                capture.records().iter().any(is_write),
                "capture must observe actual spool writes"
            );
            let before = capture.records().len();
            assert!(
                session
                    .fetch(id)
                    .unwrap_err()
                    .contains("scratch capacity exhausted")
            );
            assert!(
                !capture.records()[before..].iter().any(is_write),
                "PNG validation must not spool before budget admission"
            );
            assert_eq!(store.retention.lock().bytes, PNG.len());
            assert_eq!(pin.read().unwrap(), PNG);
            assert!(
                session
                    .fetch(id)
                    .unwrap_err()
                    .contains("ask the user to resend")
            );
        })
        .await
        .unwrap();
        assert_eq!(
            peer.requests.lock().len(),
            2,
            "one metadata lookup and one download"
        );
        peer.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_once_reclaimed_provider_and_model_refuse_without_redownload() {
        let store = store(3);
        let access = access("one");
        let a = register(&store, &access);
        let fetcher = Arc::new(Fetcher(std::sync::atomic::AtomicUsize::new(0)));
        let session = SessionAssets::new(
            Arc::clone(&store),
            access.clone(),
            Some(Arc::clone(&fetcher) as Arc<dyn AssetFetcher>),
            Handle::current(),
            true,
        );
        tokio::task::spawn_blocking(move || {
            let weak = session.fetch(a).unwrap();
            assert_eq!(weak.data.read().unwrap(), b"aaa");
            assert_eq!(input(&session, a).unwrap().read().unwrap(), b"aaa");
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
            let b = register(&store, &access);
            drop(store.admit(&access, b, b"bbb").unwrap());
            assert_eq!(weak.data.read(), Err(BlobError::Reclaimed));
            assert!(session.fetch(a).unwrap_err().contains("released"));
            assert_eq!(input(&session, a).unwrap_err(), ChatAssetRefusal::Reclaimed);
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
            let inputs = ChatAssetInputs::new(Arc::new(session));
            let proposal = json!([format!("chat-asset:{b}"), format!("chat-asset:{a}")]);
            let original = proposal.clone();
            assert!(matches!(
                Handle::current().block_on(inputs.prepare(&proposal, 4)),
                Err(ChatAssetRefusal::Reclaimed)
            ));
            assert_eq!(proposal, original, "no partial edit reaches submission");
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_finished_download_holds_no_pin_once_its_waiter_returns() {
        let store = store(3);
        let access = access("one");
        let a = register(&store, &access);
        let fetcher: Arc<dyn AssetFetcher> =
            Arc::new(Fetcher(std::sync::atomic::AtomicUsize::new(0)));
        let source = AssetSourceRef::Telegram {
            file_id: "file".into(),
        };
        let join = || match AssetStore::join(
            &store,
            &access,
            a,
            source.clone(),
            Arc::clone(&fetcher),
            &Handle::current(),
        )
        .unwrap()
        {
            Download::Pending(waiter) => waiter,
            Download::Ready(_) => panic!("the download has not started"),
        };
        let first = join();
        let second = join();
        assert_eq!(first.wait().await.unwrap().read().unwrap(), b"aaa");
        let b = register(&store, &access);
        drop(store.admit(&access, b, b"bbb").unwrap());
        assert!(matches!(
            second.wait().await,
            Err(AssetFailure::Storage(BlobError::Reclaimed))
        ));
    }

    struct Stalled {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Semaphore>,
    }
    impl AssetFetcher for Stalled {
        fn fetch(
            &self,
            _: &AssetSourceRef,
            _: u64,
        ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, crate::transport::TransportError>>
        {
            Box::pin(async {
                // Signalled on first poll, after the caller has fixed its fetch deadline.
                self.entered.notify_one();
                let _released = self.release.acquire().await;
                Ok(b"ok".to_vec())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_download_refuses_at_the_fetch_deadline() {
        let store = store(8);
        let access = access("deadline");
        let id = register(&store, &access);
        let entered = Arc::new(tokio::sync::Notify::new());
        let stalled = SessionAssets::new(
            store,
            access,
            Some(Arc::new(Stalled {
                entered: Arc::clone(&entered),
                release: Arc::new(tokio::sync::Semaphore::new(0)),
            }) as Arc<dyn AssetFetcher>),
            Handle::current(),
            true,
        );
        let fetch = tokio::task::spawn_blocking(move || stalled.fetch(id));
        entered.notified().await;
        tokio::time::advance(ASSET_FETCH_TIMEOUT).await;
        assert_eq!(
            fetch.await.unwrap().unwrap_err(),
            AssetFailure::TimedOut.for_model(id)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_download_does_not_hold_up_another_conversations_retained_attachment() {
        let store = store(8);
        let stalled_access = access("stalled");
        let stalled_id = register(&store, &stalled_access);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let fetcher = Stalled {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        };
        let stalled = SessionAssets::new(
            Arc::clone(&store),
            stalled_access,
            Some(Arc::new(fetcher) as Arc<dyn AssetFetcher>),
            Handle::current(),
            true,
        );
        let stalled = tokio::task::spawn_blocking(move || {
            stalled
                .fetch(stalled_id)
                .map(|asset| asset.data.read().unwrap())
        });
        entered.notified().await;

        let retained_access = access("retained");
        let retained_id = register(&store, &retained_access);
        drop(store.admit(&retained_access, retained_id, b"kep").unwrap());
        let retained = session(Arc::clone(&store), retained_access);
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || retained.fetch(retained_id)),
        )
        .await
        .expect("a retained read does not wait on another conversation's download")
        .unwrap()
        .unwrap();
        assert_eq!(read.data.read().unwrap(), b"kep");

        release.add_permits(1);
        assert_eq!(stalled.await.unwrap().unwrap(), b"ok");
    }

    fn received(bytes: &[u8], label: &str, encoding: AssetEncoding) -> (OwnedFd, NewAsset) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, bytes).unwrap();
        (
            std::fs::File::open(file.path()).unwrap().into(),
            NewAsset {
                descriptor: 0,
                content_type: label.to_owned(),
                encoding,
                bytes: bytes.len() as u64,
                sha256: "0".repeat(64),
            },
        )
    }
    fn session(store: Arc<AssetStore>, access: AssetAccess) -> Arc<SessionAssets> {
        Arc::new(SessionAssets::new(
            store,
            access,
            None,
            Handle::current(),
            true,
        ))
    }
    #[tokio::test]
    async fn generated_registration_is_available_same_session_with_gateway_provenance() {
        let store = store(1024);
        let access = access("one");
        let session = session(Arc::clone(&store), access.clone());
        let png = b"\x89PNG\r\n\x1a\nnew pixels";
        let (descriptor, metadata) = received(png, "image/png", AssetEncoding::Identity);
        let id = session
            .register(descriptor, &metadata, "image.edit", "invocation-2")
            .unwrap();
        assert_eq!(id, 1);
        assert!(session.offers_fetch());
        assert_eq!(
            session
                .resolve_input(id)
                .await
                .unwrap()
                .blob
                .read()
                .unwrap(),
            png
        );
        assert!(
            matches!(store.get_access(&access, id, Instant::now()).unwrap().source,
            Some(AssetSourceRef::Generated { capability, invocation }) if capability == "image.edit" && invocation == "invocation-2")
        );
        assert_eq!(store.retention.lock().bytes, png.len());
        let slot = ReplyAttachments::new(
            MAX_SENDS_PER_TURN,
            Arc::<SessionAssets>::clone(&session),
            "local".to_owned(),
        );
        assert!(slot.take().is_empty(), "attach never sends");
        slot.receive(vec![], vec![], vec![], vec![id], "asset.send", "send-1");
        assert_eq!(slot.remaining(), 3);
        assert_eq!(slot.take().pop().unwrap().bytes().unwrap(), png);
        slot.finish(dekopon_agent::attachment::AssetDeliveryDisposition::Delivered);
        let next = ReplyAttachments::new(
            MAX_SENDS_PER_TURN,
            Arc::<SessionAssets>::clone(&session),
            "local".to_owned(),
        );
        next.receive(vec![], vec![], vec![], vec![id], "asset.send", "send-2");
        assert_eq!(next.remaining(), 4, "duplicate in next turn costs nothing");
        assert!(next.take().is_empty());
        assert_eq!(session.remove(id), Err(BlobError::Unauthorized));
    }
    #[tokio::test]
    async fn pathless_removal_closes_residency_and_disabled_intake_publishes_no_id() {
        let store = store(3);
        let access = access("one");
        let session = session(Arc::clone(&store), access.clone());
        let (fd, metadata) = received(b"abc", "text/plain", AssetEncoding::Identity);
        let id = session
            .register(fd, &metadata, "asset.attach", "invocation")
            .unwrap();
        session.remove(id).unwrap();
        assert_eq!(store.retention.lock().bytes, 0);
        assert!(session.rows().is_empty());
        let disabled = Arc::new(AssetStore::with_retention(8, Duration::from_secs(600), 0));
        let disabled_session = SessionAssets::new(
            Arc::clone(&disabled),
            access.clone(),
            None,
            Handle::current(),
            true,
        );
        let (fd, metadata) = received(b"abc", "text/plain", AssetEncoding::Identity);
        assert_eq!(
            disabled_session.register(fd, &metadata, "asset.attach", "invocation"),
            Err(BlobError::Disabled)
        );
        assert!(disabled.get_inventory(&access).is_empty());
    }
    #[tokio::test]
    async fn encoded_intake_and_reference_budget_use_decoded_limits() {
        let store = store(64 * 1024 * 1024);
        let session = session(Arc::clone(&store), access("encoded-limits"));
        let raw = vec![0; dekopon_model::asset::MAX_ATTACHMENT_BYTES];
        let encoded = STANDARD.encode(&raw);
        assert_eq!(encoded.len(), 11_184_812);
        let mut references = Vec::new();
        for _ in 0..5 {
            let (fd, metadata) = received(encoded.as_bytes(), "image/png", AssetEncoding::Base64);
            let id = session
                .register(fd, &metadata, "image.edit", "invocation")
                .unwrap();
            references.push(format!("chat-asset:{id}"));
        }
        assert_eq!(store.retention.lock().bytes, 5 * encoded.len());
        let inputs = ChatAssetInputs::new(Arc::<SessionAssets>::clone(&session));
        let (assets, pins) = inputs.prepare(&json!(references), 4).await.unwrap();
        assert_eq!(assets.descriptors.len(), 5);
        assert_eq!(
            pins.iter().map(DiskBlob::len).sum::<usize>(),
            5 * encoded.len()
        );
        drop((assets, pins));
        for encoding in [AssetEncoding::Identity, AssetEncoding::Base64] {
            let bytes = match encoding {
                AssetEncoding::Identity => vec![0; raw.len() + 1],
                AssetEncoding::Base64 => STANDARD.encode(vec![0; raw.len() + 1]).into_bytes(),
            };
            let (fd, metadata) = received(&bytes, "image/png", encoding);
            assert_eq!(
                session.register(fd, &metadata, "image.edit", "overflow"),
                Err(BlobError::TooLarge)
            );
            assert_eq!(session.rows().len(), 5);
        }
    }

    #[tokio::test]
    async fn decoded_prefix_sniff_logs_once_only_on_label_disagreement() {
        use tracing_subscriber::prelude::*;
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let session = session(store(1024), access("one"));
        let raw = b"\x89PNG\r\n\x1a\nPRIVATE_PAYLOAD";
        for encoding in [AssetEncoding::Identity, AssetEncoding::Base64] {
            let bytes = match encoding {
                AssetEncoding::Identity => raw.to_vec(),
                AssetEncoding::Base64 => STANDARD.encode(raw).into_bytes(),
            };
            for label in ["image/png", "image/jpeg"] {
                let before = capture
                    .text()
                    .matches("gateway.asset.content_type_mismatch")
                    .count();
                let (fd, metadata) = received(&bytes, label, encoding);
                let id = session
                    .register(fd, &metadata, "image.edit", "invocation")
                    .unwrap();
                assert_eq!(
                    session
                        .rows()
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .content_type,
                    label
                );
                let after = capture
                    .text()
                    .matches("gateway.asset.content_type_mismatch")
                    .count();
                assert_eq!(after - before, usize::from(label != "image/png"));
                assert_eq!(
                    GeneratedImage::new(
                        session.resolve_input(id).await.unwrap().blob,
                        label.to_owned(),
                        encoding
                    )
                    .bytes()
                    .unwrap(),
                    raw
                );
            }
        }
        let text = capture.text();
        assert!(!text.contains("PRIVATE_PAYLOAD") && !text.contains(&STANDARD.encode(raw)));
        assert!(text.contains("asset.detected_type") && text.contains("asset.sha256"));
    }
    #[tokio::test]
    async fn output_note_and_mismatch_label_are_bounded_at_128_characters() {
        use tracing_subscriber::prelude::*;
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let session = session(store(1024), access("one"));
        let slot = ReplyAttachments::new(
            MAX_SENDS_PER_TURN,
            Arc::<SessionAssets>::clone(&session),
            "local".to_owned(),
        );
        for length in [128, 129] {
            let label = "a".repeat(length);
            let (fd, metadata) = received(b"\x89PNG\r\n\x1a\n", &label, AssetEncoding::Identity);
            let note = slot.receive(
                vec![metadata],
                vec![fd],
                vec![],
                vec![],
                "image.edit",
                "invocation",
            );
            assert!(note.contains(&"a".repeat(128)));
            assert!(!note.contains(&"a".repeat(129)));
            assert_eq!(
                session.rows().last().unwrap().content_type,
                label,
                "only presentation is bounded, never the authoritative label"
            );
        }
        assert!(capture.text().contains(&"a".repeat(128)));
        assert!(!capture.text().contains(&"a".repeat(129)));
    }

    #[test]
    fn sniff_reads_only_the_decoded_prefix_at_twelve_bytes_and_one_past() {
        for length in [12, 13] {
            let mut bytes = b"RIFFxxxxWEBP".to_vec();
            bytes.resize(length, b'x');
            let raw = DiskBlob::from_bytes(&bytes).unwrap();
            assert_eq!(
                sniff(&raw, AssetEncoding::Identity).unwrap(),
                Some("image/webp")
            );
            let encoded = DiskBlob::from_bytes(STANDARD.encode(&bytes).as_bytes()).unwrap();
            assert_eq!(
                sniff(&encoded, AssetEncoding::Base64).unwrap(),
                Some("image/webp")
            );
        }
    }

    #[tokio::test]
    async fn fourth_send_fits_fifth_refuses_and_a_new_turn_has_fresh_allowance() {
        let session = session(store(1024), access("one"));
        let slot = ReplyAttachments::new(
            MAX_SENDS_PER_TURN,
            Arc::<SessionAssets>::clone(&session),
            "local".to_owned(),
        );
        for index in 0..5 {
            let (fd, metadata) = received(b"abc", "text/plain", AssetEncoding::Identity);
            let id = session
                .register(fd, &metadata, "asset.attach", "invocation")
                .unwrap();
            let note = slot.receive(vec![], vec![], vec![], vec![id], "asset.send", "send");
            assert_eq!(note.is_empty(), index < 4);
        }
        assert_eq!(slot.remaining(), 0);
        assert_eq!(slot.take().len(), 4);
        slot.finish(dekopon_agent::attachment::AssetDeliveryDisposition::Delivered);
        let next = ReplyAttachments::new(MAX_SENDS_PER_TURN, session, "local".to_owned());
        assert_eq!(next.remaining(), 4);
    }

    #[tokio::test]
    async fn each_terminal_disposition_logs_once_and_only_failed_sends_leave_a_notice() {
        use dekopon_agent::attachment::AssetDeliveryDisposition;
        use tracing_subscriber::prelude::*;
        for disposition in [
            AssetDeliveryDisposition::Abandoned,
            AssetDeliveryDisposition::Failed,
            AssetDeliveryDisposition::Delivered,
        ] {
            let capture = dekopon_test_support::CaptureLayer::workspace();
            let _guard = tracing_subscriber::registry()
                .with(capture.clone())
                .set_default();
            let store = store(1024);
            let access = access("one");
            let session = session(Arc::clone(&store), access.clone());
            let (fd, metadata) = received(b"abc", "text/plain", AssetEncoding::Identity);
            let id = session
                .register(fd, &metadata, "asset.attach", "invocation")
                .unwrap();
            let slot = ReplyAttachments::new(MAX_SENDS_PER_TURN, session, "local".to_owned());
            slot.receive(vec![], vec![], vec![], vec![id], "asset.send", "send");
            if disposition != AssetDeliveryDisposition::Abandoned {
                drop(slot.take());
            }
            slot.finish(disposition);
            slot.finish(disposition);
            drop(slot);
            assert_eq!(capture.text().matches("agent.asset.send").count(), 1);
            let dispatched = disposition != AssetDeliveryDisposition::Abandoned;
            assert!(capture.text().contains(&format!("dispatched={dispatched}")));
            match disposition {
                AssetDeliveryDisposition::Abandoned => {
                    assert!(capture.text().contains("turn ended before reply"))
                }
                AssetDeliveryDisposition::Failed => {
                    assert!(capture.text().contains("delivery failed"))
                }
                AssetDeliveryDisposition::Delivered => assert!(!capture.text().contains("error=")),
            }
            assert_eq!(
                store.take_delivery_notice(&access).is_some(),
                disposition != AssetDeliveryDisposition::Delivered
            );
            assert!(store.take_delivery_notice(&access).is_none());
        }
    }
}
