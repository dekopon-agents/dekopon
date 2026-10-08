use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_TEMPLATE_PATH_BYTES: usize = 4096;
pub const MAX_TEMPLATE_QUERY_KEY_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ChatSlot {
    #[serde(rename = "conversation.id")]
    ConversationId,
    #[serde(rename = "conversation.thread")]
    ConversationThread,
    #[serde(rename = "conversation.apiChannel")]
    ConversationApiChannel,
    #[serde(rename = "transport")]
    Transport,
}

impl ChatSlot {
    pub const ALL: [Self; 4] = [
        Self::ConversationId,
        Self::ConversationThread,
        Self::ConversationApiChannel,
        Self::Transport,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConversationId => "conversation.id",
            Self::ConversationThread => "conversation.thread",
            Self::ConversationApiChannel => "conversation.apiChannel",
            Self::Transport => "transport",
        }
    }
}

impl fmt::Display for ChatSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Rendered per invocation from the attested chat scope; never serialized, so the values stay out
/// of evidence and the authority commitment.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct ChatSlotValues(BTreeMap<ChatSlot, String>);

impl fmt::Debug for ChatSlotValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_map()
            .entries(self.0.keys().map(|slot| (slot.as_str(), "[REDACTED]")))
            .finish()
    }
}

impl ChatSlotValues {
    pub fn insert(&mut self, slot: ChatSlot, value: String) {
        self.0.insert(slot, value);
    }

    #[must_use]
    pub fn get(&self, slot: ChatSlot) -> Option<&str> {
        self.0.get(&slot).map(String::as_str)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PathSegment {
    Literal(String),
    Slot(ChatSlot),
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct PathTemplate {
    segments: Vec<PathSegment>,
}

impl PathTemplate {
    #[must_use]
    pub fn segments(&self) -> &[PathSegment] {
        &self.segments
    }

    pub fn slots(&self) -> impl Iterator<Item = ChatSlot> + '_ {
        self.segments.iter().filter_map(|segment| match segment {
            PathSegment::Slot(slot) => Some(*slot),
            PathSegment::Literal(_) => None,
        })
    }

    /// A slot position matches only its own `{slot}` placeholder: the provider names the slot and
    /// never supplies the value.
    #[must_use]
    pub fn matches(&self, decoded: &[&str]) -> bool {
        self.segments.len() == decoded.len()
            && self
                .segments
                .iter()
                .zip(decoded)
                .all(|(segment, candidate)| match segment {
                    PathSegment::Literal(literal) => literal == candidate,
                    PathSegment::Slot(slot) => candidate
                        .strip_prefix('{')
                        .and_then(|rest| rest.strip_suffix('}'))
                        .is_some_and(|name| name == slot.as_str()),
                })
    }
}

impl fmt::Display for PathTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.segments.is_empty() {
            return formatter.write_str("/");
        }
        for segment in &self.segments {
            match segment {
                PathSegment::Literal(literal) => write!(formatter, "/{literal}")?,
                PathSegment::Slot(slot) => write!(formatter, "/{{{slot}}}")?,
            }
        }
        Ok(())
    }
}

impl FromStr for PathTemplate {
    type Err = PathTemplateError;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        let invalid = || PathTemplateError::Invalid {
            path: path.to_owned(),
        };
        if !path.starts_with('/') || path.len() > MAX_TEMPLATE_PATH_BYTES {
            return Err(invalid());
        }
        if path == "/" {
            return Ok(Self {
                segments: Vec::new(),
            });
        }
        let segments = path
            .split('/')
            .skip(1)
            .map(|segment| {
                if let Some(name) = segment
                    .strip_prefix('{')
                    .and_then(|rest| rest.strip_suffix('}'))
                {
                    return ChatSlot::ALL
                        .into_iter()
                        .find(|slot| slot.as_str() == name)
                        .map(PathSegment::Slot)
                        .ok_or_else(|| PathTemplateError::UnknownSlot {
                            slot: name.to_owned(),
                        });
                }
                if segment.is_empty()
                    || matches!(segment, "." | "..")
                    || !segment.bytes().all(is_literal_path_byte)
                {
                    return Err(invalid());
                }
                Ok(PathSegment::Literal(segment.to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { segments })
    }
}

impl TryFrom<String> for PathTemplate {
    type Error = PathTemplateError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        path.parse()
    }
}

impl From<PathTemplate> for String {
    fn from(template: PathTemplate) -> Self {
        template.to_string()
    }
}

/// Literals are restricted to bytes a path segment carries unencoded, so the host can rebuild the
/// outgoing path from the template without an encoding choice.
const fn is_literal_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PathTemplateError {
    #[error("request template path {path:?} is not a canonical absolute path")]
    Invalid { path: String },
    #[error("request template slot {{{slot}}} is not an attested value")]
    UnknownSlot { slot: String },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryTemplate {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pinned: BTreeMap<String, ChatSlot>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub allowed: BTreeSet<String>,
}

impl QueryTemplate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pinned.is_empty() && self.allowed.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestTemplate {
    pub method: String,
    pub path: PathTemplate,
    #[serde(default, skip_serializing_if = "QueryTemplate::is_empty")]
    pub query: QueryTemplate,
}

impl RequestTemplate {
    pub fn slots(&self) -> impl Iterator<Item = ChatSlot> + '_ {
        self.path.slots().chain(self.query.pinned.values().copied())
    }
}

impl fmt::Display for RequestTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.method, self.path)
    }
}

