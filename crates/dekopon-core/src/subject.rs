//! External chat-transport subjects and their canonical, identifier-safe form.
//!
//! A subject names *who a message came from* on an external service — a Slack user inside a Slack
//! workspace, a Telegram account, a telephone number. Raw external identifiers do not fit the
//! workspace identifier grammar (Slack IDs are uppercase, E.164 numbers start with `+`), so this
//! type owns one canonical normalization: dotted lowercase segments such as
//! `slack.t0123abc.u9xyz`, `telegram.5551234`, or `tel.16034700182`.
//!
//! The canonical form is deliberately restrictive — each segment is `[a-z0-9]+` with no separator
//! characters inside it — so the dotted string parses back unambiguously and the whole value
//! satisfies [`validate_identifier`](crate::IdentifierError)'s grammar. That makes a canonical
//! subject safe everywhere an identifier is safe: configuration keys, audit fields, and prefix
//! scopes.
//!
//! A subject is *routing metadata*, not authority. Trust in a subject comes entirely from the
//! transport that authenticated it and the broker-side owner-controlled mapping that resolves it
//! to a principal; nothing about the value itself is credible.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

use crate::MAX_IDENTIFIER_LENGTH;

/// The external service a subject was authenticated by.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum SubjectService {
    /// A Slack workspace member; the tenant segment is the Slack team identifier.
    Slack,
    /// A Telegram account, identified by its numeric user identifier.
    Telegram,
    /// A telephone number in digits-only E.164 form (the `+` is stripped).
    Tel,
}

impl SubjectService {
    /// The canonical leading segment for this service.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Telegram => "telegram",
            Self::Tel => "tel",
        }
    }

    const fn requires_tenant(self) -> bool {
        matches!(self, Self::Slack)
    }
}

impl FromStr for SubjectService {
    type Err = SubjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "slack" => Ok(Self::Slack),
            "telegram" => Ok(Self::Telegram),
            "tel" => Ok(Self::Tel),
            _ => Err(SubjectError::UnknownService {
                service: value.to_owned(),
            }),
        }
    }
}

/// One authenticated external identity in canonical form.
///
/// Constructed through the per-service constructors (which normalize raw transport identifiers)
/// or parsed from the canonical dotted string. Serde delegates to [`FromStr`], so deserialization
/// cannot produce a non-canonical value.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExternalSubject {
    service: SubjectService,
    tenant: Option<String>,
    subject: String,
}

impl ExternalSubject {
    /// A Slack workspace member: `slack.<team>.<user>`, both segments lowercased.
    pub fn slack(team: &str, user: &str) -> Result<Self, SubjectError> {
        let tenant = normalize_segment(team, "tenant")?;
        let subject = normalize_segment(user, "subject")?;
        Self::build(SubjectService::Slack, Some(tenant), subject)
    }

    /// A Telegram account: `telegram.<user id>`.
    pub fn telegram(user: &str) -> Result<Self, SubjectError> {
        let subject = normalize_segment(user, "subject")?;
        Self::build(SubjectService::Telegram, None, subject)
    }

    /// A telephone number: `tel.<digits>`, with one leading `+` stripped.
    pub fn telephone(number: &str) -> Result<Self, SubjectError> {
        let digits = number.strip_prefix('+').unwrap_or(number);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SubjectError::InvalidSegment {
                segment: "subject",
                value: number.to_owned(),
            });
        }
        Self::build(SubjectService::Tel, None, digits.to_owned())
    }

    fn build(
        service: SubjectService,
        tenant: Option<String>,
        subject: String,
    ) -> Result<Self, SubjectError> {
        let candidate = Self {
            service,
            tenant,
            subject,
        };
        if candidate.canonical().len() > MAX_IDENTIFIER_LENGTH {
            return Err(SubjectError::TooLong {
                maximum: MAX_IDENTIFIER_LENGTH,
            });
        }
        Ok(candidate)
    }

    /// The authenticated transport service.
    #[must_use]
    pub fn service(&self) -> SubjectService {
        self.service
    }

    /// The service-scoped tenant segment, when the service has one (Slack's team).
    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// The per-tenant subject segment.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Renders the canonical dotted form.
    #[must_use]
    pub fn canonical(&self) -> String {
        match &self.tenant {
            Some(tenant) => format!("{}.{tenant}.{}", self.service.as_str(), self.subject),
            None => format!("{}.{}", self.service.as_str(), self.subject),
        }
    }

    /// Whether this subject falls inside a canonical-prefix namespace scope.
    ///
    /// Matching is segment-boundary exact: `slack.t0123abc` covers `slack.t0123abc.u9xyz` but not
    /// `slack.t0123abcx.u9`. A scope equal to the whole canonical form also matches.
    #[must_use]
    pub fn in_namespace(&self, scope: &str) -> bool {
        let canonical = self.canonical();
        canonical == scope
            || (canonical.len() > scope.len()
                && canonical.starts_with(scope)
                && canonical.as_bytes()[scope.len()] == b'.')
    }
}

impl fmt::Display for ExternalSubject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.canonical())
    }
}

