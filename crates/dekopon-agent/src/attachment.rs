//! The two places bytes cross between a capability and a chat conversation.
//!
//! Everything on the provider boundary is a JSON string, and the sandboxed shell has no byte type:
//! printing a multi-megabyte base64 blob would clamp it to a screenful of garbage in the model
//! transcript and cost the session the tokens anyway. So bytes never travel *through* the model.
//! Both directions are courier behaviour in the embedding gateway, and neither is authority: the
//! broker still authorizes every invocation, and an owner still decides per route whether either
//! convention is live.
//!
//! - **Out.** A successful capability result may carry a top-level `attachments` key
//!   ([`dekopon_provider_sdk::ResultAttachment`]). The session's broker leg removes it, validates
//!   each entry, puts the accepted bytes in [`ReplyAttachments`] — a request-local slot that is
//!   never a model message — and leaves the model metadata only.
//! - **In.** A capability input may carry the marker `chat-asset:<N>`, naming an attachment the
//!   sender put on their message. For the capabilities a route lists, the leg expands each marker to
//!   a `data:` URL through [`ChatAssetSource`] before the proposal is submitted.

use std::{fmt, sync::Arc, sync::Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use thiserror::Error;

/// Maximum decoded attachment retained in memory or handed to a chat transport.
pub const MAX_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

/// The one media type a delivered attachment may declare.
const ATTACHMENT_MEDIA_TYPE: &str = "image/png";

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Expansions one invocation's input may make.
///
/// Three because that is what a remix of a handful of reference images needs, and because each one
/// is a transport download plus its bytes held in the proposal.
pub const MAX_CHAT_ASSET_INPUTS: usize = 3;

/// Decoded bytes one invocation's expanded markers may carry in total.
///
/// Half a mebibyte under the single-attachment ceiling, so three expansions plus the envelope stay
/// inside the byte bound a broker frame can carry.
pub const MAX_CHAT_ASSET_INPUT_BYTES: usize = 8_912_896;

/// The key a capability result carries attachments under, and which the gateway removes.
const ATTACHMENTS_KEY: &str = "attachments";

/// The key the gateway writes attachment metadata back under.
const ATTACHED_KEY: &str = "attached";

/// The key the gateway writes its own fixed refusal sentence under.
const ATTACHMENT_NOTE_KEY: &str = "attachmentNote";

/// One bounded PNG, held only until the embedding chat transport accepts it.
///
/// `Debug` reports metadata and never bytes. Provider-produced content is untrusted and can be
/// several megabytes; formatting it into a model transcript or telemetry record would be both a data
/// leak and an unbounded operational cost.
pub struct GeneratedImage {
    data: Vec<u8>,
}

impl GeneratedImage {
    /// Validates and owns one bounded PNG.
    ///
    /// # Errors
    ///
    /// Returns [`AttachmentRefusal::TooLarge`] past [`MAX_ATTACHMENT_BYTES`] and
    /// [`AttachmentRefusal::UnsupportedMedia`] when the bytes do not carry the PNG signature,
    /// whatever media type the producer declared.
    pub fn from_png(data: Vec<u8>) -> Result<Self, AttachmentRefusal> {
        if data.len() > MAX_ATTACHMENT_BYTES {
            return Err(AttachmentRefusal::TooLarge);
        }
        if !data.starts_with(PNG_SIGNATURE) {
            return Err(AttachmentRefusal::UnsupportedMedia);
        }
        Ok(Self { data })
    }

    /// IANA media type fixed by validation rather than by what the provider claimed.
    #[must_use]
    pub const fn media_type(&self) -> &'static str {
        ATTACHMENT_MEDIA_TYPE
    }

    /// Gateway-owned filename for this attachment's position in one reply.
    ///
    /// Neither the model nor the provider can choose a path or a service-visible name. The position
    /// is in the name because a reply can carry several attachments and a chat service shows a
    /// person the filename; repeating one name would make two different images look like one file
    /// posted twice.
    #[must_use]
    pub fn filename(&self, index: usize) -> String {
        if index == 0 {
            "generated-image.png".to_owned()
        } else {
            format!("generated-image-{}.png", index + 1)
        }
    }

    /// Raw PNG bytes for the final transport upload.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the image into its raw PNG bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

impl fmt::Debug for GeneratedImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneratedImage")
            .field("media_type", &self.media_type())
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Why one provider-offered attachment was not delivered.
///
/// A refusal never fails the script. The invocation already happened and may already have cost the
/// account money, so the model is told in one fixed sentence that the bytes did not travel and can
/// answer around it. [`Self::reason`] is the stable audit value and [`Self::note`] the sentence.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AttachmentRefusal {
    /// The route does not carry provider attachments at all.
    #[error("this route does not deliver provider attachments")]
    RouteDisabled,
    /// The `attachments` value was not a list of `{mediaType, base64}` objects, or the base64 did
    /// not decode.
    #[error("attachment was not a decodable base64 attachment list")]
    InvalidEncoding,
    /// The declared media type is not deliverable, or the bytes are not what it claimed.
    #[error("attachment media type is not deliverable")]
    UnsupportedMedia,
    /// Decoded bytes exceeded [`MAX_ATTACHMENT_BYTES`].
    #[error("attachment exceeded the byte bound")]
    TooLarge,
    /// The reply already holds as many attachments as the route permits.
    #[error("the reply already holds as many attachments as this route permits")]
    PerReplyLimit,
}

