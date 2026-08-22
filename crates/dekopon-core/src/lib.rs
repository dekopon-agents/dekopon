//! Dependency-light domain types for Dekopon.
//!
//! Identifiers are validated at construction and during deserialization. This prevents
//! malformed resource references from leaking into the rest of the workspace while
//! keeping transport, command-line, async-runtime, and policy concerns out of this crate.

#![forbid(unsafe_code)]

mod redaction;
mod subject;
mod telemetry_payloads;

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use thiserror::Error;

pub use redaction::{Redacted, redaction_marker, serialize_exposed};
pub use subject::{ExternalSubject, SubjectError, SubjectService};
pub use telemetry_payloads::{set_telemetry_payloads, telemetry_payloads};

pub(crate) const MAX_IDENTIFIER_LENGTH: usize = 253;

/// File extension a Dekopon provider component is recognized by.
///
/// Shared so the privileged broker and the direct runner cannot disagree about which files in a
/// provider directory are components. Each does its own directory read — one under owner-only
/// rules, one unprivileged — but both select by this.
pub const PROVIDER_COMPONENT_EXTENSION: &str = "wasm";

/// Words a provider may not claim as a command word.
///
/// The sandboxed shell owns this namespace: builtins, the words the evaluator executes itself, and
/// the words it refuses by name. A provider command word is dispatched only after all three, so a
/// claim on one of these could never fire — reporting it at load is the difference between a
/// manifest that lies and one that fails.
///
/// It lives here rather than in `dekopon-shell` so the broker can produce one conflict report at
/// its own startup without linking an interpreter it never runs. `dekopon-shell` owns the tables
/// this mirrors and pins the two together with a bidirectional test, so a builtin added or removed
/// there fails the build until this list agrees.
pub const RESERVED_COMMAND_WORDS: &[&str] = &[
    ".", ":", "[", "[[", "]]", "base64", "bg", "break", "cap", "case", "cat", "continue", "curl",
    "cut", "date", "declare", "do", "done", "echo", "elif", "else", "esac", "eval", "exec", "exit",
    "export", "false", "fg", "fi", "for", "function", "grep", "if", "in", "jobs", "jq", "kill",
    "local", "printf", "return", "sed", "select", "set", "shift", "sleep", "sort", "source",
    "test", "then", "trap", "true", "uniq", "unset", "until", "wait", "wc", "while", "xargs",
];

/// The reason a Dekopon identifier could not be parsed.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IdentifierError {
    /// The identifier was empty.
    #[error("{kind} identifier must not be empty")]
    Empty {
        /// Human-readable identifier kind.
        kind: &'static str,
    },
    /// The identifier exceeded the supported wire limit.
    #[error("{kind} identifier is {length} bytes; the maximum is {maximum}")]
    TooLong {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Actual byte length.
        length: usize,
        /// Maximum byte length.
        maximum: usize,
    },
    /// The first character was not an ASCII lowercase letter or digit.
    #[error(
        "{kind} identifier must start with a lowercase ASCII letter or digit, found {character:?}"
    )]
    InvalidStart {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Invalid character.
        character: char,
    },
    /// The last character was a separator.
    #[error(
        "{kind} identifier must end with a lowercase ASCII letter or digit, found {character:?}"
    )]
    InvalidEnd {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Invalid character.
        character: char,
    },
    /// A character outside the portable identifier alphabet was present.
    #[error(
        "{kind} identifier contains invalid character {character:?} at byte {index}; use lowercase ASCII letters, digits, '.', '-', or '_'"
    )]
    InvalidCharacter {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Byte offset in the submitted value.
        index: usize,
        /// Invalid character.
        character: char,
    },
    /// Two separator characters appeared next to one another.
    #[error("{kind} identifier contains adjacent separators at byte {index}")]
    AdjacentSeparators {
        /// Human-readable identifier kind.
        kind: &'static str,
        /// Byte offset of the second separator.
        index: usize,
    },
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
        #[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Returns the validated identifier as a string slice.
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
                // `TryFrom<String>` validates the deserialized buffer in place; `parse` would
                // validate a borrow of it and then allocate a second copy of the same bytes.
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
identifier!(TraceId, "trace", "A validated end-to-end trace identifier.");
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

/// The authenticated actor responsible for an operation.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Actor {
    /// A human operator.
    Human {
        /// The operator's trusted principal identity.
        principal: PrincipalId,
    },
    /// A Dekopon agent. The envelope carrying this value must authenticate it.
    Agent {
        /// The agent identity.
        agent: AgentId,
    },
    /// A non-human service principal.
    Service {
        /// The service's trusted principal identity.
        principal: PrincipalId,
    },
}

/// Coarse risk classification used as policy input.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum RiskLevel {
    /// No expected external side effect and limited data exposure.
    Low,
    /// Meaningful data access or a reversible/local effect.
    Medium,
    /// An external write, sensitive data access, or difficult rollback.
    High,
    /// A potentially destructive or high-impact operation.
    Critical,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// Operational phase reported for an agent.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum AgentStatus {
    /// Configuration intentionally prevents the agent from running.
    Disabled,
    /// The agent is valid but not yet ready.
    Pending,
    /// The agent is ready for orchestration.
    Ready,
    /// The agent cannot operate because of an error.
    Error,
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentId, IdentifierError, RiskLevel};

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
}

