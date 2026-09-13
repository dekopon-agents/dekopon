//! H: conversations — where a message was posted, and the selector routes and grants share.
//!
//! One vocabulary for the whole chain. A transport mints a [`Conversation`] from the authenticated
//! envelope in the exact form a grant and a Cedar policy compare against, so no layer re-normalizes
//! and no layer can accept a shape another rejects. A [`ConversationMatch`] is the owner-authored
//! selector over those values, and it is the same type in a gateway route and in an attestor grant:
//! an operator who can write one can read the other.
//!
//! Nothing here decides authority. [`Conversation::is_canonical_for`] is structural — it consults no
//! grant — and [`ConversationMatch::matches`] answers only "does this selector name this
//! conversation".

use std::fmt;

use dekopon_core::{ExternalSubject, SubjectService};
use serde::{Deserialize, Serialize};

use crate::ChatTransportKind;

/// Where a message was posted, which is who can see the answer.
///
/// Kind is where the message was *posted*; [`Conversation::thread`] is where the answer *lands*.
/// A thread rooted in a direct message stays [`Self::DirectMessage`] with the thread coordinate
/// set, because Slack's Agent experience roots a thread on every DM turn and a `Thread` kind there
/// would make direct messages unmatchable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationKind {
    /// One human and the bot. Addressed by definition. A thread rooted here stays `DirectMessage`.
    DirectMessage,
    /// Several humans and the bot with no container membership behind it (Slack `mpim`, a
    /// WhatsApp group). Ambient: the bot must be addressed.
    GroupDirectMessage,
    /// A container's channel: anyone the container admits can read it.
    Channel,
    /// A thread, forum post, or topic whose parent is a channel or group DM. `id` is the parent.
    Thread,
}

impl ConversationKind {
    /// The one spelling of this kind: the YAML word, the Cedar string, and the trace attribute.
    ///
    /// One constant renders all three, so an operator reading `context.conversation.kind` in a
    /// policy and `kind: [channel, thread]` in a route file is reading the same fact spelled the
    /// same way. A test pins it against the serde rendering.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectMessage => "directMessage",
            Self::GroupDirectMessage => "groupDirectMessage",
            Self::Channel => "channel",
            Self::Thread => "thread",
        }
    }
}

impl fmt::Display for ConversationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where one answer will be posted.
///
/// Canonical at birth: the transport mints it in the exact form grants and Cedar compare against,
/// so no layer re-normalizes. Every part comes from the authenticated envelope and none of it is
/// model-controlled.
#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Conversation {
    /// Where the message was posted.
    pub kind: ConversationKind,
    /// Slack team id (lowercase), Discord guild id, WhatsApp `waba:phoneNumberId`; absent on
    /// Telegram, on Discord direct messages, and optional on the local transport.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_part",
        skip_serializing_if = "Option::is_none"
    )]
    pub container: Option<String>,
    /// The DM, group, or channel the service names; for kind `thread`, the *parent* channel.
    #[serde(deserialize_with = "deserialize_part")]
    pub id: String,
    /// The thread the answer joins: Slack root `ts`, Discord thread channel id, Telegram topic id.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_part",
        skip_serializing_if = "Option::is_none"
    )]
    pub thread: Option<String>,
}

impl fmt::Debug for Conversation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Conversation([REDACTED])")
    }
}

impl Conversation {
    /// Stable identity of the exchange, unique within its transport: `id` or `id:thread`.
    ///
    /// The value that fills every `conversation_id` slot — the cancel request, the active-session
    /// registry, the admission key, the gateway's `ConversationKey`, and the storage grant's
    /// `conversation` namespace — so those keys are one value rather than four derivations that
    /// have to agree.
    #[must_use]
    pub fn key(&self) -> String {
        match &self.thread {
            Some(thread) => format!("{}:{thread}", self.id),
            None => self.id.clone(),
        }
    }