impl AttachmentRefusal {
    /// Stable low-cardinality audit reason.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::RouteDisabled => "route-disabled",
            Self::InvalidEncoding => "invalid-encoding",
            Self::UnsupportedMedia => "unsupported-media",
            Self::TooLarge => "too-large",
            Self::PerReplyLimit => "per-reply-limit",
        }
    }

    /// The fixed gateway-authored sentence the model reads in place of the bytes.
    ///
    /// Fixed text, never a provider diagnostic: a provider message can reflect untrusted upstream
    /// content, and this string goes straight into the next model request.
    #[must_use]
    pub const fn note(&self) -> &'static str {
        match self {
            Self::RouteDisabled => {
                "This conversation cannot carry attachments, so the file this capability produced \
                 was discarded. Answer in text."
            }
            Self::InvalidEncoding => {
                "The gateway could not read the file this capability produced, so it was not \
                 delivered. Answer in text."
            }
            Self::UnsupportedMedia => {
                "The file this capability produced is not a type the gateway delivers, so it was \
                 discarded. Answer in text."
            }
            Self::TooLarge => {
                "The file this capability produced is larger than the gateway delivers, so it was \
                 discarded. Answer in text, or ask for a smaller one."
            }
            Self::PerReplyLimit => {
                "This reply already carries every attachment it is allowed, so the newest file was \
                 discarded. Finish with what is already queued."
            }
        }
    }
}

/// Request-local slot through which validated attachments leave one session.
///
/// The bytes never become a model message, part of a prompt outcome, or part of any conversation
/// history. An embedder takes the slot only after a successful session and drops it on failure or
/// cancellation, which keeps provider-produced content out of transcripts, persistent history, and
/// accidental `Debug` output.
#[derive(Debug)]
pub struct ReplyAttachments {
    max_per_reply: usize,
    images: Mutex<Vec<GeneratedImage>>,
}

impl ReplyAttachments {
    /// A slot for one reply, carrying at most `max_per_reply` attachments.
    #[must_use]
    pub fn new(max_per_reply: u8) -> Self {
        Self {
            max_per_reply: max_per_reply as usize,
            images: Mutex::new(Vec::new()),
        }
    }

