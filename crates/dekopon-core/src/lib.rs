#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
pub mod asset;

#[cfg(feature = "native")]
pub mod base64;

mod accept;
mod attribute;
mod diagnostics;
mod failure;
mod redaction;
mod skill;
mod subject;
mod trace;
#[cfg(unix)]
mod trusted_file;

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;

pub use accept::{ACCEPT_BACKOFF_MS, MAX_ACCEPT_BACKOFF_MS, retryable_accept_error};
pub use attribute::{BoundedDisplay, MAX_ATTRIBUTE_BYTES, bounded_attribute, bounded_display};
pub use diagnostics::error_chain;
pub use failure::{MAX_FAILURE_CODE_BYTES, MAX_FAILURE_MESSAGE_BYTES, ProviderFailureDetail};
pub use redaction::{REDACTION_MARKER, Redacted, redaction_marker, serialize_exposed};
pub use skill::{MAX_SKILL_NAME_LENGTH, SkillId, SkillIdError};
pub use subject::{ExternalSubject, SubjectError, SubjectService};
pub use trace::{TraceId, TraceIdError};
#[cfg(unix)]
pub use trusted_file::{
    AncestorPolicy, FileHygieneError, FileTier, check_trusted_ancestors, check_trusted_metadata,
    read_trusted_file,
};

pub(crate) const MAX_IDENTIFIER_LENGTH: usize = 253;
pub const MAX_SECRET_DRN_LENGTH: usize = 512;
pub const MAX_SECRET_USERNAME_LENGTH: usize = 256;

pub const PROVIDER_COMPONENT_EXTENSION: &str = "wasm";

/// Must mirror dekopon-shell's own builtin and reserved-word tables exactly; a bidirectional test
/// fails the build if the two disagree.
pub const RESERVED_COMMAND_WORDS: &[&str] = &[
    ".", ":", "[", "[[", "]]", "base64", "bg", "break", "cap", "case", "cat", "continue", "cut",
    "declare", "do", "done", "echo", "elif", "else", "esac", "eval", "exec", "exit", "export",
    "false", "fg", "fi", "for", "function", "grep", "if", "in", "jobs", "jq", "kill", "local",
    "printf", "read", "return", "sed", "select", "set", "shift", "sleep", "sort", "source", "test",
    "then", "trap", "true", "uniq", "unset", "until", "wait", "wc", "while", "xargs",
];

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IdentifierError {
    #[error("{kind} identifier must not be empty")]
    Empty { kind: &'static str },
    #[error("{kind} identifier is {length} bytes; the maximum is {maximum}")]
    TooLong {
        kind: &'static str,
        length: usize,
        maximum: usize,
    },
    #[error(
        "{kind} identifier must start with a lowercase ASCII letter or digit, found {character:?}"
    )]
    InvalidStart { kind: &'static str, character: char },
    #[error(
        "{kind} identifier must end with a lowercase ASCII letter or digit, found {character:?}"
    )]
    InvalidEnd { kind: &'static str, character: char },
    #[error(
        "{kind} identifier contains invalid character {character:?} at byte {index}; use lowercase ASCII letters, digits, '.', '-', or '_'"
    )]
    InvalidCharacter {
        kind: &'static str,
        index: usize,
        character: char,
    },
    #[error("{kind} identifier contains adjacent separators at byte {index}")]
    AdjacentSeparators { kind: &'static str, index: usize },
}

fn is_edge_character(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit()
}

fn is_separator(character: char) -> bool {
    matches!(character, '.' | '-' | '_')
}

