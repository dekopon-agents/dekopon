//! The attachments a conversation carries, and the numbers a model refers to them by.
//!
//! An attachment is part of the message that carried it. Chat services deliver it by reference
//! rather than by value, so hearing the whole request means being able to resolve that reference —
//! which is why this lives in the gateway beside transport credentials rather than behind the
//! broker. Nothing
//! here decides *whether* an effect may happen; it reads what a sender already handed the bot on a
//! transport the bot is already authenticated to.
//!
//! What the store holds is **metadata only**. Bytes are fetched when a model asks for them and
//! dropped when the request they joined is built, so a conversation that mentions a screenshot
//! forty turns later costs one small reference line rather than a megabyte of retained image.
//!
//! Numbering is per scope-aware conversation key and monotonic. `Chat Asset #5` is short enough to
//! replay inside the history byte budget, and stable enough that a follow-up three turns later still
//! resolves. The key is exactly the transcript key, so private and shared attachment audiences
//! cannot drift from the reference notes that name them.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dekopon_agent::prompt::{AssetSource, FetchedAsset};
use tokio::runtime::Handle;

use crate::{conversation::ConversationKey, transport::AssetFetcher};

/// Attachments one conversation may accumulate before the oldest are forgotten.
///
/// A ceiling rather than a timer, matching [`crate::conversation::ConversationStore`]: the insert
/// that would exceed it is the one that evicts. Someone who pastes a long screenshot thread keeps
/// the recent ones addressable, which is what a follow-up question is ever about.
const MAX_ASSETS_PER_CONVERSATION: usize = 32;

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
            Self::Slack { file_id, .. } => formatter
                .debug_struct("Slack")
                .field("file_id", file_id)
                .finish_non_exhaustive(),
            Self::Discord { attachment_id, .. } => formatter
                .debug_struct("Discord")
                .field("attachment_id", attachment_id)
                .finish_non_exhaustive(),
            Self::Telegram { file_id } => formatter
                .debug_struct("Telegram")
                .field("file_id", file_id)
                .finish(),
        }
    }
}

/// The attachments of every live conversation, bounded and evicted without a timer.
///
/// Shares [`crate::conversation::ConversationStore`]'s complete non-debug key on purpose, so agent,
/// configured transport, conversation, and private/shared audience boundaries apply identically to
/// transcript and attachment state. It keeps its existing independent idle and LRU bounds.
pub(crate) struct AssetStore {
    conversations: usize,
    idle_timeout: Duration,
    entries: Mutex<HashMap<ConversationKey, ConversationAssets>>,
}

/// One conversation's attachments, and when it last saw one.
struct ConversationAssets {
    /// Oldest first, so eviction is a pop from the front.
    assets: Vec<AssetRef>,
    /// Never reused within a conversation, so a number always means one file.
    next_id: u64,
    touched: Instant,
}

impl AssetStore {
    /// Creates a store tracking at most `conversations` conversations, each idle-expiring after
    /// `idle_timeout`.
    pub fn new(conversations: usize, idle_timeout: Duration) -> Self {
        Self {
            conversations,
            idle_timeout,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Registers what one message carried and returns the references, numbered.
    ///
    /// Takes the transport's own description rather than an [`AssetRef`], because the identifier is
    /// this store's to assign — a transport that numbered its own would collide with the one
    /// beside it.
    pub fn register(
        &self,
        conversation: &ConversationKey,
        arriving: Vec<PendingAsset>,
        now: Instant,
    ) -> Vec<AssetRef> {
        if arriving.is_empty() {
            return Vec::new();
        }
        let mut entries = self.entries.lock().unwrap_or_else(|error| {
            // A poisoned lock means a thread panicked mid-update. The attachments of one
            // conversation are not worth aborting a daemon over, and the map is still coherent.
            error.into_inner()
        });
        Self::expire(&mut entries, self.idle_timeout, now);
        let entry = entries
            .entry(conversation.clone())
            .or_insert_with(|| ConversationAssets {
                assets: Vec::new(),
                next_id: 1,
                touched: now,
            });
        entry.touched = now;
        let mut registered = Vec::with_capacity(arriving.len());
        for pending in arriving {
            let asset = AssetRef {
                id: entry.next_id,
                name: pending.name,
                mime: pending.mime,
                size: pending.size,
                source: pending.source,
            };
            entry.next_id = entry.next_id.saturating_add(1);
            entry.assets.push(asset.clone());
            registered.push(asset);
        }
        while entry.assets.len() > MAX_ASSETS_PER_CONVERSATION {
            entry.assets.remove(0);
        }
        Self::enforce_ceiling(&mut entries, self.conversations);
        registered
    }

    /// Registers what one message carried and reports what a model may be shown.
    ///
    /// The two halves answer different questions, and conflating them was a bug worth naming. The
    /// returned refs are *this message's* attachments, because those are the ones a new reference
    /// note describes. Whether the tool is offered depends on the whole **conversation**: a
    /// follow-up carries no attachment of its own, and gating on that would withdraw the tool
    /// exactly when someone asks a second question about the screenshot they already sent — the
    /// reference line still sitting in replayed history, and nothing able to act on it. A model in
    /// that position answers from the earlier description instead of looking, which reads as
    /// confidently making things up.
    pub fn assets_for(
        &self,
        conversation: &ConversationKey,
        arriving: Vec<PendingAsset>,
        images_supported: bool,
        now: Instant,
    ) -> Registered {
        let arrived = self
            .register(conversation, arriving, now)
            .into_iter()
            .map(|asset| asset.id)
            .collect();
        let inventory = self.inventory(conversation, now);
        let fetchable = inventory
            .iter()
            .any(|asset| asset.is_fetchable(images_supported));
        Registered {
            inventory,
            arrived,
            fetchable,
        }
    }

    /// Every attachment this conversation can still offer, oldest first.
    ///
    /// Touches the entry, so a conversation that keeps talking keeps its attachments addressable.
    fn inventory(&self, conversation: &ConversationKey, now: Instant) -> Vec<AssetRef> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut entries, self.idle_timeout, now);
        let Some(entry) = entries.get_mut(conversation) else {
            return Vec::new();
        };
        entry.touched = now;
        entry.assets.clone()
    }

