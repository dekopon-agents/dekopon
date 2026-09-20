//! The attachments a conversation carries, and the numbers a model refers to them by.
//!
//! An attachment is part of the message that carried it. Chat services deliver it by reference
//! rather than by value, so hearing the whole request means being able to resolve that reference —
//! which is why this lives in the gateway beside transport credentials rather than behind the
//! broker. Nothing
//! here decides *whether* an effect may happen; it reads what a sender already handed the bot on a
//! transport the bot is already authenticated to.
//!
//! Inventories and model messages hold metadata/weak resolvers. One process-wide disk LRU owns
//! residency; actual consumers acquire temporary pins. A released input never silently refetches.
//!
//! Numbering is per scope-aware conversation generation and monotonic within that generation.
//! `Chat Asset #5` is short enough to replay inside the history byte budget, and stable enough that
//! a follow-up three turns later still resolves. Persistent assets carry the exact transcript key
//! and its live generation fence, so private/shared audiences and invalidation cannot drift from
//! the reference notes that name them.

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

/// Attachments one conversation may accumulate before the oldest are forgotten.
///
/// A ceiling rather than a timer, matching [`crate::conversation::ConversationStore`]: the insert
/// that would exceed it is the one that evicts. Someone who pastes a long screenshot thread keeps
/// the recent ones addressable, which is what a follow-up question is ever about.
pub(crate) const MAX_ASSETS_PER_CONVERSATION: usize = 32;

/// One attachment, as the gateway knows it before anyone asks for the bytes.
///
/// `Debug` prints no source, because Slack private URLs and Discord signed CDN URLs are
/// capabilities. They are metadata in the payload sense, not the span sense.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct AssetRef {
    /// The number the conversation refers to this by.
    pub id: u64,
    /// The name the sender gave it, which is untrusted text.
    pub name: String,
    /// IANA media type as the transport reported it, also untrusted.
    pub mime: String,
    /// Size the transport reported, used to refuse an oversized fetch before making it.
    pub size: u64,
    /// How the owning transport resolves this back to bytes, when it can.
    ///
    /// `None` for a file the app cannot see — Slack withholds the id and URL when the token lacks
    /// access to it. Such a file is still named for the model, because "there is something here I
    /// cannot open" is a better answer than pretending nothing arrived.
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