    /// Removes everything this session accepted, oldest first.
    pub fn take(&self) -> Vec<GeneratedImage> {
        std::mem::take(
            &mut *self
                .images
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Accepts one validated attachment, or reports the per-reply ceiling.
    ///
    /// The ceiling covers the whole session rather than one invocation: a script that calls the same
    /// capability in a loop must not be able to widen the reply the route configured.
    ///
    /// # Errors
    ///
    /// Returns [`AttachmentRefusal::PerReplyLimit`] once the slot is full.
    pub fn store(&self, image: GeneratedImage) -> Result<(), AttachmentRefusal> {
        let mut images = self
            .images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if images.len() >= self.max_per_reply {
            return Err(AttachmentRefusal::PerReplyLimit);
        }
        images.push(image);
        Ok(())
    }
}

/// Replaces a capability result's `attachments` key with metadata, reporting every refusal.
///
/// Returns the refusals in the order they happened; the caller audits each one. `slot` is `None` for
/// a session whose route delivers no attachments, which still strips the key: the bytes must not
/// reach the shell just because nowhere can accept them.
pub fn strip_attachments(
    output: &mut Value,
    slot: Option<&ReplyAttachments>,
) -> Vec<AttachmentRefusal> {
    let Some(object) = output.as_object_mut() else {
        return Vec::new();
    };
    let Some(offered) = object.remove(ATTACHMENTS_KEY) else {
        return Vec::new();
    };
    let mut refusals = Vec::new();
    let mut accepted = Vec::new();
    match slot {
        None => refusals.push(AttachmentRefusal::RouteDisabled),
        Some(slot) => {
            match serde_json::from_value::<Vec<dekopon_provider_sdk::ResultAttachment>>(offered) {
                // The shape is the provider's claim, not a host contract, so a result that used the
                // reserved key for something else is one refusal rather than a failed invocation.
                Err(_shape) => refusals.push(AttachmentRefusal::InvalidEncoding),
                Ok(attachments) => {
                    for attachment in attachments {
                        match accept(slot, &attachment) {
                            Ok(bytes) => accepted.push(serde_json::json!({
                                "mediaType": ATTACHMENT_MEDIA_TYPE,
                                "bytes": bytes,
                            })),
                            Err(refusal) => refusals.push(refusal),
                        }
                    }
                }
            }
        }
    }
    object.insert(ATTACHED_KEY.to_owned(), Value::Array(accepted));
    if let Some(first) = refusals.first() {
        object.insert(
            ATTACHMENT_NOTE_KEY.to_owned(),
            Value::String(first.note().to_owned()),
        );
    }
    refusals
}

/// Validates one offered attachment and stores it, answering with its delivered byte count.
fn accept(
    slot: &ReplyAttachments,
    attachment: &dekopon_provider_sdk::ResultAttachment,
) -> Result<usize, AttachmentRefusal> {
    if attachment.media_type != ATTACHMENT_MEDIA_TYPE {
        return Err(AttachmentRefusal::UnsupportedMedia);
    }
    // Checked before decoding: base64 costs four characters for every three bytes, so the encoded
    // length already rules an oversized attachment out without allocating its decode.
    if attachment.base64.len() > MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 {
        return Err(AttachmentRefusal::TooLarge);
    }
    #[allow(
        clippy::map_err_ignore,
        reason = "base64 DecodeError adds only an offset and the offending byte inside untrusted \
                  provider bytes; InvalidEncoding already names the failure, and this module never \
                  puts attachment content in a diagnostic"
    )]
    let data = STANDARD
        .decode(&attachment.base64)
        .map_err(|_| AttachmentRefusal::InvalidEncoding)?;
    let bytes = data.len();
    slot.store(GeneratedImage::from_png(data)?)?;
    Ok(bytes)
}