/// Why a provider may not claim a command word.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CommandWordConflictKind {
    /// The sandboxed shell owns the word: a builtin, a control word, or one it refuses by name.
    Reserved,
    /// The word is shaped like a capability identifier, which it would shadow.
    ///
    /// The shell resolves provider command words *before* capability fallback, so allowing one
    /// would make the provider command win and the granted capability of that name unreachable
    /// under its own spelling. Provider words and capability identifiers stay disjoint instead.
    CapabilityShaped,
    /// More than one provider claimed it.
    Duplicate,
    /// One provider declared it more than once.
    Repeated,
}

impl CommandWordConflictKind {
    /// Returns the operator-facing explanation of why the claim cannot stand.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Reserved => "is reserved by the sandboxed shell and could never dispatch",
            Self::CapabilityShaped => {
                "contains `.`, `-`, or `_`, so it would shadow the capability of that name; the \
                 shell resolves provider command words before capability fallback"
            }
            Self::Duplicate => "is claimed by more than one provider",
            Self::Repeated => "is declared more than once by the same provider",
        }
    }

    /// Returns the fix an operator should apply.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::Duplicate => "rename one command word, or drop a provider from the search path",
            Self::CapabilityShaped => {
                "rename the command word to a separator-free one; the capability remains invocable \
                 by its full identifier"
            }
            Self::Reserved => "rename the command word; this name is reserved",
            Self::Repeated => "remove the repeated entry from that provider's command words",
        }
    }
}

/// One command word that cannot be granted to the provider(s) claiming it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandWordConflict {
    /// The contested word.
    pub word: String,
    /// Every distinct provider claiming it, in the order they were loaded.
    pub claimants: Vec<String>,
    /// Why the claim cannot stand.
    pub kind: CommandWordConflictKind,
}

/// Finds every reason the given provider-declared command words cannot all be granted.
///
/// Ambiguity is fatal in a way absence is not. A word two providers both claim has no meaning the
/// shell can pick without silently choosing for the operator, so this reports rather than resolves,
/// and it reports *everything* — fixing a provider directory should take one restart, not six.
///
/// `declared` is `(provider id, command words)` in load order.
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
        // A manifest listing one word twice is one provider's mistake, not a collision between
        // two, so the count that decides Duplicate is of distinct providers.
        let mut distinct = Vec::with_capacity(providers.len());
        for provider in &providers {
            if !distinct.contains(provider) {
                distinct.push(provider.clone());
            }
        }
        let kind = if RESERVED_COMMAND_WORDS.contains(&word) {
            CommandWordConflictKind::Reserved
        } else if word.contains(['.', '-', '_']) && word.parse::<CapabilityId>().is_ok() {
            CommandWordConflictKind::CapabilityShaped
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

    /// A provider may claim `gh`, which is the point of having deleted the builtin.
    ///
    /// This test replaces `gh_is_reserved_until_its_builtin_is_deleted`, which existed to fail at
    /// exactly this moment. Keeping its successor pointed at the same word is what stops `gh`
    /// quietly returning to the reserved list and stranding the out-of-tree provider.
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
            ("gh.pr", CommandWordConflictKind::CapabilityShaped),
        ] {
            let conflicts = command_word_conflicts(&declared(&[("some-provider", &[word])]));
            assert_eq!(conflicts.len(), 1, "{word}: {conflicts:?}");
            assert_eq!(conflicts[0].kind, kind, "{word}");
            assert_eq!(conflicts[0].word, word);
        }
    }

    /// A repeated word is still refused, but the operator is told what actually happened: one
    /// manifest lists it twice. "More than one provider" would send them looking for a provider
    /// that does not exist.
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

        // A real two-provider collision still reads as one, even when one of them repeats.
        let conflicts = command_word_conflicts(&declared(&[
            ("fly", &["deploy", "deploy"]),
            ("k8s", &["deploy"]),
        ]));
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].kind, CommandWordConflictKind::Duplicate);
        assert_eq!(conflicts[0].claimants, ["fly", "k8s"]);
    }

    /// The explanation is printed verbatim in the broker's startup report, so it has to describe
    /// the mechanism that actually exists: provider words resolve before capability fallback.
    #[test]
    fn the_capability_shaped_explanation_names_the_shadowing_hazard() {
        let conflicts = command_word_conflicts(&declared(&[("some-provider", &["gh.pr"])]));

        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        let explanation = conflicts[0].kind.explanation();
        assert!(
            explanation.contains("shadow the capability"),
            "{explanation}"
        );
        assert!(
            explanation.contains("before capability fallback"),
            "{explanation}"
        );
    }

    #[test]
    fn two_providers_claiming_one_word_names_both() {
        let conflicts =
            command_word_conflicts(&declared(&[("fly", &["deploy"]), ("k8s", &["deploy"])]));
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].kind, CommandWordConflictKind::Duplicate);
        assert_eq!(conflicts[0].claimants, ["fly", "k8s"]);
    }

    /// The requirement this whole shape exists for: fixing a provider directory takes one restart.
    ///
    /// A check that returned on the first problem would make an operator rediscover the next one
    /// after every rebuild.
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
                ("gh.pr", CommandWordConflictKind::CapabilityShaped),
                ("jq", CommandWordConflictKind::Reserved),
            ]
        );
    }

    /// Reserved beats duplicate: two providers claiming `jq` have a naming problem, but the one
    /// worth telling them about is that `jq` could never have dispatched either way.
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