    /// Looks one attachment up by the number a model named.
    pub fn get(&self, conversation: &ConversationKey, id: u64, now: Instant) -> Option<AssetRef> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut entries, self.idle_timeout, now);
        let entry = entries.get_mut(conversation)?;
        entry.touched = now;
        entry.assets.iter().find(|asset| asset.id == id).cloned()
    }

    /// Drops every conversation idle past the timeout, at the lookup that would have used one.
    fn expire(
        entries: &mut HashMap<ConversationKey, ConversationAssets>,
        idle_timeout: Duration,
        now: Instant,
    ) {
        entries.retain(|_, entry| now.saturating_duration_since(entry.touched) < idle_timeout);
    }

    /// Evicts least recently used conversations down to the ceiling.
    fn enforce_ceiling(
        entries: &mut HashMap<ConversationKey, ConversationAssets>,
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

/// Bytes one session may pull for a single attachment.
///
/// Well under the 50 MB the model APIs accept, because the binding constraint is the prompt rather
/// than the wire: a screenshot near this size already costs more tokens than the conversation
/// around it. A larger file is refused in words the model can pass on, not by failing the session.
const MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;

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
    conversation: ConversationKey,
    fetcher: Option<Arc<dyn AssetFetcher>>,
    runtime: Handle,
    images_supported: bool,
    available: bool,
    spent: Mutex<u32>,
}

impl SessionAssets {
    pub fn new(
        store: Arc<AssetStore>,
        conversation: ConversationKey,
        fetcher: Option<Arc<dyn AssetFetcher>>,
        runtime: Handle,
        images_supported: bool,
        available: bool,
    ) -> Self {
        Self {
            store,
            conversation,
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
        !self.available || self.fetcher.is_none()
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
        let Some(asset) = self.store.get(&self.conversation, id, Instant::now()) else {
            return Err(format!(
                "There is no Chat Asset #{id} in this conversation. The reference lines in the messages above name the ones there are."
            ));
        };
        if !asset.is_fetchable(self.images_supported) {
            return Err(format!(
                "Chat Asset #{id} cannot be opened: {}.",
                asset.unreadable_reason(self.images_supported)
            ));
        }
        if asset.size > MAX_ASSET_BYTES {
            return Err(format!(
                "Chat Asset #{id} is {} which is over the {} the gateway will read.",
                kibibytes(asset.size),
                kibibytes(MAX_ASSET_BYTES)
            ));
        }
        let (Some(fetcher), Some(source)) = (self.fetcher.as_ref(), asset.source.as_ref()) else {
            return Err(format!("Chat Asset #{id} cannot be opened."));
        };
        let data = self
            .runtime
            .block_on(fetcher.fetch(source, MAX_ASSET_BYTES))
            .map_err(|error| {
                // The transport's own category, never its message: a transport error can carry
                // service text, and this string goes into a prompt.
                format!("Chat Asset #{id} could not be read ({}).", error.category())
            })?;
        Ok(FetchedAsset {
            name: asset.name,
            mime: asset.mime,
            data,
        })
    }
}