/// Why one `chat-asset:<N>` marker could not be expanded into a capability input.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ChatAssetRefusal {
    /// No attachment in this conversation carries that number.
    #[error("the conversation has no attachment with that number")]
    UnknownAsset,
    /// The attachment is not an image, which is all a capability input may carry.
    #[error("only image attachments may be passed to a capability")]
    UnsupportedMedia,
    /// This invocation already expanded [`MAX_CHAT_ASSET_INPUTS`] markers.
    #[error("one invocation may expand at most {MAX_CHAT_ASSET_INPUTS} chat attachments")]
    PerInvocationLimit,
    /// The expansions together would exceed [`MAX_CHAT_ASSET_INPUT_BYTES`].
    #[error("the expanded attachments exceeded the per-invocation byte budget")]
    ByteBudget,
    /// The gateway could not read the attachment's bytes at all.
    #[error("the attachment's bytes could not be read")]
    Unavailable,
}

impl ChatAssetRefusal {
    /// Stable low-cardinality audit reason.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::UnknownAsset => "unknown-asset",
            Self::UnsupportedMedia => "unsupported-media",
            Self::PerInvocationLimit => "per-invocation-limit",
            Self::ByteBudget => "byte-budget",
            Self::Unavailable => "unavailable",
        }
    }

    /// The fixed gateway-authored sentence the model reads instead of a proposal.
    #[must_use]
    pub const fn note(&self) -> &'static str {
        match self {
            Self::UnknownAsset => {
                "the gateway found no chat attachment with that number; the reference lines in the \
                 conversation name the ones there are"
            }
            Self::UnsupportedMedia => "the gateway passes only image attachments to a capability",
            Self::PerInvocationLimit => {
                "the gateway passes at most three chat attachments to one call; split the work"
            }
            Self::ByteBudget => {
                "the chat attachments named in this call are together too large for one call"
            }
            Self::Unavailable => "the gateway could not read that chat attachment's bytes",
        }
    }
}

/// Where a session's broker leg gets one chat attachment's bytes for a capability input.
///
/// Separate from the model-facing attachment tool on purpose. That tool spends a session budget on
/// showing a *model* a file; this spends a per-invocation budget on handing bytes to an authorized
/// capability, and neither may consume the other's allowance. An embedder that supplies no source
/// expands no marker, which is what leaves `dekopon-run` exactly as capable as before.
pub trait ChatAssetSource: Send + Sync {
    /// Returns one attachment's IANA media type and bytes.
    ///
    /// Named apart from the model-facing attachment tool's own `fetch` because an implementor is
    /// usually the same type serving both, and two methods called `fetch` on one reader would make
    /// every call site ambiguous about which budget it is spending.
    ///
    /// # Errors
    ///
    /// Returns the stable reason the attachment cannot be handed to a capability.
    fn fetch_for_capability(&self, id: u64) -> Result<(String, Vec<u8>), ChatAssetRefusal>;
}

/// One route's chat-asset input expansion: which capabilities opted in, and where bytes come from.
///
/// Owned rather than borrowed because a session's broker leg outlives every statement of the script
/// that drives it, and the leg is what expands a marker.
pub struct ChatAssetInputs {
    source: Arc<dyn ChatAssetSource>,
    capabilities: Vec<String>,
}

impl ChatAssetInputs {
    /// Binds a source to the capability identifiers this route lists.
    ///
    /// A capability absent from `capabilities` has its markers left untouched, which is deliberate:
    /// an unexpanded marker is an ordinary string the provider then rejects as invalid input, and a
    /// gateway must not decide on a provider's behalf that a string beginning `chat-asset:` was
    /// meant as one.
    #[must_use]
    pub fn new(source: Arc<dyn ChatAssetSource>, capabilities: Vec<String>) -> Self {
        Self {
            source,
            capabilities,
        }
    }