impl FromStr for ExternalSubject {
    type Err = SubjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut segments = value.split('.');
        let service = segments
            .next()
            .filter(|segment| !segment.is_empty())
            .ok_or(SubjectError::Empty)?
            .parse::<SubjectService>()?;
        let second = segments
            .next()
            .ok_or(SubjectError::MissingSegment { segment: "subject" })?;
        let third = segments.next();
        if segments.next().is_some() {
            return Err(SubjectError::TooManySegments);
        }
        let (tenant, subject) = if service.requires_tenant() {
            let subject = third.ok_or(SubjectError::MissingSegment { segment: "subject" })?;
            (Some(second), subject)
        } else {
            if third.is_some() {
                return Err(SubjectError::TooManySegments);
            }
            (None, second)
        };
        let tenant = tenant
            .map(|tenant| require_canonical_segment(tenant, "tenant"))
            .transpose()?;
        let subject = require_canonical_segment(subject, "subject")?;
        if service == SubjectService::Tel && !subject.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SubjectError::InvalidSegment {
                segment: "subject",
                value: subject,
            });
        }
        Self::build(service, tenant, subject)
    }
}

impl Serialize for ExternalSubject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical())
    }
}

impl<'de> Deserialize<'de> for ExternalSubject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// Lowercases a raw transport identifier and requires the canonical segment alphabet.
fn normalize_segment(value: &str, segment: &'static str) -> Result<String, SubjectError> {
    let normalized = value.to_ascii_lowercase();
    require_canonical_segment(&normalized, segment)
}

fn require_canonical_segment(
    value: impl Into<String>,
    segment: &'static str,
) -> Result<String, SubjectError> {
    let value = value.into();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(SubjectError::InvalidSegment { segment, value });
    }
    Ok(value)
}

/// A raw or canonical subject that could not be represented.
///
/// Variants echo segment *values* only for segments that are routing metadata by definition;
/// nothing here is secret.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SubjectError {
    /// The canonical form was empty.
    #[error("external subject must not be empty")]
    Empty,
    /// The leading segment named no known service.
    #[error("unknown external subject service {service:?}")]
    UnknownService {
        /// The unrecognized service segment.
        service: String,
    },
    /// A required segment was absent.
    #[error("external subject is missing its {segment} segment")]
    MissingSegment {
        /// Which segment was missing.
        segment: &'static str,
    },
    /// More segments than the service's canonical form defines.
    #[error("external subject has more segments than its service defines")]
    TooManySegments,
    /// A segment failed the canonical `[a-z0-9]+` alphabet (or digits-only for `tel`).
    #[error("external subject {segment} segment {value:?} is not canonical")]
    InvalidSegment {
        /// Which segment was invalid.
        segment: &'static str,
        /// The offending value.
        value: String,
    },
    /// The canonical form exceeded the identifier length bound.
    #[error("external subject exceeds {maximum} bytes")]
    TooLong {
        /// Maximum canonical length.
        maximum: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::{ExternalSubject, SubjectService};

    #[test]
    fn raw_transport_identifiers_normalize_into_canonical_form() {
        let slack = ExternalSubject::slack("T0123ABC", "U9XYZ").expect("slack subject");
        assert_eq!(slack.canonical(), "slack.t0123abc.u9xyz");
        assert_eq!(slack.service(), SubjectService::Slack);
        assert_eq!(slack.tenant(), Some("t0123abc"));

        let tel = ExternalSubject::telephone("+16034700182").expect("telephone subject");
        assert_eq!(tel.canonical(), "tel.16034700182");
        assert_eq!(tel.tenant(), None);

        let telegram = ExternalSubject::telegram("5551234").expect("telegram subject");
        assert_eq!(telegram.canonical(), "telegram.5551234");
    }

    #[test]
    fn canonical_forms_round_trip_through_parse_and_serde() {
        for canonical in [
            "slack.t0123abc.u9xyz",
            "telegram.5551234",
            "tel.16034700182",
        ] {
            let subject = canonical
                .parse::<ExternalSubject>()
                .expect("canonical form parses");
            assert_eq!(subject.canonical(), canonical);
            let encoded = serde_json::to_string(&subject).expect("serializes");
            assert_eq!(encoded, format!("{canonical:?}"));
            let decoded = serde_json::from_str::<ExternalSubject>(&encoded).expect("deserializes");
            assert_eq!(decoded, subject);
        }
    }

    #[test]
    fn canonical_subjects_satisfy_the_identifier_grammar() {
        // The whole point of the canonical form: a subject is safe anywhere an identifier is.
        for canonical in [
            "slack.t0123abc.u9xyz",
            "telegram.5551234",
            "tel.16034700182",
        ] {
            canonical
                .parse::<crate::PrincipalId>()
                .expect("canonical subjects fit the identifier grammar");
        }
    }

    #[test]
    fn malformed_subjects_fail_closed() {
        for invalid in [
            "",
            "slack",
            "slack.t0123abc",
            "slack.t0123abc.u9xyz.extra",
            "telegram.5551234.extra",
            "tel.not-digits",
            "tel.+1603",
            "sms.5551234",
            "slack..u9xyz",
            "slack.T0123.u9",
        ] {
            assert!(
                invalid.parse::<ExternalSubject>().is_err(),
                "{invalid:?} must not parse"
            );
        }
        assert!(ExternalSubject::slack("team space", "user").is_err());
        assert!(ExternalSubject::telephone("call-me").is_err());
    }

    #[test]
    fn namespace_scopes_match_on_segment_boundaries_only() {
        let subject = "slack.t0123abc.u9xyz"
            .parse::<ExternalSubject>()
            .expect("canonical form parses");
        assert!(subject.in_namespace("slack"));
        assert!(subject.in_namespace("slack.t0123abc"));
        assert!(subject.in_namespace("slack.t0123abc.u9xyz"));
        assert!(!subject.in_namespace("slack.t0123abcx"));
        assert!(!subject.in_namespace("slack.t0123ab"));
        assert!(!subject.in_namespace("tel"));
    }
}
