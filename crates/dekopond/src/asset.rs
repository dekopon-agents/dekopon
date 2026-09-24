//! This lives beside transport credentials in the gateway, not the broker, because resolving an
//! attachment reference is reading already-authenticated data, not deciding whether an effect may
//! happen.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    io::Read,
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use dekopon_agent::attachment::GeneratedImage;
use dekopon_agent::{
    attachment::{ChatAssetRefusal, ChatAssetSource},
    prompt::{AssetSource, FetchedAsset},
};
use dekopon_broker_protocol::{AssetEncoding, AssetRow, NewAsset};
use dekopon_model::asset::{BlobError, BlobReference, BlobSource, DiskBlob};
use tokio::runtime::Handle;

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
    fetched: bool,
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

#[derive(Clone, Eq, PartialEq)]
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
        let _gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    fn with_active<T>(&self, operation: impl FnOnce(&AssetStateKey) -> T) -> Option<T> {
        let Some(fence) = self.fence.as_ref() else {
            return Some(operation(&self.key));
        };
        let _gate = fence
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fence.is_active().then(|| operation(&self.key))
    }

    fn is_active(&self) -> bool {
        self.with_active(|_| ()).is_some()
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
    next_one_shot_id: AtomicU64,
}

struct ConversationAssets {
    assets: Vec<AssetRef>,
    touched: Instant,
    fence: Option<Weak<AssetFence>>,
    delivery_failed: bool,
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
            next_one_shot_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn retention_enabled(&self) -> bool {
        self.conversations > 0
            && self
                .retention
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .budget
                > 0
    }

    #[cfg(test)]
    pub fn assets_for(
        &self,
        conversation: &ConversationKey,
        arriving: Vec<PendingAsset>,
        images_supported: bool,
        now: Instant,
    ) -> Registered {
        self.assets_for_access(
            &AssetAccess::one_shot(conversation.clone()),
            arriving,
            images_supported,
            now,
        )
    }

    pub fn assets_for_access(
        &self,
        access: &AssetAccess,
        arriving: Vec<PendingAsset>,
        images_supported: bool,
        now: Instant,
    ) -> Registered {
        access
            .with_active(|state_key| {
                let mut entries = self
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Self::expire(&mut entries, self.idle_timeout, now);
                let mut arrived = Vec::with_capacity(arriving.len());
                if !arriving.is_empty() {
                    let entry =
                        entries
                            .entry(state_key.clone())
                            .or_insert_with(|| ConversationAssets {
                                assets: Vec::new(),
                                touched: now,
                                fence: access.weak_fence(),
                                delivery_failed: false,
                            });
                    entry.touched = now;
                    for pending in arriving {
                        let asset = AssetRef {
                            id: access.allocate_id(&self.next_one_shot_id),
                            name: pending.name,
                            mime: pending.mime,
                            size: pending.size,
                            source: pending.source,
                            fetched: false,
                            encoding: AssetEncoding::Identity,
                            sent: false,
                        };
                        arrived.push(asset.id);
                        entry.assets.push(asset);
                    }
                    while entry.assets.len() > MAX_ASSETS_PER_CONVERSATION {
                        let evicted = entry.assets.remove(0);
                        self.retention
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .evict(&(state_key.clone(), evicted.id));
                    }
                    Self::enforce_ceiling(&mut entries, self.conversations);
                }

                let inventory = entries.get_mut(state_key).map_or_else(Vec::new, |entry| {
                    entry.touched = now;
                    entry.assets.clone()
                });
                let fetchable = inventory
                    .iter()
                    .any(|asset| asset.is_fetchable(images_supported));
                Registered {
                    inventory,
                    arrived,
                    fetchable,
                }
            })
            .unwrap_or_else(Registered::empty)
    }