    /// Whether this route opted `capability` into marker expansion.
    #[must_use]
    pub fn covers(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|listed| listed == capability)
    }

    /// Expands every marker in one invocation's input, under this invocation's own budget.
    ///
    /// # Errors
    ///
    /// Returns the first refusal. The input is then abandoned rather than half-expanded: a proposal
    /// carrying one of three requested images is not the call the model asked for.
    pub fn expand(&self, input: &mut Value) -> Result<usize, ChatAssetRefusal> {
        let mut budget = ExpansionBudget::default();
        self.walk(input, &mut budget)?;
        Ok(budget.expanded)
    }

    fn walk(
        &self,
        input: &mut Value,
        budget: &mut ExpansionBudget,
    ) -> Result<(), ChatAssetRefusal> {
        match input {
            Value::String(text) => {
                if let Some(id) = chat_asset_marker(text) {
                    *text = self.expanded(id, budget)?;
                }
                Ok(())
            }
            Value::Array(items) => {
                for item in items {
                    self.walk(item, budget)?;
                }
                Ok(())
            }
            Value::Object(fields) => {
                for (_name, value) in fields.iter_mut() {
                    self.walk(value, budget)?;
                }
                Ok(())
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
        }
    }

    fn expanded(&self, id: u64, budget: &mut ExpansionBudget) -> Result<String, ChatAssetRefusal> {
        if budget.expanded >= MAX_CHAT_ASSET_INPUTS {
            return Err(ChatAssetRefusal::PerInvocationLimit);
        }
        let (mime, data) = self.source.fetch_for_capability(id)?;
        if !mime.starts_with("image/") {
            return Err(ChatAssetRefusal::UnsupportedMedia);
        }
        let spent = budget
            .bytes
            .checked_add(data.len())
            .ok_or(ChatAssetRefusal::ByteBudget)?;
        if spent > MAX_CHAT_ASSET_INPUT_BYTES {
            return Err(ChatAssetRefusal::ByteBudget);
        }
        budget.bytes = spent;
        budget.expanded += 1;
        Ok(format!("data:{mime};base64,{}", STANDARD.encode(&data)))
    }
}

/// What one invocation has already spent expanding markers.
#[derive(Default)]
struct ExpansionBudget {
    expanded: usize,
    bytes: usize,
}