    /// The channel a service API is called with.
    ///
    /// Takes the transport kind because a thread is a different *kind of thing* per service: a
    /// Discord thread is itself a channel, so REST addresses it directly, while a Slack thread is
    /// a coordinate inside a channel and Slack's API takes the channel with the `thread_ts` beside
    /// it. Deriving that from the value alone is impossible, and guessing it is how a reply lands
    /// in the wrong place.
    #[must_use]
    pub fn api_channel(&self, kind: ChatTransportKind) -> &str {
        match (kind, &self.thread) {
            (ChatTransportKind::Discord, Some(thread)) => thread.as_str(),
            _ => self.id.as_str(),
        }
    }

    /// Defensive wire bounds on every part, common to every service-specific canonical form.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        bounded_scope_part(&self.id)
            && self.container.as_deref().is_none_or(bounded_scope_part)
            && self.thread.as_deref().is_none_or(bounded_scope_part)
    }

    /// Whether this is a form the claimed transport actually mints, for this authenticated sender.
    ///
    /// The one definition of the conversation grammar: [`ConversationMatch::validate`], the
    /// broker's grant validation, its claim validation, and
    /// [`DeliveryIdentity::is_canonical_for`](crate::DeliveryIdentity::is_canonical_for) all decide
    /// the same shapes here, so no layer can accept a conversation another rejects. Structural plus
    /// the direct-message subject correlation the two single-sender transports have — a Telegram or
    /// WhatsApp direct message whose id is not the sender is not a conversation that transport
    /// produces — and it fails closed on any value outside the wire bounds.
    ///
    /// Slack identifiers are checked as lowercase tokens only. The leading letter is never checked:
    /// multi-person DMs are `G…` on older workspaces and `C…` on newer ones, so a prefix rule would
    /// refuse real traffic.
    #[must_use]
    pub fn is_canonical_for(&self, kind: ChatTransportKind, subject: &ExternalSubject) -> bool {
        if !self.is_bounded() || !kind.produces(self.kind) {
            return false;
        }
        let container = self.container.as_deref();
        let thread = self.thread.as_deref();
        match kind {
            ChatTransportKind::Slack => {
                subject.service() == SubjectService::Slack
                    && container.is_some_and(lowercase_token)
                    && lowercase_token(&self.id)
                    && match self.kind {
                        // A classic direct message with no thread is the one Slack shape that
                        // carries no answer thread; everything else answers in one.
                        ConversationKind::DirectMessage => {
                            thread.is_none_or(canonical_slack_timestamp)
                        }
                        _ => thread.is_some_and(canonical_slack_timestamp),
                    }
            }
            ChatTransportKind::Discord => {
                subject.service() == SubjectService::Discord
                    && canonical_unsigned_decimal(&self.id)
                    && match self.kind {
                        ConversationKind::DirectMessage => container.is_none() && thread.is_none(),
                        ConversationKind::Channel => {
                            container.is_some_and(canonical_unsigned_decimal) && thread.is_none()
                        }
                        ConversationKind::Thread => {
                            container.is_some_and(canonical_unsigned_decimal)
                                && thread.is_some_and(canonical_unsigned_decimal)
                        }
                        ConversationKind::GroupDirectMessage => false,
                    }
            }
            ChatTransportKind::Telegram => {
                subject.service() == SubjectService::Telegram
                    && container.is_none()
                    && thread.is_none_or(canonical_positive_service_decimal)
                    && match self.kind {
                        // The private chat id *is* the sender's user id, so a direct message
                        // claiming another chat is claiming somebody else's conversation.
                        ConversationKind::DirectMessage => {
                            canonical_positive_service_decimal(&self.id)
                                && self.id == subject.subject()
                        }
                        ConversationKind::Channel => canonical_negative_decimal(&self.id),
                        ConversationKind::Thread => {
                            canonical_negative_decimal(&self.id) && thread.is_some()
                        }
                        ConversationKind::GroupDirectMessage => false,
                    }
            }
            ChatTransportKind::Whatsapp => {
                subject.service() == SubjectService::Whatsapp
                    && self.kind == ConversationKind::DirectMessage
                    && container.is_some_and(canonical_whatsapp_container)
                    && canonical_meta_decimal(&self.id)
                    // The sender is the conversation: there is nothing else a WhatsApp individual
                    // message can be addressed to.
                    && self.id == subject.subject()
                    && thread.is_none()
            }
            ChatTransportKind::Local => {
                container.is_none_or(lowercase_scope_value)
                    && lowercase_scope_value(&self.id)
                    && thread.is_none()
            }
        }
    }
}