fn validate_identifier(value: &str, kind: &'static str) -> Result<(), IdentifierError> {
    if value.is_empty() {
        return Err(IdentifierError::Empty { kind });
    }
    if value.len() > MAX_IDENTIFIER_LENGTH {
        return Err(IdentifierError::TooLong {
            kind,
            length: value.len(),
            maximum: MAX_IDENTIFIER_LENGTH,
        });
    }

    let mut characters = value.char_indices();
    let (_, first) = characters.next().ok_or(IdentifierError::Empty { kind })?;
    if !is_edge_character(first) {
        return Err(IdentifierError::InvalidStart {
            kind,
            character: first,
        });
    }

    let mut previous_was_separator = false;
    for (index, character) in value.char_indices() {
        if !is_edge_character(character) && !is_separator(character) {
            return Err(IdentifierError::InvalidCharacter {
                kind,
                index,
                character,
            });
        }
        if is_separator(character) && previous_was_separator {
            return Err(IdentifierError::AdjacentSeparators { kind, index });
        }
        previous_was_separator = is_separator(character);
    }

    let last = value
        .chars()
        .next_back()
        .ok_or(IdentifierError::Empty { kind })?;
    if !is_edge_character(last) {
        return Err(IdentifierError::InvalidEnd {
            kind,
            character: last,
        });
    }

    Ok(())
}

macro_rules! identifier {
    ($name:ident, $label:literal, $docs:literal) => {
        #[doc = $docs]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                validate_identifier(value, $label)?;
                Ok(Self(value.to_owned()))
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                validate_identifier(&value, $label)?;
                Ok(Self(value))
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdentifierError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::try_from(String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}

identifier!(AgentId, "agent", "A validated agent resource identifier.");
identifier!(
    CapabilityId,
    "capability",
    "A validated capability resource identifier."
);
identifier!(
    ProviderId,
    "provider",
    "A validated capability-provider identifier."
);
identifier!(TaskId, "task", "A validated task identifier.");
identifier!(
    InvocationId,
    "invocation",
    "A validated capability invocation identifier."
);
identifier!(
    TransportId,
    "transport",
    "A validated owner-configured chat transport identifier."
);
identifier!(
    PrincipalId,
    "principal",
    "A validated authenticated principal identifier."
);
identifier!(GroupId, "group", "A validated principal group identifier.");

/// A DRN is deliberately inert: knowing or copying one grants no authority; the actual backend
/// location stays in the broker's private secret map.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SecretDrn(String);

impl SecretDrn {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretDrn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SecretDrn {
    type Err = SecretDrnError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_secret_drn(value)?;
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for SecretDrn {
    type Error = SecretDrnError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_secret_drn(&value)?;
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for SecretDrn {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

fn validate_secret_drn(value: &str) -> Result<(), SecretDrnError> {
    if value.len() > MAX_SECRET_DRN_LENGTH {
        return Err(SecretDrnError::TooLong {
            length: value.len(),
            maximum: MAX_SECRET_DRN_LENGTH,
        });
    }
    let mut parts = value.splitn(5, ':');
    if parts.next() != Some("drn")
        || parts.next().is_none_or(|part| !valid_drn_authority(part))
        || parts.next() != Some("secret")
        || parts.next().is_none_or(|part| !valid_drn_component(part))
    {
        return Err(SecretDrnError::Malformed);
    }
    let path = parts.next().ok_or(SecretDrnError::Malformed)?;
    if path.is_empty()
        || path.bytes().any(|byte| {
            byte.is_ascii_uppercase()
                || byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'%' | b'?' | b'#' | b'\\')
        })
        || path
            .split('/')
            .any(|segment| !valid_drn_component(segment) || matches!(segment, "." | ".."))
    {
        return Err(SecretDrnError::Malformed);
    }
    Ok(())
}

fn valid_drn_authority(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LENGTH
        && value
            .split('.')
            .all(|segment| valid_drn_component(segment) && !segment.as_bytes().contains(&b'_'))
}

fn valid_drn_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LENGTH
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .next_back()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SecretDrnError {
    #[error("secret DRN must be canonical `drn:<authority>:secret:<realm>:<logical-path>`")]
    Malformed,
    #[error("secret DRN is {length} bytes; maximum is {maximum}")]
    TooLong { length: usize, maximum: usize },
}

/// Deliberately has no serialization and no public way back to an owned byte vector; ordinary
/// rendering reveals neither content nor length.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SecretSinkKind {
    HttpBearer,
    HttpBasic,
}

impl fmt::Display for SecretSinkKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::HttpBearer => "httpBearer",
            Self::HttpBasic => "httpBasic",
        })
    }
}