    // Recalled ids are kept because replayed turns already name them as `Chat Asset #N`.
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
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::expire(&mut entries, self.idle_timeout, now);
            let entry = entries
                .entry(state_key.clone())
                .or_insert_with(|| ConversationAssets {
                    assets: Vec::new(),
                    touched: now,
                    fence: access.weak_fence(),
                    delivery_failed: false,
                });
            entry.touched = now;
            entry
                .assets
                .extend(recalled.into_iter().map(|recalled| AssetRef {
                    id: recalled.id,
                    name: recalled.asset.name,
                    mime: recalled.asset.mime,
                    size: recalled.asset.size,
                    source: recalled.asset.source,
                    fetched: false,
                    encoding: AssetEncoding::Identity,
                    sent: false,
                }));
            entry.assets.sort_by_key(|asset| asset.id);
            let excess = entry
                .assets
                .len()
                .saturating_sub(MAX_ASSETS_PER_CONVERSATION);
            entry.assets.drain(..excess);
            Self::enforce_ceiling(&mut entries, self.conversations);
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
                let mut entries = self
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Self::expire(&mut entries, self.idle_timeout, now);
                let entry = entries.get_mut(state_key)?;
                entry.touched = now;
                entry.assets.iter().find(|asset| asset.id == id).cloned()
            })
            .flatten()
    }

    fn expire(
        entries: &mut HashMap<AssetStateKey, ConversationAssets>,
        idle_timeout: Duration,
        now: Instant,
    ) {
        entries.retain(|_, entry| {
            let active = entry
                .fence
                .as_ref()
                .is_none_or(|fence| fence.upgrade().is_some_and(|fence| fence.is_active()));
            active && now.saturating_duration_since(entry.touched) < idle_timeout
        });
    }

    fn enforce_ceiling(entries: &mut HashMap<AssetStateKey, ConversationAssets>, capacity: usize) {
        while entries.len() > capacity {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
    }
}

impl fmt::Debug for AssetStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                "\n  Chat Asset #{} — {name} ({mime}, {size}){arrived}",
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
    pub fetchable: bool,
}

impl Registered {
    fn empty() -> Self {
        Self {
            inventory: Vec::new(),
            arrived: Vec::new(),
            fetchable: false,
        }
    }
}

const MAX_ASSET_BYTES: u64 = dekopon_model::asset::MAX_ATTACHMENT_BYTES as u64;

const MAX_FETCHES_PER_SESSION: u32 = 4;

const ASSET_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Fetch is synchronous because the prompt loop that calls it runs on a blocking task, so blocking
/// here parks a blocking thread rather than a runtime worker.
pub(crate) struct SessionAssets {
    store: Arc<AssetStore>,
    access: AssetAccess,
    fetcher: Option<Arc<dyn AssetFetcher>>,
    runtime: Handle,
    images_supported: bool,
    available: bool,
    spent: Mutex<u32>,
}

impl SessionAssets {
    pub fn new(
        store: Arc<AssetStore>,
        access: AssetAccess,
        fetcher: Option<Arc<dyn AssetFetcher>>,
        runtime: Handle,
        images_supported: bool,
        available: bool,
    ) -> Self {
        Self {
            store,
            access,
            fetcher,
            runtime,
            images_supported,
            available,
            spent: Mutex::new(0),
        }
    }
}

impl AssetSource for SessionAssets {
    fn is_empty(&self) -> bool {
        if !self.access.is_active() {
            return true;
        }
        if self.available && self.fetcher.is_some() {
            return false;
        }
        !self.store.get_inventory(&self.access).iter().any(|asset| {
            asset.is_fetchable(self.images_supported)
                && matches!(asset.source, Some(AssetSourceRef::Generated { .. }))
        })
    }