impl ChatTransportKind {
    /// Whether this transport ever mints a conversation of that kind.
    ///
    /// Owner-facing: a route or a grant naming a kind its transport never produces is a startup
    /// refusal rather than a line that silently matches nothing.
    #[must_use]
    pub const fn produces(self, kind: ConversationKind) -> bool {
        match (self, kind) {
            // Slack is the only service with every shape: `im`, `mpim`, channels and groups, and
            // threads under any of them.
            (Self::Slack, _) | (Self::Local, _) => true,
            (
                Self::Discord,
                ConversationKind::DirectMessage
                | ConversationKind::Channel
                | ConversationKind::Thread,
            ) => true,
            (
                Self::Telegram,
                ConversationKind::DirectMessage
                | ConversationKind::Channel
                | ConversationKind::Thread,
            ) => true,
            // Group messaging is not on the Cloud API's individual-message path, so the gateway
            // drops group payloads rather than half-answering them.
            (Self::Whatsapp, ConversationKind::DirectMessage) => true,
            (Self::Discord | Self::Telegram, ConversationKind::GroupDirectMessage)
            | (
                Self::Whatsapp,
                ConversationKind::GroupDirectMessage
                | ConversationKind::Channel
                | ConversationKind::Thread,
            ) => false,
        }
    }

    /// Whether this transport's conversations carry a container at all.
    ///
    /// Telegram has no workspace above a chat, so a `container:` on a Telegram selector is a
    /// refusal naming the field rather than a line that matches nothing.
    #[must_use]
    pub const fn has_container(self) -> bool {
        !matches!(self, Self::Telegram)
    }
}

/// Either the word `any` or an explicit, non-empty list of kinds.
///
/// Spelled out rather than defaulted: `kind: any` is a deliberate sentence an operator writes, and
/// a selector with no kind at all would be a catch-all nobody chose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationKindMatch {
    /// Every kind the transport produces.
    Any,
    /// Exactly these kinds. `[channel]` excludes threads; `[channel, thread]` claims both.
    Kinds(Vec<ConversationKind>),
}

impl ConversationKindMatch {
    /// Whether this selector names that kind.
    #[must_use]
    pub fn contains(&self, kind: ConversationKind) -> bool {
        match self {
            Self::Any => true,
            Self::Kinds(kinds) => kinds.contains(&kind),
        }
    }
}

impl Serialize for ConversationKindMatch {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Any => serializer.serialize_str("any"),
            Self::Kinds(kinds) => kinds.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ConversationKindMatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct KindMatch;

        impl<'de> serde::de::Visitor<'de> for KindMatch {
            type Value = ConversationKindMatch;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("the word `any` or a list of conversation kinds")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == "any" {
                    return Ok(ConversationKindMatch::Any);
                }
                // A bare kind word is the mistake worth naming: `kind: channel` reads as though it
                // claimed the channel *and* its threads, and silently claims neither thread.
                Err(E::custom(format!(
                    "`kind` is the word `any` or a list, so write `kind: [{value}]` rather than \
                     `kind: {value}`"
                )))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut kinds = Vec::new();
                while let Some(kind) = sequence.next_element::<ConversationKind>()? {
                    kinds.push(kind);
                }
                Ok(ConversationKindMatch::Kinds(kinds))
            }
        }

        deserializer.deserialize_any(KindMatch)
    }
}