pub(crate) fn is_query_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_TEMPLATE_QUERY_KEY_BYTES
        && !key.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{
        ChatSlot, ChatSlotValues, PathSegment, PathTemplate, PathTemplateError, RequestTemplate,
    };

    #[test]
    fn chat_slot_values_debug_names_the_slots_and_redacts_the_ids() {
        let mut slots = ChatSlotValues::default();
        slots.insert(ChatSlot::ConversationId, "C0123ABC".to_owned());
        slots.insert(ChatSlot::ConversationThread, "1700000000.000100".to_owned());
        let rendered = format!("{slots:?}");
        assert!(rendered.contains("conversation.id"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
        assert!(!rendered.contains("C0123ABC"), "{rendered}");
        assert!(!rendered.contains("1700000000"), "{rendered}");
    }

    #[test]
    fn path_templates_parse_slots_and_round_trip() {
        let template: PathTemplate = "/channels/{conversation.apiChannel}/messages"
            .parse()
            .expect("valid template");
        assert_eq!(
            template.segments(),
            [
                PathSegment::Literal("channels".to_owned()),
                PathSegment::Slot(ChatSlot::ConversationApiChannel),
                PathSegment::Literal("messages".to_owned()),
            ]
        );
        assert_eq!(
            template.to_string(),
            "/channels/{conversation.apiChannel}/messages"
        );
        assert!(template.matches(&["channels", "{conversation.apiChannel}", "messages"]));
        assert!(!template.matches(&["channels", "123", "messages"]));
        assert!(!template.matches(&["channels", "{conversation.id}", "messages"]));
        assert!(!template.matches(&["channels", "{conversation.apiChannel}"]));
        let root: PathTemplate = "/".parse().expect("root");
        assert!(root.matches(&[]));
    }

    #[test]
    fn path_templates_refuse_non_canonical_paths_and_unknown_slots() {
        for path in [
            "",
            "channels",
            "/a//b",
            "/a/",
            "/a/../b",
            "/a/./b",
            "/a%2Fb",
            "/a?b",
            "/a#b",
            "/a b",
            "/{conversation.apiChannel",
        ] {
            assert!(
                matches!(
                    path.parse::<PathTemplate>(),
                    Err(PathTemplateError::Invalid { .. })
                ),
                "{path:?}"
            );
        }
        assert_eq!(
            "/channels/{conversation.guild}".parse::<PathTemplate>(),
            Err(PathTemplateError::UnknownSlot {
                slot: "conversation.guild".to_owned()
            })
        );
    }

    #[test]
    fn request_templates_read_the_owner_grammar() {
        let template: RequestTemplate = serde_json::from_value(serde_json::json!({
            "method": "GET",
            "path": "/api/conversations.replies",
            "query": {
                "pinned": {"channel": "conversation.id", "ts": "conversation.thread"},
                "allowed": ["limit", "cursor"]
            }
        }))
        .expect("grammar");
        assert_eq!(
            template.slots().collect::<Vec<_>>(),
            [ChatSlot::ConversationId, ChatSlot::ConversationThread]
        );
        assert!(
            serde_json::from_value::<RequestTemplate>(serde_json::json!({
                "method": "GET",
                "path": "/api",
                "query": {"pinned": {"channel": "conversation.container"}}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RequestTemplate>(serde_json::json!({
                "method": "GET",
                "path": "/api",
                "headers": {}
            }))
            .is_err()
        );
    }
}