    fn fetch(&self, id: u64) -> Result<FetchedAsset, String> {
        {
            let mut spent = self
                .spent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *spent >= MAX_FETCHES_PER_SESSION {
                return Err(format!(
                    "This session has already opened {MAX_FETCHES_PER_SESSION} attachments, which is the limit. Answer with what you have."
                ));
            }
            *spent += 1;
        }
        let (asset, data) = self
            .load(
                id,
                AssetConsumer::Model {
                    images_supported: self.images_supported,
                },
            )
            .map_err(|failure| failure.for_model(id))?;
        let reference = BlobReference::new(
            Arc::new(ScopedBlob {
                store: Arc::downgrade(&self.store),
                access: self.access.clone(),
                id,
            }),
            data.len(),
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
    fn fetch_for_capability(
        &self,
        id: u64,
    ) -> Result<(String, dekopon_model::asset::DiskBlob), ChatAssetRefusal> {
        let (asset, _resolution_pin) = self
            .load(id, AssetConsumer::Capability)
            .map_err(AssetFailure::for_capability)?;
        // Capability resolution counts as actual use, not replay; the initial pin is kept until the
        // recency update completes, and bytes are never substituted if that update fails.
        let data = self
            .store
            .pin(&self.access, id, true)
            .map_err(|error| AssetFailure::Storage(error).for_capability())?
            .ok_or(ChatAssetRefusal::Reclaimed)?;
        Ok((asset.mime, data))
    }
    fn rows(&self) -> Vec<AssetRow> {
        self.store
            .get_inventory(&self.access)
            .into_iter()
            .map(|asset| AssetRow {
                id: asset.id,
                content_type: asset.mime,
                encoding: asset.encoding,
                bytes: asset.size,
                origin: match asset.source {
                    Some(AssetSourceRef::Generated { capability, .. }) => {
                        format!("provider:{capability}")
                    }
                    _ => "chat".to_owned(),
                },
                sent: asset.sent,
            })
            .collect()
    }
}

impl SessionAssets {
    fn load(&self, id: u64, consumer: AssetConsumer) -> Result<(AssetRef, DiskBlob), AssetFailure> {
        // There is no store-wide download lock because the session gate admits one session per
        // conversation with sequential reads, so no attachment is ever fetched twice at once.
        let Some(asset) = self.store.get_access(&self.access, id, Instant::now()) else {
            return match self.store.pin(&self.access, id, false) {
                Err(BlobError::Unknown) => Err(AssetFailure::Unknown),
                Err(error) => Err(AssetFailure::Storage(error)),
                Ok(_) => Err(AssetFailure::Unknown),
            };
        };
        if let AssetConsumer::Model { images_supported } = consumer
            && !asset.is_fetchable(images_supported)
        {
            return Err(AssetFailure::Unreadable {
                reason: asset.unreadable_reason(images_supported),
                refusal: if asset.source.is_none() {
                    ChatAssetRefusal::Unavailable
                } else {
                    ChatAssetRefusal::UnsupportedMedia
                },
            });
        }
        if let Some(data) = self
            .store
            .pin(&self.access, id, false)
            .map_err(AssetFailure::Storage)?
        {
            return Ok((asset, data));
        }
        if let Some(size) = asset.size
            && size > MAX_ASSET_BYTES
        {
            return Err(AssetFailure::TooLarge { size });
        }
        self.store
            .check_size(id, asset.size.unwrap_or_default() as usize)
            .map_err(AssetFailure::Storage)?;
        let (Some(fetcher), Some(source)) = (self.fetcher.as_ref(), asset.source.as_ref()) else {
            return Err(AssetFailure::Unavailable);
        };
        let data = self
            .runtime
            .block_on(tokio::time::timeout(
                ASSET_FETCH_TIMEOUT,
                fetcher.fetch(source, MAX_ASSET_BYTES),
            ))
            .map_err(|_elapsed| AssetFailure::Transport {
                category: "fetch-timeout",
            })?
            .map_err(|error| AssetFailure::Transport {
                // Only the transport's error category goes into the prompt, never its message,
                // since a transport error can carry arbitrary service text.
                category: error.category(),
            })?;
        // A generation can retire while a transport read is in flight; the read may not always be
        // cancellable, but its bytes must never reach the model once retirement is visible.
        if !self.access.is_active() {
            return Err(AssetFailure::Unknown);
        }
        let data = self
            .store
            .admit(&self.access, id, &data)
            .map_err(AssetFailure::Storage)?;
        Ok((asset, data))
    }
}

enum AssetConsumer {
    Model { images_supported: bool },
    Capability,
}

enum AssetFailure {
    Storage(dekopon_model::asset::BlobError),
    Unknown,
    Unreadable {
        reason: &'static str,
        refusal: ChatAssetRefusal,
    },
    TooLarge {
        size: u64,
    },
    Unavailable,
    Transport {
        category: &'static str,
    },
}

impl AssetFailure {
    fn for_model(self, id: u64) -> String {
        match self {
            Self::Storage(error) => format!(
                "Chat Asset #{id} is unavailable: {error}. No automatic refetch or fallback was performed."
            ),
            Self::Unknown => format!(
                "There is no Chat Asset #{id} in this conversation. The reference lines in the messages above name the ones there are."
            ),
            Self::Unreadable { reason, .. } => {
                format!("Chat Asset #{id} cannot be opened: {reason}.")
            }
            Self::TooLarge { size } => format!(
                "Chat Asset #{id} is {} which is over the {} the gateway will read.",
                kibibytes(size),
                kibibytes(MAX_ASSET_BYTES)
            ),
            Self::Unavailable => format!("Chat Asset #{id} cannot be opened."),
            Self::Transport { category } => {
                format!("Chat Asset #{id} could not be read ({category}).")
            }
        }
    }

    const fn for_capability(self) -> ChatAssetRefusal {
        match self {
            Self::Unknown => ChatAssetRefusal::UnknownAsset,
            Self::Unreadable { refusal, .. } => refusal,
            Self::Storage(BlobError::Unknown) => ChatAssetRefusal::UnknownAsset,
            Self::Storage(BlobError::Reclaimed) => ChatAssetRefusal::Reclaimed,
            Self::Storage(BlobError::Unauthorized) => ChatAssetRefusal::Unauthorized,
            Self::Storage(_)
            | Self::TooLarge { .. }
            | Self::Transport { .. }
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
    resident: HashMap<RetentionKey, Resident>,
    released: VecDeque<RetentionKey>,
}
impl Retention {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            bytes: 0,
            clock: 0,
            resident: HashMap::new(),
            released: VecDeque::new(),
        }
    }
    fn miss(&self, id: u64, bytes: usize, reason: &'static str) {
        tracing::info!(target: "dekopond::audit", { audit.event = "gateway.asset.retention_miss",
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
    fn evict(&mut self, key: &RetentionKey) {
        match self.remove(key) {
            Ok(()) | Err(BlobError::Reclaimed | BlobError::Capacity) => (),
            Err(error) => {
                tracing::warn!(asset.id = key.1, %error, "could not reclaim evicted asset")
            }
        }
    }
    fn remove(&mut self, key: &RetentionKey) -> Result<(), BlobError> {
        if let Some(entry) = self.resident.get(key) {
            entry.data.reclaim()?;
        }
        if let Some(entry) = self.resident.remove(key) {
            self.bytes -= entry.data.len().max(1);
            drop(entry);
            self.released.push_back(key.clone());
            while self.released.len() > MAX_RELEASE_TOMBSTONES {
                self.released.pop_front();
            }
        }
        Ok(())
    }
    fn admit(&mut self, key: RetentionKey, bytes: &[u8]) -> Result<DiskBlob, BlobError> {
        if bytes.len() > dekopon_model::asset::MAX_ATTACHMENT_BYTES {
            return Err(BlobError::TooLarge);
        }
        self.admit_with(key, bytes.len(), || DiskBlob::from_bytes(bytes))
    }
    fn admit_with(
        &mut self,
        key: RetentionKey,
        len: usize,
        create: impl FnOnce() -> Result<DiskBlob, BlobError>,
    ) -> Result<DiskBlob, BlobError> {
        // If two reads of the same attachment finish downloading back to back, the second keeps the
        // first copy instead of charging the byte budget twice.
        if let Some(entry) = self.resident.get_mut(&key) {
            self.clock += 1;
            entry.used = self.clock;
            return Ok(entry.data.clone());
        }
        self.check_size(key.1, len)?;
        let charge = len.max(1);
        let needed = (self.bytes + charge).saturating_sub(self.budget);
        let mut candidates: Vec<_> = self
            .resident
            .iter()
            .filter(|(_, entry)| !entry.data.is_pinned())
            .map(|(key, entry)| (key.clone(), entry.used, entry.data.len().max(1)))
            .collect();
        candidates.sort_by_key(|(_, used, _)| *used);
        if candidates.iter().map(|(_, _, bytes)| *bytes).sum::<usize>() < needed {
            self.miss(key.1, len, "all-pinned");
            return Err(BlobError::Capacity);
        }
        for (old, _, _) in candidates {
            if self.bytes + charge <= self.budget {
                break;
            }
            self.remove(&old)?;
        }
        // Capacity is reserved under this mutex before the one file creation, so no staging file
        // can escape the configured budget, and a failed construction never increments accounting.
        let data = create().inspect_err(|_error| {
            self.miss(key.1, len, "storage");
        })?;
        self.bytes += charge;
        self.clock += 1;
        self.resident.insert(
            key,
            Resident {
                data: data.clone(),
                used: self.clock,
            },
        );
        Ok(data)
    }
}

impl AssetStore {
    fn get_inventory(&self, access: &AssetAccess) -> Vec<AssetRef> {
        self.assets_for_access(access, Vec::new(), true, Instant::now())
            .inventory
    }
    fn check_size(&self, id: u64, bytes: usize) -> Result<(), BlobError> {
        self.retention
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check_size(id, bytes)
    }
    fn pin(
        &self,
        access: &AssetAccess,
        id: u64,
        touch: bool,
    ) -> Result<Option<DiskBlob>, BlobError> {
        access
            .with_active(|key| {
                let mut entries = self
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Self::expire(&mut entries, self.idle_timeout, Instant::now());
                let asset = entries
                    .get(key)
                    .and_then(|entry| entry.assets.iter().find(|asset| asset.id == id));
                let mut retention = self
                    .retention
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let cache_key = (key.clone(), id);
                let Some(asset) = asset else {
                    let reclaimed = retention.released.contains(&cache_key);
                    retention.miss(id, 0, if reclaimed { "reclaimed" } else { "unknown" });
                    return Err(if reclaimed {
                        BlobError::Reclaimed
                    } else {
                        BlobError::Unknown
                    });
                };
                if touch {
                    retention.clock += 1;
                }
                let clock = retention.clock;
                if let Some(resident) = retention.resident.get_mut(&cache_key) {
                    if touch {
                        resident.used = clock;
                    }
                    return Ok(Some(resident.data.clone()));
                }
                if asset.fetched {
                    retention.miss(id, asset.size.unwrap_or_default() as usize, "reclaimed");
                    Err(BlobError::Reclaimed)
                } else {
                    Ok(None)
                }
            })
            .unwrap_or_else(|| {
                self.retention
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .miss(id, 0, "unauthorized");
                Err(BlobError::Unauthorized)
            })
    }
    fn admit(&self, access: &AssetAccess, id: u64, bytes: &[u8]) -> Result<DiskBlob, BlobError> {
        access
            .with_active(|key| {
                let mut entries = self
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut retention = self
                    .retention
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // The completed download is recorded before any fallible size, cleanup, or
                // admission check, since a successfully downloaded input must never silently be
                // fetched twice.
                let asset = entries
                    .get_mut(key)
                    .and_then(|entry| entry.assets.iter_mut().find(|asset| asset.id == id))
                    .ok_or(BlobError::Unauthorized)?;
                asset.fetched = true;
                asset.size = Some(bytes.len() as u64);
                retention.check_size(id, bytes.len())?;
                Self::expire(&mut entries, self.idle_timeout, Instant::now());
                let stale: Vec<_> = retention
                    .resident
                    .iter()
                    .filter(|((key, id), resident)| {
                        !resident.data.is_pinned()
                            && !entries.get(key).is_some_and(|entry| {
                                entry.assets.iter().any(|asset| asset.id == *id)
                            })
                    })
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in stale {
                    retention.remove(&key)?;
                }
                entries
                    .get(key)
                    .and_then(|entry| entry.assets.iter().find(|asset| asset.id == id))
                    .ok_or(BlobError::Unauthorized)?;
                retention.admit((key.clone(), id), bytes)
            })
            .unwrap_or(Err(BlobError::Unauthorized))
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
        match store.pin(&self.access, self.id, true) {
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
            let mut entries = self.store.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut retention = self.store.retention.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let id = self.access.allocate_id(&self.store.next_one_shot_id);
            let data = retention.admit_with((key.clone(), id), len, || Ok(data))?;
            drop(data);
            let entry = entries.entry(key.clone()).or_insert_with(|| ConversationAssets {
                assets: Vec::new(), touched: Instant::now(), fence: self.access.weak_fence(), delivery_failed: false,
            });
            entry.touched = Instant::now();
            entry.assets.push(AssetRef { id, name: format!("asset-{id}"), mime: metadata.content_type.clone(), size: Some(metadata.bytes),
                source: Some(AssetSourceRef::Generated { capability: capability.to_owned(), invocation: invocation.to_owned() }),
                fetched: true, encoding: metadata.encoding, sent: false,
            });
            while entry.assets.len() > MAX_ASSETS_PER_CONVERSATION {
                let evicted = entry.assets.remove(0);
                retention.evict(&(key.clone(), evicted.id));
            }
            AssetStore::enforce_ceiling(&mut entries, self.store.conversations);
            if let Some(detected) = detected && detected != metadata.content_type {
                let label: String = metadata.content_type.chars().filter(|c| !c.is_control()).take(128).collect();
                tracing::info!(target: "dekopond::audit", { audit.event = "gateway.asset.content_type_mismatch", asset.id = id, asset.content_type = label, asset.detected_type = detected, asset.bytes = metadata.bytes, asset.sha256 = metadata.sha256 }, "declared asset label differs from decoded prefix");
            }
            Ok(id)
        }).unwrap_or(Err(BlobError::Unauthorized))
    }
    fn remove(&self, id: u64) -> Result<(), BlobError> {
        self.access
            .with_active(|key| {
                let mut entries = self
                    .store
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let entry = entries.get_mut(key).ok_or(BlobError::Unknown)?;
                let index = entry
                    .assets
                    .iter()
                    .position(|asset| asset.id == id)
                    .ok_or(BlobError::Unknown)?;
                if entry.assets[index].sent {
                    return Err(BlobError::Unauthorized);
                }
                self.store
                    .retention
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&(key.clone(), id))?;
                entry.assets.remove(index);
                Ok(())
            })
            .unwrap_or(Err(BlobError::Unauthorized))
    }
    fn send(&self, id: u64) -> Result<Option<GeneratedImage>, BlobError> {
        self.access
            .with_active(|key| {
                let mut entries = self
                    .store
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let asset = entries
                    .get_mut(key)
                    .and_then(|entry| entry.assets.iter_mut().find(|asset| asset.id == id))
                    .ok_or(BlobError::Unknown)?;
                if asset.sent {
                    return Ok(None);
                }
                // Mark an asset sent once, even if its retained file becomes unavailable; never
                // retry the send implicitly.
                asset.sent = true;
                let retention = self
                    .store
                    .retention
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let data = retention
                    .resident
                    .get(&(key.clone(), id))
                    .ok_or(BlobError::Reclaimed)?
                    .data
                    .clone();
                Ok(Some(GeneratedImage::new(
                    data,
                    asset.mime.clone(),
                    asset.encoding,
                )))
            })
            .unwrap_or(Err(BlobError::Unauthorized))
    }
    fn delivery_failed(&self) {
        self.access.with_active(|key| {
            if let Some(entry) = self
                .store
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(key)
            {
                entry.delivery_failed = true;
            }
        });
    }
}
impl AssetStore {
    pub fn take_delivery_notice(&self, access: &AssetAccess) -> Option<&'static str> {
        access.with_active(|key| {
            let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
                true,
                Instant::now(),
            )
            .arrived[0]
    }
    fn store(budget: usize) -> Arc<AssetStore> {
        Arc::new(AssetStore::with_retention(
            8,
            Duration::from_secs(600),
            budget,
        ))
    }

    #[test]
    fn lru_use_a_admit_c_evicts_b_and_listing_does_not_touch() {
        let store = store(6);
        let access = access("one");
        let a = register(&store, &access);
        let b = register(&store, &access);
        drop(store.admit(&access, a, b"aaa").unwrap());
        drop(store.admit(&access, b, b"bbb").unwrap());
        drop(store.pin(&access, a, true).unwrap());
        assert_eq!(store.get_inventory(&access).len(), 2);
        let c = register(&store, &access);
        drop(store.admit(&access, c, b"ccc").unwrap());
        assert!(store.pin(&access, a, false).unwrap().is_some());
        assert_eq!(store.pin(&access, b, true), Err(BlobError::Reclaimed));
        assert!(store.pin(&access, c, false).unwrap().is_some());
        assert_eq!(store.retention.lock().unwrap().bytes, 6);
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
            assert_eq!(store.retention.lock().unwrap().bytes, 3);
            register(&store, &access);
            assert_eq!(
                store.get_inventory(&access).len(),
                MAX_ASSETS_PER_CONVERSATION
            );
            assert!(store.get_access(&access, first, Instant::now()).is_none());
            let retained = store.retention.lock().unwrap();
            assert_eq!(retained.resident.len(), usize::from(pinned));
            assert_eq!(retained.bytes, if pinned { 3 } else { 0 });
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
                        pin = store.pin(&access, id, false).unwrap();
                    }
                }
            }
            assert_eq!(
                store.retention.lock().unwrap().resident.len(),
                MAX_ASSETS_PER_CONVERSATION
            );
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
            let retained = store.retention.lock().unwrap();
            let expected = MAX_ASSETS_PER_CONVERSATION + usize::from(pinned);
            assert_eq!(retained.resident.len(), expected);
            assert_eq!(retained.bytes, 3 * expected);
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
        assert_eq!(store.retention.lock().unwrap().bytes, 3);
        drop(pin);
        drop(store.admit(&access, b, b"bbb").unwrap());
        assert_eq!(store.retention.lock().unwrap().bytes, 3);
        let zero = AssetStore::with_retention(8, Duration::from_secs(600), 0);
        let c = register(&zero, &access);
        assert_eq!(zero.admit(&access, c, b""), Err(BlobError::Disabled));
        assert_eq!(zero.retention.lock().unwrap().bytes, 0);
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
        assert_eq!(store.retention.lock().unwrap().bytes, 3);
        assert_eq!(store.pin(&second, a + 10, true), Err(BlobError::Unknown));
    }