/// One owner-authored selector, shared by gateway routes and attestor grants.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConversationMatch {
    /// Required. The word `any`, or a non-empty list of kinds.
    pub kind: ConversationKindMatch,
    /// One container, or every container when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// These ids only (the parent id for threads), or every id when absent. Empty is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
}

impl ConversationMatch {
    /// Whether this selector claims that conversation: kind, container, and id.
    ///
    /// The thread coordinate is never matched. A thread's parent is its `id`, so a selector naming
    /// the parent claims the threads under it exactly when its kind list says `thread`; naming an
    /// individual thread is what grants deliberately cannot do.
    #[must_use]
    pub fn matches(&self, conversation: &Conversation) -> bool {
        self.kind.contains(conversation.kind)
            && self
                .container
                .as_deref()
                .is_none_or(|container| conversation.container.as_deref() == Some(container))
            && self
                .ids
                .as_ref()
                .is_none_or(|ids| ids.iter().any(|id| id == &conversation.id))
    }

    /// Every problem at once, for a transport of this kind.
    ///
    /// Every one rather than the first: a selector with an empty kind list and two unusable ids is
    /// one refusal naming all three, not three restarts.
    #[must_use]
    pub fn validate(&self, transport: ChatTransportKind) -> Vec<ConversationMatchProblem> {
        let mut problems = Vec::new();
        if let ConversationKindMatch::Kinds(kinds) = &self.kind {
            if kinds.is_empty() {
                problems.push(ConversationMatchProblem::EmptyKindList);
            }
            for (index, kind) in kinds.iter().enumerate() {
                if kinds[..index].contains(kind) {
                    problems.push(ConversationMatchProblem::DuplicateKind { kind: *kind });
                }
                if !transport.produces(*kind) {
                    problems.push(ConversationMatchProblem::ImpossibleKind {
                        kind: *kind,
                        transport,
                    });
                }
            }
        }
        if let Some(container) = &self.container {
            if transport.has_container() {
                if !canonical_container_for(transport, container) {
                    problems.push(ConversationMatchProblem::NonCanonicalContainer {
                        container: container.clone(),
                    });
                }
            } else {
                problems.push(ConversationMatchProblem::ContainerNotSupported { transport });
            }
        }
        match &self.ids {
            Some(ids) if ids.is_empty() => problems.push(ConversationMatchProblem::EmptyIds),
            Some(ids) => {
                for id in ids {
                    if thread_form(transport, id) {
                        problems.push(ConversationMatchProblem::ThreadFormId { id: id.clone() });
                    } else if !canonical_id_for(transport, id) {
                        problems.push(ConversationMatchProblem::NonCanonicalId { id: id.clone() });
                    }
                }
            }
            None => {}
        }
        problems
    }
}

/// One thing wrong with an owner-authored conversation selector.
///
/// Rendered by whichever configuration surface read it — an attestor grant in the broker, a route
/// in the gateway — so the sentence an operator sees names the file they wrote.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConversationMatchProblem {
    /// `kind: []` claims nothing; the word for every kind is `any`.
    #[error("`kind` is an empty list, which matches nothing; write `kind: any` or list the kinds")]
    EmptyKindList,
    /// The same kind listed twice.
    #[error("`kind` lists {kind} twice")]
    DuplicateKind {
        /// The repeated kind.
        kind: ConversationKind,
    },
    /// A kind this transport never mints.
    #[error("`kind` lists {kind}, which a {transport} transport never produces")]
    ImpossibleKind {
        /// The impossible kind.
        kind: ConversationKind,
        /// The transport family it was written for.
        transport: ChatTransportKind,
    },
    /// `ids: []` claims nothing; the way to claim every id is to omit `ids`.
    #[error("`ids` is an empty list, which matches nothing; omit `ids` to match every id")]
    EmptyIds,
    /// A container on a transport with no container above a conversation.
    #[error("`container` is set, and a {transport} conversation has no container")]
    ContainerNotSupported {
        /// The transport family it was written for.
        transport: ChatTransportKind,
    },
    /// A container no transport of this kind would ever mint.
    #[error("`container` {container:?} is not a canonical container for this transport")]
    NonCanonicalContainer {
        /// The authored value.
        container: String,
    },
    /// An id no transport of this kind would ever mint.
    #[error("`ids` entry {id:?} is not a canonical conversation id for this transport")]
    NonCanonicalId {
        /// The authored value.
        id: String,
    },
    /// An `id:thread` entry: selectors name a parent, never one thread.
    #[error(
        "`ids` entry {id:?} names a thread; selectors name the parent conversation, and `kind` \
         decides whether its threads are claimed"
    )]
    ThreadFormId {
        /// The authored value.
        id: String,
    },
}