/// Reads `chat-asset:<N>` as the attachment number it names.
///
/// An exact match and nothing else. A leading zero, a surrounding sentence, a trailing space, or an
/// empty number is an ordinary string: widening this would let a gateway rewrite text a person
/// actually typed.
fn chat_asset_marker(text: &str) -> Option<u64> {
    let digits = text.strip_prefix("chat-asset:")?;
    if digits.is_empty()
        || digits.starts_with('0')
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    use super::{
        AttachmentRefusal, ChatAssetInputs, ChatAssetRefusal, GeneratedImage, MAX_ATTACHMENT_BYTES,
        MAX_CHAT_ASSET_INPUT_BYTES, ReplyAttachments, chat_asset_marker, strip_attachments,
    };

    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(b"bounded provider bytes");
        bytes
    }

    #[test]
    fn generated_image_debug_never_contains_bytes() {
        let image = GeneratedImage::from_png(png()).expect("valid PNG fixture");
        let debugged = format!("{image:?}");
        assert!(debugged.contains("image/png"), "{debugged}");
        assert!(debugged.contains("30"), "{debugged}");
        assert!(!debugged.contains("bounded provider bytes"), "{debugged}");
    }

    #[test]
    fn a_reply_attachment_is_named_for_its_position() {
        let image = GeneratedImage::from_png(png()).expect("valid PNG fixture");
        assert_eq!(image.filename(0), "generated-image.png");
        assert_eq!(image.filename(1), "generated-image-2.png");
    }

    #[test]
    fn oversized_and_non_png_bytes_are_refused_by_their_own_reasons() {
        assert_eq!(
            GeneratedImage::from_png(vec![0; MAX_ATTACHMENT_BYTES + 1]).expect_err("oversized"),
            AttachmentRefusal::TooLarge
        );
        assert_eq!(
            GeneratedImage::from_png(b"not a png".to_vec()).expect_err("not a PNG"),
            AttachmentRefusal::UnsupportedMedia
        );
    }

    /// The marker is matched exactly. Everything else is text a person may well have typed, and
    /// rewriting it would be the gateway editing a message rather than expanding a reference.
    #[test]
    fn only_an_exact_marker_names_an_attachment() {
        assert_eq!(chat_asset_marker("chat-asset:2"), Some(2));
        assert_eq!(chat_asset_marker("chat-asset:12"), Some(12));
        for text in [
            "chat-asset:0",
            "chat-asset:01",
            "chat-asset:",
            "chat-asset:1 ",
            " chat-asset:1",
            "see chat-asset:1",
            "chat-asset:-1",
            "chat-asset:1.0",
            "chat-asset:one",
        ] {
            assert_eq!(chat_asset_marker(text), None, "{text}");
        }
    }

    struct FixedAssets {
        mime: &'static str,
        bytes: usize,
    }

    impl super::ChatAssetSource for FixedAssets {
        fn fetch_for_capability(&self, id: u64) -> Result<(String, Vec<u8>), ChatAssetRefusal> {
            if id > 8 {
                return Err(ChatAssetRefusal::UnknownAsset);
            }
            Ok((self.mime.to_owned(), vec![b'x'; self.bytes]))
        }
    }

    #[test]
    fn every_marker_in_a_listed_capability_becomes_a_data_url() {
        let source = FixedAssets {
            mime: "image/png",
            bytes: 3,
        };
        let inputs = ChatAssetInputs::new(Arc::new(source), vec!["gpt-image.edit".to_owned()]);
        assert!(inputs.covers("gpt-image.edit"));
        assert!(!inputs.covers("gpt-image.generate"));

        let mut input = json!({
            "prompt": "remix these",
            "images": ["chat-asset:1", "chat-asset:2"],
            "nested": {"reference": "chat-asset:3", "text": "chat-asset:0"}
        });
        assert_eq!(inputs.expand(&mut input).expect("three expansions"), 3);
        assert_eq!(input["images"][0], "data:image/png;base64,eHh4");
        assert_eq!(input["nested"]["reference"], "data:image/png;base64,eHh4");
        assert_eq!(input["nested"]["text"], "chat-asset:0");
        assert_eq!(input["prompt"], "remix these");
    }

    #[test]
    fn each_budget_refuses_with_its_own_reason() {
        let small = || FixedAssets {
            mime: "image/png",
            bytes: 3,
        };
        let listed = || vec!["gpt-image.edit".to_owned()];
        let mut four = json!([
            "chat-asset:1",
            "chat-asset:2",
            "chat-asset:3",
            "chat-asset:4"
        ]);
        assert_eq!(
            ChatAssetInputs::new(Arc::new(small()), listed())
                .expand(&mut four)
                .expect_err("a fourth expansion"),
            ChatAssetRefusal::PerInvocationLimit
        );

        let mut unknown = json!(["chat-asset:99"]);
        assert_eq!(
            ChatAssetInputs::new(Arc::new(small()), listed())
                .expand(&mut unknown)
                .expect_err("no such attachment"),
            ChatAssetRefusal::UnknownAsset
        );

        let document = FixedAssets {
            mime: "application/pdf",
            bytes: 3,
        };
        let mut pdf = json!(["chat-asset:1"]);
        assert_eq!(
            ChatAssetInputs::new(Arc::new(document), listed())
                .expand(&mut pdf)
                .expect_err("not an image"),
            ChatAssetRefusal::UnsupportedMedia
        );

        let large = FixedAssets {
            mime: "image/png",
            bytes: MAX_CHAT_ASSET_INPUT_BYTES / 2 + 1,
        };
        let mut two = json!(["chat-asset:1", "chat-asset:2"]);
        assert_eq!(
            ChatAssetInputs::new(Arc::new(large), listed())
                .expand(&mut two)
                .expect_err("over the byte budget"),
            ChatAssetRefusal::ByteBudget
        );
    }

    #[test]
    fn a_stripped_result_carries_metadata_and_no_base64() {
        let slot = ReplyAttachments::new(2);
        let mut output = json!({
            "attachments": [{"mediaType": "image/png", "base64": STANDARD.encode(png())}],
            "image": {"generationId": "gen-1"}
        });
        assert!(strip_attachments(&mut output, Some(&slot)).is_empty());
        assert_eq!(
            output["attached"],
            json!([{"mediaType": "image/png", "bytes": 30}])
        );
        assert!(output.get("attachments").is_none());
        assert!(output.get("attachmentNote").is_none());
        assert_eq!(output["image"]["generationId"], "gen-1");
        assert_eq!(slot.take().len(), 1);
    }

    #[test]
    fn a_session_without_a_slot_strips_and_refuses_as_route_disabled() {
        let mut output = json!({"attachments": [{"mediaType": "image/png", "base64": "UE5H"}]});
        assert_eq!(
            strip_attachments(&mut output, None),
            vec![AttachmentRefusal::RouteDisabled]
        );
        assert_eq!(output["attached"], json!([]));
        assert_eq!(
            output["attachmentNote"],
            AttachmentRefusal::RouteDisabled.note()
        );
    }

    #[test]
    fn each_attachment_refusal_keeps_its_own_reason() {
        let cases = [
            (json!("not a list"), AttachmentRefusal::InvalidEncoding),
            (
                json!([{"mediaType": "image/png", "base64": "%%%"}]),
                AttachmentRefusal::InvalidEncoding,
            ),
            (
                json!([{"mediaType": "image/jpeg", "base64": "UE5H"}]),
                AttachmentRefusal::UnsupportedMedia,
            ),
            (
                json!([{"mediaType": "image/png", "base64": STANDARD.encode(b"not a png")}]),
                AttachmentRefusal::UnsupportedMedia,
            ),
            (
                json!([{"mediaType": "image/png", "base64": "A".repeat(MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 + 1)}]),
                AttachmentRefusal::TooLarge,
            ),
        ];
        for (offered, expected) in cases {
            let slot = ReplyAttachments::new(1);
            let mut output = json!({"attachments": offered});
            assert_eq!(
                strip_attachments(&mut output, Some(&slot)),
                vec![expected],
                "{expected:?}"
            );
            assert_eq!(output["attached"], json!([]), "{expected:?}");
            assert_eq!(output["attachmentNote"], expected.note(), "{expected:?}");
        }
    }

    /// The ceiling is the session's, not one result's: two results each offering one attachment to
    /// a one-attachment route is exactly the case a per-result check would miss.
    #[test]
    fn the_per_reply_ceiling_holds_across_results() {
        let slot = ReplyAttachments::new(1);
        let encoded = STANDARD.encode(png());
        let mut first = json!({"attachments": [{"mediaType": "image/png", "base64": encoded}]});
        assert!(strip_attachments(&mut first, Some(&slot)).is_empty());
        let mut second = json!({"attachments": [{"mediaType": "image/png", "base64": encoded}]});
        assert_eq!(
            strip_attachments(&mut second, Some(&slot)),
            vec![AttachmentRefusal::PerReplyLimit]
        );
        assert_eq!(second["attached"], json!([]));
        assert_eq!(slot.take().len(), 1);
    }

    /// A result that is not an object, or carries no attachments, is handed on untouched.
    #[test]
    fn a_result_without_attachments_is_left_alone() {
        let slot = ReplyAttachments::new(1);
        for mut output in [json!({"ok": true}), json!("plain text"), json!([1, 2])] {
            let before = output.clone();
            assert!(strip_attachments(&mut output, Some(&slot)).is_empty());
            assert_eq!(output, before);
        }
    }
}