    #[test]
    fn tombstones_are_bounded_and_evicted_ids_never_redownload() {
        let store = store(1);
        let access = access("one");
        for _ in 0..MAX_RELEASE_TOMBSTONES + 5 {
            let id = register(&store, &access);
            drop(store.admit(&access, id, b"x").unwrap());
        }
        let cache = store.retention.lock().unwrap();
        assert_eq!(cache.released.len(), MAX_RELEASE_TOMBSTONES);
        assert_eq!(cache.bytes, 1);
        assert_eq!(cache.resident.len(), 1);
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
        let cache = store.retention.lock().unwrap();
        assert_eq!(cache.bytes, 0);
        assert!(cache.resident.is_empty());
    }

    #[test]
    fn retention_miss_is_correlated_and_contains_only_bounded_metadata() {
        use tracing_subscriber::prelude::*;
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let span = tracing::info_span!("gateway.session");
        span.in_scope(|| {
            assert_eq!(Retention::new(3).check_size(7, 4), Err(BlobError::TooLarge));
        });
        assert!(capture.records().iter().any(|record| matches!(record,
            dekopon_test_support::Record::Event { fields, parent: Some(parent), .. }
            if fields.contains("gateway.asset.retention_miss") && parent == "gateway.session")));
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
            true,
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
            let (_, blob) = session.fetch_for_capability(id).unwrap();
            assert_eq!(blob.len(), 3);
            assert_eq!(session.rows()[0].bytes, Some(3));
            drop(blob);
            store
                .retention
                .lock()
                .unwrap()
                .remove(&(access.key.clone(), id))
                .unwrap();
            assert_eq!(
                session.fetch_for_capability(id).unwrap_err(),
                ChatAssetRefusal::Reclaimed
            );
            assert_eq!(session.rows()[0].bytes, Some(3));
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
            let empty_id = registered.arrived[1];
            let empty = store.admit(&access, empty_id, b"").unwrap();
            assert_eq!(empty.len(), 0);
            assert_eq!(session.rows()[1].bytes, Some(0));
            drop(empty);
            store
                .retention
                .lock()
                .unwrap()
                .remove(&(access.key.clone(), empty_id))
                .unwrap();
            assert_eq!(
                session.fetch_for_capability(empty_id).unwrap_err(),
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
                    true,
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
                    session.fetch_for_capability(id).unwrap_err(),
                    ChatAssetRefusal::Reclaimed
                );
                assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
                assert_eq!(session.rows()[0].bytes, Some(3));
                let cache = store.retention.lock().unwrap();
                assert_eq!(cache.bytes, 0);
                assert!(cache.resident.is_empty());
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
            assert!(store.pin(&access, id, false).unwrap().is_none());
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
            .assets_for_access(&access, message.assets, true, Instant::now())
            .arrived[0];
        let session = SessionAssets::new(
            Arc::clone(&store),
            access.clone(),
            transport.asset_fetcher(),
            Handle::current(),
            true,
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
                dekopon_test_support::Record::Span { name: "asset.spool", fields, .. }
                if fields.contains("write"))
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
            assert_eq!(store.retention.lock().unwrap().bytes, PNG.len());
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
            peer.requests.lock().unwrap().len(),
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
            true,
        );
        tokio::task::spawn_blocking(move || {
            let weak = session.fetch(a).unwrap();
            assert_eq!(weak.data.read().unwrap(), b"aaa");
            assert_eq!(
                session.fetch_for_capability(a).unwrap().1.read().unwrap(),
                b"aaa"
            );
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
            let b = register(&store, &access);
            drop(store.admit(&access, b, b"bbb").unwrap());
            assert_eq!(weak.data.read(), Err(BlobError::Reclaimed));
            assert!(session.fetch(a).unwrap_err().contains("released"));
            assert_eq!(
                session.fetch_for_capability(a).unwrap_err(),
                ChatAssetRefusal::Reclaimed
            );
            assert_eq!(fetcher.0.load(Ordering::Relaxed), 1);
            let inputs = ChatAssetInputs::new(Arc::new(session));
            let input = json!([format!("chat-asset:{b}"), format!("chat-asset:{a}")]);
            let original = input.clone();
            assert!(matches!(
                inputs.prepare(&input, 4),
                Err(ChatAssetRefusal::Reclaimed)
            ));
            assert_eq!(input, original, "no partial edit reaches submission");
        })
        .await
        .unwrap();
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
            self.entered.notify_one();
            Box::pin(async {
                let _released = self.release.acquire().await;
                Ok(b"ok".to_vec())
            })
        }
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
            false,
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
        assert!(!session.is_empty());
        assert_eq!(
            session.fetch_for_capability(id).unwrap().1.read().unwrap(),
            png
        );
        assert!(
            matches!(store.get_access(&access, id, Instant::now()).unwrap().source,
            Some(AssetSourceRef::Generated { capability, invocation }) if capability == "image.edit" && invocation == "invocation-2")
        );
        assert_eq!(store.retention.lock().unwrap().bytes, png.len());
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
        assert_eq!(store.retention.lock().unwrap().bytes, 0);
        assert!(session.rows().is_empty());
        let disabled = Arc::new(AssetStore::with_retention(8, Duration::from_secs(600), 0));
        let disabled_session = SessionAssets::new(
            Arc::clone(&disabled),
            access.clone(),
            None,
            Handle::current(),
            true,
            false,
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
        assert_eq!(store.retention.lock().unwrap().bytes, 5 * encoded.len());
        let inputs = ChatAssetInputs::new(Arc::<SessionAssets>::clone(&session));
        let (assets, pins) = inputs.prepare(&json!(references), 4).unwrap();
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
                        session.fetch_for_capability(id).unwrap().1,
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
