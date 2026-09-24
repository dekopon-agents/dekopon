use std::fmt;

use dekopon_core::{ExternalSubject, SubjectService};
use serde::{Deserialize, Serialize};

use crate::ChatTransportKind;

/// A DM with a thread coordinate stays DirectMessage kind, since Slack roots a thread on every DM
/// turn and a Thread kind there would make DMs unmatchable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationKind {
    DirectMessage,
    GroupDirectMessage,
    Channel,
    Thread,
}

impl ConversationKind {
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

#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Conversation {
    pub kind: ConversationKind,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_part",
        skip_serializing_if = "Option::is_none"
    )]
    pub container: Option<String>,
    #[serde(deserialize_with = "deserialize_part")]
    pub id: String,
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
    /// This exact value fills every conversation_id slot, including cancel requests, session
    /// registry, admission key, gateway key, and storage namespace, so they use one derivation
    /// instead of several that could disagree.
    #[must_use]
    pub fn key(&self) -> String {
        match &self.thread {
            Some(thread) => format!("{}:{thread}", self.id),
            None => self.id.clone(),
        }
    }

    /// Needs the transport kind since a Discord thread is itself a channel addressed directly,
    /// while Slack addresses the channel plus a separate thread_ts; guessing wrong misdirects the
    /// reply.
    #[must_use]
    pub fn api_channel(&self, kind: ChatTransportKind) -> &str {
        match (kind, &self.thread) {
            (ChatTransportKind::Discord, Some(thread)) => thread.as_str(),
            _ => self.id.as_str(),
        }
    }

    #[must_use]
    pub fn is_bounded(&self) -> bool {
        bounded_scope_part(&self.id)
            && self.container.as_deref().is_none_or(bounded_scope_part)
            && self.thread.as_deref().is_none_or(bounded_scope_part)
    }

    /// Slack ids skip the leading-letter check because multi-person DMs are G-prefixed on old
    /// workspaces and C-prefixed on new ones, so a strict prefix rule would reject real traffic.
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
                        // A Telegram private chat id is the sender's own user id, so a direct
                        // message claiming a different chat id is claiming someone else's
                        // conversation.
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
                    // For WhatsApp, the sender is the conversation; an individual message can't be
                    // addressed to anything else.
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
    #[must_use]
    pub const fn produces(self, kind: ConversationKind) -> bool {
        match (self, kind) {
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

    pub(crate) const fn has_container(self) -> bool {
        !matches!(self, Self::Telegram)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationKindMatch {
    Any,
    Kinds(Vec<ConversationKind>),
}

impl ConversationKindMatch {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConversationMatch {
    pub kind: ConversationKindMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<String>>,
}

impl ConversationMatch {
    /// Never matches on the thread coordinate, since naming one specific thread in a grant is
    /// deliberately impossible; kind: thread claims every thread under the parent id instead.
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

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConversationMatchProblem {
    #[error("`kind` is an empty list, which matches nothing; write `kind: any` or list the kinds")]
    EmptyKindList,
    #[error("`kind` lists {kind} twice")]
    DuplicateKind { kind: ConversationKind },
    #[error("`kind` lists {kind}, which a {transport} transport never produces")]
    ImpossibleKind {
        kind: ConversationKind,
        transport: ChatTransportKind,
    },
    #[error("`ids` is an empty list, which matches nothing; omit `ids` to match every id")]
    EmptyIds,
    #[error("`container` is set, and a {transport} conversation has no container")]
    ContainerNotSupported { transport: ChatTransportKind },
    #[error("`container` {container:?} is not a canonical container for this transport")]
    NonCanonicalContainer { container: String },
    #[error("`ids` entry {id:?} is not a canonical conversation id for this transport")]
    NonCanonicalId { id: String },
    #[error(
        "`ids` entry {id:?} names a thread; selectors name the parent conversation, and `kind` \
         decides whether its threads are claimed"
    )]
    ThreadFormId { id: String },
}

fn thread_form(transport: ChatTransportKind, value: &str) -> bool {
    transport != ChatTransportKind::Local
        && value.split_once(':').is_some_and(|(parent, thread)| {
            !thread.is_empty() && canonical_id_for(transport, parent)
        })
}

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

fn canonical_container_for(transport: ChatTransportKind, value: &str) -> bool {
    bounded_scope_part(value)
        && match transport {
            ChatTransportKind::Slack => lowercase_token(value),
            ChatTransportKind::Discord => canonical_unsigned_decimal(value),
            ChatTransportKind::Whatsapp => canonical_whatsapp_container(value),
            ChatTransportKind::Local => lowercase_scope_value(value),
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

#[cfg(test)]
mod local_selector_tests {
    use super::{
        ChatTransportKind, ConversationKind, ConversationKindMatch, ConversationMatch,
        ConversationMatchProblem,
    };

    #[test]
    fn a_local_id_containing_a_colon_is_one_id_rather_than_a_thread_form() {
        let selector = ConversationMatch {
            kind: ConversationKindMatch::Kinds(vec![ConversationKind::Channel]),
            container: None,
            ids: Some(vec!["team:dev".to_owned(), "plain".to_owned()]),
        };

        assert_eq!(selector.validate(ChatTransportKind::Local), Vec::new());
        assert_eq!(
            ConversationMatch {
                kind: ConversationKindMatch::Kinds(vec![ConversationKind::Thread]),
                container: None,
                ids: Some(vec!["c0123abc:1712345678.000100".to_owned()]),
            }
            .validate(ChatTransportKind::Slack),
            vec![ConversationMatchProblem::ThreadFormId {
                id: "c0123abc:1712345678.000100".to_owned()
            }],
            "a transport that does thread is still refused the parent:thread form"
        );
    }
}