/// Where an attachment's bytes come from, in the terms its own transport understands.
#[derive(Clone, Eq, PartialEq)]
pub(crate) enum AssetSourceRef {
    /// Gateway provenance, not provider-supplied identity or a transport download source.
    Generated {
        capability: String,
        invocation: String,
    },
    /// A Slack file, fetched from its private download URL with the bot token.
    Slack {
        /// Slack's own file identifier, which is safe to log.
        file_id: String,
        /// The private download URL, which is not.
        url: String,
    },
    /// A Discord attachment, fetched from its signed CDN URL without the bot token.
    Discord {
        /// Discord's snowflake attachment identifier, which is safe to log.
        attachment_id: String,
        /// Channel containing the source message, used to refresh an expired signed URL.
        channel_id: String,
        /// Source message containing the attachment, also used only for URL refresh.
        message_id: String,
        /// The signed CDN URL, which is not logged and is fetched only from an allowed host.
        url: String,
    },
    /// A WhatsApp image, resolved lazily under the owning phone number.
    WhatsApp { media_id: String, mime: String },
    /// A Telegram file, which is a handle rather than a URL.
    ///
    /// The Bot API hands out a `file_id` and nothing else; resolving it to a path takes a `getFile`
    /// call, and the path is only valid for about an hour. So unlike Slack there is no URL to carry
    /// here — the round trip happens at fetch time, which is also when the path is freshest.
    Telegram {
        /// The opaque handle Telegram gave this file.
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

/// One persistent transcript generation's attachment-access fence.
///
/// Conversation invalidation closes the fence under `gate`. Asset publication and lookup hold the
/// same gate through their store operation, which gives replacement a linear boundary: an asset
/// operation either finishes before invalidation or observes the retired generation afterwards.
/// Neither the fence nor the access token implements `Debug`, because its storage key contains the
/// same sensitive identifiers as [`ConversationKey`].
pub(crate) struct AssetFence {
    gate: Mutex<()>,
    active: AtomicBool,
    /// Survives independent asset TTL/LRU removal while this transcript generation stays live.
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

    /// Retires this generation and waits for an already-started asset operation to finish.
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

/// Complete key for one attachment inventory.
///
/// `None` preserves the independently bounded one-shot inventory. A persistent generation is
/// globally non-reused for the life of the paired conversation and asset stores, so a number from
/// a retired generation cannot alias the same number minted by its replacement.
#[derive(Clone, Eq, Hash, PartialEq)]
struct AssetStateKey {
    conversation: ConversationKey,
    generation: Option<u64>,
}

/// Request-local authority to publish and look up attachment metadata in one state generation.
///
/// This is not broker authority and carries no bytes. Persistent access is valid only while the
/// conversation store keeps its generation live; one-shot access keeps the pre-existing TTL/LRU
/// behavior because there is no transcript generation to follow.
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

    /// Runs one complete store operation while this generation is still current.
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

/// The attachments of every live conversation, bounded and evicted without a timer.
///
/// Persistent entries share [`crate::conversation::ConversationStore`]'s complete non-debug key
/// and generation fence, so agent, configured transport, conversation, private/shared audience,
/// and invalidation boundaries apply identically to transcript and attachment state. One-shot
/// entries retain the independent idle/LRU lifetime they had before persistent history existed.
pub(crate) struct AssetStore {
    conversations: usize,
    idle_timeout: Duration,
    entries: Mutex<HashMap<AssetStateKey, ConversationAssets>>,
    retention: Mutex<Retention>,
    downloads: Mutex<()>,
    next_one_shot_id: AtomicU64,
}

/// One conversation generation's attachments, and when it last saw one.
struct ConversationAssets {
    /// Oldest first, so eviction is a pop from the front.
    assets: Vec<AssetRef>,
    touched: Instant,
    /// Absent for one-shot state; weak so this map cannot keep a retired generation live.
    fence: Option<Weak<AssetFence>>,
    delivery_failed: bool,
}

impl AssetStore {
    /// Creates a store tracking at most `conversations` conversations, each idle-expiring after
    /// `idle_timeout`.
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
            downloads: Mutex::new(()),
            next_one_shot_id: AtomicU64::new(1),
        }
    }

    /// Registers what one message carried and reports what a one-shot model may be shown.
    ///
    /// One-shot routes have no transcript generation. Their attachment state keeps its historical
    /// private-keyed TTL/LRU behavior; persistent sessions must use [`Self::assets_for_access`].
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

    /// Registers and inventories assets only if this conversation generation is still live.
    ///
    /// Registration and inventory share one fence hold. A grant/idle/capacity replacement cannot
    /// land between them, and a stale session cannot publish into the replacement's independently
    /// numbered inventory. Whether the tool is offered depends on the whole live generation: a
    /// follow-up carries no attachment of its own, while its replayed history can still name an
    /// earlier one.
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
                        entry.assets.remove(0);
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

    /// Looks one attachment up in independently bounded one-shot state.
    #[cfg(test)]
    pub fn get(&self, conversation: &ConversationKey, id: u64, now: Instant) -> Option<AssetRef> {
        self.get_access(&AssetAccess::one_shot(conversation.clone()), id, now)
    }

    /// Looks one attachment up only while its exact conversation generation remains live.
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

    /// Drops idle and retired generations at the next attachment-store operation.
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

    /// Evicts least recently used attachment inventories down to the independent ceiling.
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
    /// Counts, never contents — the same rule [`AssetRef`] follows.
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

/// One attachment as its transport found it, before the store assigns a number.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingAsset {
    /// Sender-supplied file name.
    pub name: String,
    /// Sender-supplied media type.
    pub mime: String,
    /// Size the transport reported.
    pub size: u64,
    /// How to turn this back into bytes, when the transport could say.
    pub source: Option<AssetSourceRef>,
}

/// Media types a model can be shown as an image.
///
/// The intersection of what a chat service will deliver and what the model APIs accept. A chat
/// service imposes no allowlist on uploads at all — a 700 MB screen recording is a legal
/// attachment — so the narrow end of that intersection is the one worth enforcing.
const READABLE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Media types a model can be handed as a document.
///
/// The API's own `input_file` list. Spreadsheets and presentations are parsed server-side rather
/// than rendered, and a spreadsheet is read only to its first thousand rows per sheet — worth
/// knowing before concluding a model ignored the bottom of one.
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

/// Whether an attachment is one a model can be shown at all.
pub(crate) fn is_readable(mime: &str) -> bool {
    is_image(mime) || READABLE_DOCUMENT_TYPES.contains(&mime)
}

/// Whether an attachment is an image, which is the half a model needs a vision modality for.
pub(crate) fn is_image(mime: &str) -> bool {
    READABLE_IMAGE_TYPES.contains(&mime)
}

/// The lines appended to a prompt naming what this conversation carries.
///
/// The **whole inventory**, not just what this message brought. A reference line is the only way a
/// model learns a number exists, and it used to live solely in the turn that introduced it — so
/// once ordinary chatter pushed that turn out of the replayed history window, the file became
/// unreachable while the store still held it for another hour. The model would answer that it had
/// never been sent a PDF, which was true of the prompt it could see and false of the conversation.
///
/// Repeating the list costs one short line per attachment, bounded by the per-conversation
/// ceiling, and it lands with the newest message rather than in the cached prefix.
pub(crate) fn reference_note(registered: &Registered, images_supported: bool) -> Option<String> {
    if registered.inventory.is_empty() {
        return None;
    }
    let mut note = String::from("[gateway: files in this conversation");
    let mut any_fetchable = false;
    for asset in &registered.inventory {
        let size = kibibytes(asset.size);
        let name = &asset.name;
        let mime = &asset.mime;
        // Marked so a model asking "is this a good recipe?" reaches for the file that arrived with
        // the question rather than one from twenty messages ago.
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
    /// Whether a model could actually be shown this one.
    ///
    /// Only an image needs the vision modality. A document is text or a parsed attachment to every
    /// endpoint that accepts one at all, so gating it on the image modality would refuse a PDF to a
    /// model perfectly able to read it.
    pub fn is_fetchable(&self, images_supported: bool) -> bool {
        self.source.is_some()
            && is_readable(&self.mime)
            && (images_supported || !is_image(&self.mime))
    }

    /// Why this one cannot be shown, in words a model can repeat to the sender.
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

/// Renders a byte count the way a person reads one.
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

/// What this conversation can offer a model, after one message's attachments joined it.
pub(crate) struct Registered {
    /// Every attachment the conversation still holds, oldest first.
    pub inventory: Vec<AssetRef>,
    /// The identifiers that arrived on *this* message, so the note can say which are new.
    pub arrived: Vec<u64>,
    /// Whether at least one of them could actually be fetched, which is what decides if the tool
    /// is offered at all.
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

/// Bytes one session may pull for a single attachment.
///
/// Well under the 50 MB the model APIs accept, because the binding constraint is the prompt rather
/// than the wire: a screenshot near this size already costs more tokens than the conversation
/// around it. A larger file is refused in words the model can pass on, not by failing the session.
const MAX_ASSET_BYTES: u64 = dekopon_model::asset::MAX_ATTACHMENT_BYTES as u64;

/// Attachments one session may pull, however many turns it takes.
///
/// A model that decides to look at everything should still be answering a question rather than
/// touring the conversation's history, and each fetch is a round trip plus a re-encoded prompt.
const MAX_FETCHES_PER_SESSION: u32 = 4;

/// One session's view of the attachments it may show its model.
///
/// Implements [`dekopon_agent::prompt::AssetSource`], whose `fetch` is synchronous because the
/// prompt loop is. The loop runs on a blocking task, so blocking on the download here parks a
/// blocking thread rather than a runtime worker — the same reason the loop is on one at all.
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
        // Every arm returns words rather than an error. A model that asked for the wrong number,
        // or for something too large, can say so and carry on answering; ending the session would
        // turn a recoverable turn into the fixed failure line.
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

/// Pins the files referenced by a proposal in this conversation generation.
///
/// Deliberately a second entry point rather than a second caller of [`AssetSource::fetch`]. That
/// budget is about how many files one *model* may look at; this one is about how much a single
/// invocation may carry, which `dekopon-agent` counts per invocation. Spending one from the other
/// would let a remix exhaust the model's ability to read its own conversation, or the reverse.
///
/// `images_supported` is deliberately not consulted: whether the route's chat model can be shown an
/// image says nothing about whether a capability can be handed a referenced file.
impl ChatAssetSource for SessionAssets {
    fn fetch_for_capability(
        &self,
        id: u64,
    ) -> Result<(String, dekopon_model::asset::DiskBlob), ChatAssetRefusal> {
        let (asset, _resolution_pin) = self
            .load(id, AssetConsumer::Capability)
            .map_err(AssetFailure::for_capability)?;
        // Capability resolution is actual use, not inventory replay. Keep the initial pin until
        // the scoped recency update completes; never substitute bytes if that resolution fails.
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
    /// Reads one attachment's bytes, or which check refused it.
    ///
    /// The one definition both entry points share, so the model-facing wording and the
    /// capability-facing refusal reason can never disagree about what is readable.
    fn load(&self, id: u64, consumer: AssetConsumer) -> Result<(AssetRef, DiskBlob), AssetFailure> {
        // Serialize first fetch/publication: concurrent references must not redownload the same file.
        let _download = self
            .store
            .downloads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(asset) = self.store.get_access(&self.access, id, Instant::now()) else {
            // Pin lookup emits the correlated unknown/reclaimed/unauthorized distinction.
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
                // A file the app cannot see at all and a file of the wrong type are one message to
                // a model and two different answers to a capability, so the distinction is recorded
                // here rather than re-derived from the prose above.
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
            // Retained descriptors were checked at intake by decoded length. Their stored
            // base64 size may exceed the raw download ceiling.
            return Ok((asset, data));
        }
        if asset.size > MAX_ASSET_BYTES {
            return Err(AssetFailure::TooLarge { size: asset.size });
        }
        // Zero and individually impossible admissions do not spend transport IO.
        self.store
            .check_size(id, asset.size as usize)
            .map_err(AssetFailure::Storage)?;
        let (Some(fetcher), Some(source)) = (self.fetcher.as_ref(), asset.source.as_ref()) else {
            return Err(AssetFailure::Unavailable);
        };
        let data = self
            .runtime
            .block_on(fetcher.fetch(source, MAX_ASSET_BYTES))
            .map_err(|error| AssetFailure::Transport {
                // The transport's own category, never its message: a transport error can carry
                // service text, and this string goes into a prompt.
                category: error.category(),
            })?;
        // A generation can be retired while a transport read is in flight. The read cannot always
        // be cancelled, but its bytes must not enter the model after the retirement became visible.
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

/// Which check refused one attachment read, before it is rendered for its audience.
enum AssetFailure {
    Storage(dekopon_model::asset::BlobError),
    /// No such number in this conversation, or its generation was retired underneath the read.
    Unknown,
    /// The gateway will not show this one: why, in words, and which refusal a capability reads.
    Unreadable {
        reason: &'static str,
        refusal: ChatAssetRefusal,
    },
    /// Larger than the gateway reads.
    TooLarge {
        size: u64,
    },
    /// Nothing can resolve it back to bytes.
    Unavailable,
    /// The transport refused or failed the read.
    Transport {
        category: &'static str,
    },
}

impl AssetFailure {
    /// Words a model can repeat to the sender.
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

    /// The stable reason `dekopon-agent` audits when a capability input reference cannot be resolved.
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

// Residency has exactly one owner: this process's AssetStore. Inventory entries and model
// messages carry metadata/resolvers only. A consumer's DiskBlob clone is a temporary pin.
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
    fn remove(&mut self, key: &RetentionKey) -> Result<(), BlobError> {
        if let Some(entry) = self.resident.get(key) {
            entry.data.reclaim()?;
        }
        if let Some(entry) = self.resident.remove(key) {
            self.bytes -= entry.data.len().max(1);
            drop(entry); // descriptor cleanup occurs on the blocking consumer, never a listing
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
        // Capacity is reserved by this mutex before the only file creation. No staging file
        // escapes the configured budget, and failed construction never increments accounting.
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
                    retention.miss(id, asset.size as usize, "reclaimed");
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
                // Record the completed download before any fallible size, cleanup or admission
                // check: successfully downloaded inputs must never silently fetch twice.
                entries
                    .get_mut(key)
                    .and_then(|entry| entry.assets.iter_mut().find(|asset| asset.id == id))
                    .ok_or(BlobError::Unauthorized)?
                    .fetched = true;
                retention.check_size(id, bytes.len())?;
                // Retired/expired inventories lose cache residency, but active pins stay charged until
                // a later blocking admission can safely dispose their sole remaining cache owner.
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
                let asset = entries
                    .get_mut(key)
                    .and_then(|entry| entry.assets.iter_mut().find(|asset| asset.id == id))
                    .ok_or(BlobError::Unauthorized)?;
                let data = retention.admit((key.clone(), id), bytes)?;
                asset.size = data.len() as u64;
                Ok(data)
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
        // Sniff only a decoded prefix. The declared label remains authoritative even on mismatch.
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
            entry.assets.push(AssetRef { id, name: format!("asset-{id}"), mime: metadata.content_type.clone(), size: metadata.bytes,
                source: Some(AssetSourceRef::Generated { capability: capability.to_owned(), invocation: invocation.to_owned() }),
                fetched: true, encoding: metadata.encoding, sent: false,
            });
            while entry.assets.len() > MAX_ASSETS_PER_CONVERSATION { entry.assets.remove(0); }
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
                // Mark once even when the retained file has become unavailable: never retry implicitly.
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

// Twelve decoded bytes identify supported image signatures; no whole-file decode at intake.
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
                    size: 3,
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
    async fn downloaded_oversize_with_unknown_or_underreported_size_never_redownloads() {
        for reported_size in [0, 1] {
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
        let id = register(&store, &access); // Reported three bytes cannot fit the two-byte budget.
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
        let slot = ReplyAttachments::new(MAX_SENDS_PER_TURN, session.clone(), "local".to_owned());
        assert!(slot.take().is_empty(), "attach never sends");
        slot.receive(vec![], vec![], vec![], vec![id], "asset.send", "send-1");
        assert_eq!(slot.remaining(), 3);
        assert_eq!(slot.take().pop().unwrap().bytes().unwrap(), png);
        slot.finish(dekopon_agent::attachment::AssetDeliveryDisposition::Delivered);
        let next = ReplyAttachments::new(MAX_SENDS_PER_TURN, session.clone(), "local".to_owned());
        next.receive(vec![], vec![], vec![], vec![id], "asset.send", "send-2");
        assert_eq!(next.remaining(), 4, "duplicate in next turn costs nothing");
        assert!(next.take().is_empty());
        assert_eq!(session.remove(id), Err(BlobError::Unauthorized));
    }
    #[tokio::test]
    async fn pathless_removal_closes_residency_and_disabled_intake_publishes_no_id() {
        let store = store(3);
        let access = access("one");
        let session = session(store.clone(), access.clone());
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
        let session = session(store.clone(), access("encoded-limits"));
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
        let inputs = ChatAssetInputs::new(session.clone());
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
        let slot = ReplyAttachments::new(MAX_SENDS_PER_TURN, session.clone(), "local".to_owned());
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
        let slot = ReplyAttachments::new(MAX_SENDS_PER_TURN, session.clone(), "local".to_owned());
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
            let session = session(store.clone(), access.clone());
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