/// Whether a value is the `id:thread` form a selector must never carry.
fn thread_form(transport: ChatTransportKind, value: &str) -> bool {
    value
        .split_once(':')
        .is_some_and(|(parent, thread)| !thread.is_empty() && canonical_id_for(transport, parent))
}

/// The conversation-id grammar of one transport family, independent of kind.
fn canonical_id_for(transport: ChatTransportKind, value: &str) -> bool {
    bounded_scope_part(value)
        && match transport {
            ChatTransportKind::Slack => lowercase_token(value),
            ChatTransportKind::Discord => canonical_unsigned_decimal(value),
            ChatTransportKind::Telegram => canonical_signed_decimal(value),
            ChatTransportKind::Whatsapp => canonical_meta_decimal(value),
            ChatTransportKind::Local => lowercase_scope_value(value),
        }
}

/// The container grammar of one transport family.
fn canonical_container_for(transport: ChatTransportKind, value: &str) -> bool {
    bounded_scope_part(value)
        && match transport {
            ChatTransportKind::Slack => lowercase_token(value),
            ChatTransportKind::Discord => canonical_unsigned_decimal(value),
            ChatTransportKind::Whatsapp => canonical_whatsapp_container(value),
            ChatTransportKind::Local => lowercase_scope_value(value),
            // Refused before this is reached; `has_container` is the one definition of it.
            ChatTransportKind::Telegram => false,
        }
}

/// `waba:phoneNumberId`, the pair Meta authenticates a Cloud API delivery under.
pub(crate) fn canonical_whatsapp_container(value: &str) -> bool {
    let mut parts = value.split(':');
    parts.next().is_some_and(canonical_meta_decimal)
        && parts.next().is_some_and(canonical_meta_decimal)
        && parts.next().is_none()
}

pub(crate) fn lowercase_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

pub(crate) fn lowercase_scope_value(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-' | b'_' | b':')
        })
}

pub(crate) fn bounded_scope_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.bytes().any(|byte| byte.is_ascii_control())
        && !value.contains(['/', '\\'])
}

pub(crate) fn canonical_slack_timestamp(value: &str) -> bool {
    value.split_once('.').is_some_and(|(seconds, fraction)| {
        seconds.len() == 10
            && fraction.len() == 6
            && !seconds.starts_with('0')
            && seconds.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    })
}

pub(crate) fn canonical_unsigned_decimal(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|number| number != 0 && number.to_string() == value)
}

pub(crate) fn canonical_positive_service_decimal(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|number| number > 0 && number.to_string() == value)
}

pub(crate) fn canonical_negative_decimal(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|number| number < 0 && number.to_string() == value)
}

pub(crate) fn canonical_signed_decimal(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|number| number != 0 && number.to_string() == value)
}

pub(crate) fn canonical_meta_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn deserialize_part<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = crate::deserialize_bounded_string::<D, 256>(deserializer)?;
    bounded_scope_part(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("conversation part is not canonical"))
}

fn deserialize_optional_part<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalPart;

    impl<'de> serde::de::Visitor<'de> for OptionalPart {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("null or a bounded conversation part")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<Inner>(self, deserializer: Inner) -> Result<Self::Value, Inner::Error>
        where
            Inner: serde::Deserializer<'de>,
        {
            deserialize_part(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(OptionalPart)
}