/// Untrusted intent only; never authority and never passed to a provider, since the broker must
/// separately authorize secret.use against an owner-authored binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum SecretUseProposal {
    HttpBearer {
        secret: SecretDrn,
    },
    HttpBasic {
        secret: SecretDrn,
        #[serde(deserialize_with = "deserialize_secret_username")]
        username: String,
    },
}

impl SecretUseProposal {
    #[must_use]
    pub const fn secret(&self) -> &SecretDrn {
        match self {
            Self::HttpBearer { secret } | Self::HttpBasic { secret, .. } => secret,
        }
    }

    #[must_use]
    pub const fn sink(&self) -> SecretSinkKind {
        match self {
            Self::HttpBearer { .. } => SecretSinkKind::HttpBearer,
            Self::HttpBasic { .. } => SecretSinkKind::HttpBasic,
        }
    }

    #[must_use]
    pub fn username(&self) -> Option<&str> {
        match self {
            Self::HttpBasic { username, .. } => Some(username),
            Self::HttpBearer { .. } => None,
        }
    }
}

fn deserialize_secret_username<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let username = String::deserialize(deserializer)?;
    if username.is_empty()
        || username.len() > MAX_SECRET_USERNAME_LENGTH
        || username.contains(':')
        || username
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == 0x7f)
    {
        return Err(D::Error::custom(
            "HTTP Basic username must be nonempty, bounded, colon-free, and contain no controls",
        ));
    }
    Ok(username)
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Actor {
    Human { principal: PrincipalId },
    Agent { agent: AgentId },
    Service { principal: PrincipalId },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum AgentStatus {
    Disabled,
    Pending,
    Ready,
    Error,
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentId, IdentifierError, RiskLevel, SecretDrn, SecretSinkKind, SecretUseProposal,
    };

    #[test]
    fn accepts_portable_identifiers() {
        for value in ["reviewer", "github.pull-request.read", "agent_2"] {
            let parsed = value.parse::<AgentId>();
            assert!(parsed.is_ok(), "{value} should be valid: {parsed:?}");
        }
    }

    #[test]
    fn rejects_invalid_identifiers_with_context() {
        assert!(matches!(
            "Reviewer".parse::<AgentId>(),
            Err(IdentifierError::InvalidStart { .. })
        ));
        assert!(matches!(
            "github..read".parse::<AgentId>(),
            Err(IdentifierError::AdjacentSeparators { .. })
        ));
        assert!(matches!(
            "reviewer/one".parse::<AgentId>(),
            Err(IdentifierError::InvalidCharacter { index: 8, .. })
        ));
        assert!(matches!(
            "reviewer-".parse::<AgentId>(),
            Err(IdentifierError::InvalidEnd { .. })
        ));
    }

    #[test]
    fn deserialization_cannot_bypass_validation() {
        let error = serde_json::from_str::<AgentId>(r#""not valid""#)
            .expect_err("whitespace must be rejected");
        assert!(error.to_string().contains("invalid character"));
    }

    #[test]
    fn display_is_stable() {
        assert_eq!(RiskLevel::High.to_string(), "High");
    }

    #[test]
    fn secret_drns_have_one_logical_backend_independent_spelling() {
        let value = "drn:com.xrl:secret:prod:payments/blah-api-basic";
        let parsed = value.parse::<SecretDrn>().expect("canonical DRN");
        assert_eq!(parsed.to_string(), value);
        let round_trip = serde_json::from_str::<SecretDrn>(
            &serde_json::to_string(&parsed).expect("serialize DRN"),
        )
        .expect("deserialize DRN");
        assert_eq!(round_trip, parsed);
    }

    #[test]
    fn secret_drns_reject_physical_or_ambiguous_spellings() {
        for value in [
            "drn::secret:prod:name",
            "drn:com.xrl:secret:Prod:name",
            "drn:com.xrl:secret:prod:",
            "drn:com.xrl:secret:prod:a//b",
            "drn:com.xrl:secret:prod:a/../b",
            "drn:com.xrl:secret:prod:a%2fb",
            "drn:com.xrl:secret:prod:a?version=2",
            "drn:com..xrl:secret:prod:name",
            "drn:com_xrl:secret:prod:name",
        ] {
            assert!(value.parse::<SecretDrn>().is_err(), "accepted {value}");
        }
    }

    #[test]
    fn typed_secret_use_rejects_basic_username_confusion() {
        let valid = serde_json::from_str::<SecretUseProposal>(
            r#"{"kind":"httpBasic","secret":"drn:com.xrl:secret:prod:api/basic","username":"user-a"}"#,
        )
        .expect("valid Basic proposal");
        assert_eq!(valid.sink(), SecretSinkKind::HttpBasic);
        assert_eq!(valid.username(), Some("user-a"));

        for username in ["", "user:password", "line\nbreak"] {
            let document = format!(
                r#"{{"kind":"httpBasic","secret":"drn:com.xrl:secret:prod:api/basic","username":{}}}"#,
                serde_json::to_string(username).expect("username JSON")
            );
            assert!(
                serde_json::from_str::<SecretUseProposal>(&document).is_err(),
                "accepted {username:?}"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CommandWordConflictKind {
    Reserved,
    Duplicate,
    Repeated,
}

impl CommandWordConflictKind {
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Reserved => "is reserved by the sandboxed shell and could never dispatch",
            Self::Duplicate => "is claimed by more than one provider",
            Self::Repeated => "is declared more than once by the same provider",
        }
    }

    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::Duplicate => "rename one command word, or drop a provider from the search path",
            Self::Reserved => "rename the command word; this name is reserved",
            Self::Repeated => "remove the repeated entry from that provider's command words",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandWordConflict {
    pub word: String,
    pub claimants: Vec<String>,
    pub kind: CommandWordConflictKind,
}

/// Reports every conflict rather than silently picking a winner for the operator, and reports all
/// of them at once so one fix pass suffices.
#[must_use]
pub fn command_word_conflicts(declared: &[(String, Vec<String>)]) -> Vec<CommandWordConflict> {
    use std::collections::BTreeMap;

    let mut claimants: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (provider, words) in declared {
        for word in words {
            claimants
                .entry(word.as_str())
                .or_default()
                .push(provider.clone());
        }
    }

    let mut conflicts = Vec::new();
    for (word, providers) in claimants {
        let mut distinct = Vec::with_capacity(providers.len());
        for provider in &providers {
            if !distinct.contains(provider) {
                distinct.push(provider.clone());
            }
        }
        let kind = if RESERVED_COMMAND_WORDS.contains(&word) {
            CommandWordConflictKind::Reserved
        } else if distinct.len() > 1 {
            CommandWordConflictKind::Duplicate
        } else if distinct.len() < providers.len() {
            CommandWordConflictKind::Repeated
        } else {
            continue;
        };
        conflicts.push(CommandWordConflict {
            word: word.to_owned(),
            claimants: distinct,
            kind,
        });
    }
    conflicts
}

#[cfg(test)]
mod command_word_tests {
    use super::{CommandWordConflictKind, RESERVED_COMMAND_WORDS, command_word_conflicts};

    fn declared(entries: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        entries
            .iter()
            .map(|(provider, words)| {
                (
                    (*provider).to_owned(),
                    words.iter().map(|word| (*word).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_word_no_one_else_claims_is_no_conflict() {
        assert!(
            command_word_conflicts(&declared(&[("fly", &["fly"]), ("k8s", &["kubectl"])]))
                .is_empty()
        );
    }

    #[test]
    fn a_provider_may_claim_gh_now_that_no_builtin_owns_it() {
        assert!(command_word_conflicts(&declared(&[("gh", &["gh"])])).is_empty());
    }

    #[test]
    fn each_class_of_conflict_is_recognized() {
        for (word, kind) in [
            ("jq", CommandWordConflictKind::Reserved),
            ("eval", CommandWordConflictKind::Reserved),
            ("break", CommandWordConflictKind::Reserved),
        ] {
            let conflicts = command_word_conflicts(&declared(&[("some-provider", &[word])]));
            assert_eq!(conflicts.len(), 1, "{word}: {conflicts:?}");
            assert_eq!(conflicts[0].kind, kind, "{word}");
            assert_eq!(conflicts[0].word, word);
        }
    }

    #[test]
    fn one_provider_repeating_a_word_is_not_reported_as_two_providers() {
        let conflicts = command_word_conflicts(&declared(&[("fly", &["deploy", "deploy"])]));

        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].kind, CommandWordConflictKind::Repeated);
        assert_eq!(conflicts[0].claimants, ["fly"]);
        assert!(
            conflicts[0]
                .kind
                .explanation()
                .contains("more than once by the same provider"),
            "{}",
            conflicts[0].kind.explanation()
        );

        let conflicts = command_word_conflicts(&declared(&[
            ("fly", &["deploy", "deploy"]),
            ("k8s", &["deploy"]),
        ]));
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].kind, CommandWordConflictKind::Duplicate);
        assert_eq!(conflicts[0].claimants, ["fly", "k8s"]);
    }

    #[test]
    fn a_word_containing_a_separator_follows_the_ordinary_rules() {
        assert!(
            command_word_conflicts(&declared(&[(
                "some-provider",
                &["gh.pr", "wiki-page", "wikipedia_page"]
            )]))
            .is_empty()
        );

        let conflicts =
            command_word_conflicts(&declared(&[("fly", &["gh.pr"]), ("k8s", &["gh.pr"])]));
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].kind, CommandWordConflictKind::Duplicate);
        assert_eq!(conflicts[0].claimants, ["fly", "k8s"]);
    }

    #[test]
    fn two_providers_claiming_one_word_names_both() {
        let conflicts =
            command_word_conflicts(&declared(&[("fly", &["deploy"]), ("k8s", &["deploy"])]));
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].kind, CommandWordConflictKind::Duplicate);
        assert_eq!(conflicts[0].claimants, ["fly", "k8s"]);
    }

    #[test]
    fn conflicts_of_several_classes_are_all_reported_at_once() {
        let conflicts = command_word_conflicts(&declared(&[
            ("fly", &["deploy", "jq"]),
            ("k8s", &["deploy", "gh.pr"]),
            ("danger", &["eval"]),
        ]));

        let mut found = conflicts
            .iter()
            .map(|conflict| (conflict.word.as_str(), conflict.kind))
            .collect::<Vec<_>>();
        found.sort();
        assert_eq!(
            found,
            [
                ("deploy", CommandWordConflictKind::Duplicate),
                ("eval", CommandWordConflictKind::Reserved),
                ("jq", CommandWordConflictKind::Reserved),
            ]
        );
    }

    #[test]
    fn a_reserved_word_is_reported_as_reserved_even_when_contested() {
        let conflicts = command_word_conflicts(&declared(&[("one", &["jq"]), ("two", &["jq"])]));
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, CommandWordConflictKind::Reserved);
        assert_eq!(conflicts[0].claimants, ["one", "two"]);
    }

    #[test]
    fn the_reserved_list_is_sorted_and_free_of_duplicates() {
        let mut sorted = RESERVED_COMMAND_WORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted, RESERVED_COMMAND_WORDS,
            "keep this list sorted and unique"
        );
    }
}

#[must_use]
pub fn chat_asset_marker(text: &str) -> Option<u64> {
    let digits = text.strip_prefix("chat-asset:")?;
    if digits.is_empty()
        || digits.starts_with('0')
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}
