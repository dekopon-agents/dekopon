//! External chat-transport subjects and their canonical, identifier-safe form.
//!
//! A subject names *who a message came from* on an external service — a Slack user inside a Slack
//! workspace, a Discord or Telegram account, a WhatsApp account, a telephone number, or a local
//! development identity that no service authenticated at all. Raw
//! external identifiers do not fit the workspace identifier grammar (Slack IDs are uppercase,
//! E.164 numbers start with `+`), so this type owns one canonical normalization: dotted lowercase
//! segments such as `slack.t0123abc.u9xyz`, `discord.123456789`, `telegram.5551234`,
//! `whatsapp.16034700182`, or `tel.16034700182`.
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
    /// A Discord account, identified globally by its numeric user identifier.
    Discord,
    /// A Telegram account, identified by its numeric user identifier.
    Telegram,
    /// A WhatsApp account, identified by the signed webhook `wa_id`.
    Whatsapp,
    /// A telephone number in digits-only E.164 form (the `+` is stripped).
    Tel,
    /// A local development identity: `dev.<surface>.<name>`, authenticated by nothing.
    ///
    /// This is the one service in this enum with no external authenticator behind it. The other
    /// five carry a name a real service verified before the message reached a transport; this one
    /// carries a name a local caller typed on an owner-only socket. It exists because the
    /// alternative — a development tool borrowing `tel.15550100000` — puts a value in
    /// `identityMappings`, in policy, and in the audit chain that reads like a phone number and is
    /// not one, and every later reader has to be told which of those are real.
    ///
    /// Because nothing authenticates it, a broker admits it only under an explicit opt-in; see
    /// `dekopon-brokerd`'s `allowDevelopmentSubjects`. Nothing here enforces that, exactly as
    /// nothing here enforces which Slack workspaces a deployment trusts: this type owns the
    /// canonical shape, and authority stays with the broker's owner-controlled configuration.
    ///
    /// The tenant segment names the surface that minted it — `dev.console.xavier` — so a grant can
    /// scope to one development surface without also admitting every other.
    Dev,
}

impl SubjectService {
    /// The canonical leading segment for this service.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Whatsapp => "whatsapp",
            Self::Tel => "tel",
            Self::Dev => "dev",
        }
    }

    const fn requires_tenant(self) -> bool {
        matches!(self, Self::Slack | Self::Dev)
    }

    /// Whether a real external service authenticated this subject before it reached a transport.
    ///
    /// The distinction a deployment acts on: an unauthenticated subject is a claim a local caller
    /// made, so a broker requires an explicit opt-in before it will resolve one to a principal.
    #[must_use]
    pub const fn is_authenticated_externally(self) -> bool {
        !matches!(self, Self::Dev)
    }
}

impl FromStr for SubjectService {
    type Err = SubjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "slack" => Ok(Self::Slack),
            "discord" => Ok(Self::Discord),
            "telegram" => Ok(Self::Telegram),
            "whatsapp" => Ok(Self::Whatsapp),
            "tel" => Ok(Self::Tel),
            "dev" => Ok(Self::Dev),
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

    /// A Discord account: `discord.<user id>`.
    ///
    /// Discord user identifiers are global rather than server-scoped, so the guild is routing
    /// context and not part of the authenticated subject.
    pub fn discord(user: &str) -> Result<Self, SubjectError> {
        let subject = numeric_segment(user, "subject")?;
        Self::build(SubjectService::Discord, None, subject)
    }

    /// A Telegram account: `telegram.<user id>`, all digits.
    pub fn telegram(user: &str) -> Result<Self, SubjectError> {
        let subject = digits_segment(user, "subject")?;
        Self::build(SubjectService::Telegram, None, subject)
    }

    /// A WhatsApp account: `whatsapp.<wa_id>`, using the signed digits-only sender identifier.
    pub fn whatsapp(wa_id: &str) -> Result<Self, SubjectError> {
        if wa_id.is_empty()
            || wa_id.starts_with('0')
            || !wa_id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(SubjectError::InvalidSegment {
                segment: "subject",
                value: wa_id.to_owned(),
            });
        }
        Self::build(SubjectService::Whatsapp, None, wa_id.to_owned())
    }

    /// A telephone number: `tel.<digits>`, with one leading `+` stripped.
    pub fn telephone(number: &str) -> Result<Self, SubjectError> {
        let digits = number.strip_prefix('+').unwrap_or(number);
        let subject = digits_segment(digits, "subject")?;
        Self::build(SubjectService::Tel, None, subject)
    }

    /// A local development identity: `dev.<surface>.<name>`, both segments lowercased.
    ///
    /// `surface` names the tool that minted it, such as `console`, so a deployment can grant one
    /// development surface without admitting every other. Constructing one asserts nothing: a
    /// broker resolves it to a principal only under an explicit opt-in and an owner-authored
    /// mapping, exactly as it does for a subject a real service authenticated.
    pub fn development(surface: &str, name: &str) -> Result<Self, SubjectError> {
        let tenant = normalize_segment(surface, "tenant")?;
        let subject = normalize_segment(name, "subject")?;
        Self::build(SubjectService::Dev, Some(tenant), subject)
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

    /// The service-native subject segment (per-tenant only for services that require one).
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
    ///
    /// Compared segment by segment rather than against a rendered canonical string: an attestor
    /// grant asks this once per configured namespace on every attested request, and the answer
    /// never needs the joined form.
    #[must_use]
    pub fn in_namespace(&self, scope: &str) -> bool {
        let mut wanted = scope.split('.');
        for segment in self.segments() {
            match wanted.next() {
                // The scope ran out exactly on a segment boundary, so it is a covering prefix.
                None => return true,
                Some(value) if value == segment => {}
                Some(_) => return false,
            }
        }
        wanted.next().is_none()
    }

    /// The canonical segments in order, without joining them.
    fn segments(&self) -> impl Iterator<Item = &str> {
        [
            Some(self.service.as_str()),
            self.tenant.as_deref(),
            Some(self.subject.as_str()),
        ]
        .into_iter()
        .flatten()
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
        let numeric = match service {
            SubjectService::Discord => is_discord_snowflake(&subject),
            SubjectService::Whatsapp => {
                !subject.starts_with('0') && subject.bytes().all(|byte| byte.is_ascii_digit())
            }
            SubjectService::Telegram | SubjectService::Tel => {
                subject.bytes().all(|byte| byte.is_ascii_digit())
            }
            SubjectService::Slack | SubjectService::Dev => true,
        };
        if !numeric {
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

fn digits_segment(value: &str, segment: &'static str) -> Result<String, SubjectError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SubjectError::InvalidSegment {
            segment,
            value: value.to_owned(),
        });
    }
    Ok(value.to_owned())
}

fn numeric_segment(value: &str, segment: &'static str) -> Result<String, SubjectError> {
    if !is_discord_snowflake(value) {
        return Err(SubjectError::InvalidSegment {
            segment,
            value: value.to_owned(),
        });
    }
    Ok(value.to_owned())
}

fn is_discord_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|parsed| parsed != 0 && parsed.to_string() == value)
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
    /// A segment failed the canonical alphabet or a service-specific numeric identifier rule.
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
    use super::{ExternalSubject, SubjectError, SubjectService};

    #[test]
    fn raw_transport_identifiers_normalize_into_canonical_form() {
        let slack = ExternalSubject::slack("T0123ABC", "U9XYZ").expect("slack subject");
        assert_eq!(slack.canonical(), "slack.t0123abc.u9xyz");
        assert_eq!(slack.service(), SubjectService::Slack);
        assert_eq!(slack.tenant(), Some("t0123abc"));

        let discord = ExternalSubject::discord("123456789012345678").expect("discord subject");
        assert_eq!(discord.canonical(), "discord.123456789012345678");
        assert_eq!(discord.service(), SubjectService::Discord);
        assert_eq!(discord.tenant(), None);

        let whatsapp = ExternalSubject::whatsapp("16034700182").expect("WhatsApp subject");
        assert_eq!(whatsapp.canonical(), "whatsapp.16034700182");
        assert_eq!(whatsapp.service(), SubjectService::Whatsapp);

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
            "discord.123456789012345678",
            "telegram.5551234",
            "whatsapp.16034700182",
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
            "discord.123456789012345678",
            "telegram.5551234",
            "whatsapp.16034700182",
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
            "discord.not-numeric",
            "discord.0",
            "discord.00123",
            "discord.18446744073709551616",
            "discord.123.extra",
            "telegram.5551234.extra",
            // An identityMappings typo, refused at broker startup rather than accepted as a
            // canonical subject no transport can ever produce.
            "telegram.alice",
            "telegram.abc123",
            "whatsapp.not-digits",
            "whatsapp.1603.extra",
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
        assert!(ExternalSubject::discord("not-a-snowflake").is_err());
        assert!(ExternalSubject::whatsapp("+1603").is_err());
        assert!(ExternalSubject::whatsapp("01603").is_err());
        assert!(ExternalSubject::telephone("call-me").is_err());
        assert!(ExternalSubject::telegram("alice").is_err());
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
        // A scope is never a partial segment, an empty string, or longer than the subject itself.
        assert!(!subject.in_namespace(""));
        assert!(!subject.in_namespace("slack."));
        assert!(!subject.in_namespace("slack.t0123abc."));
        assert!(!subject.in_namespace("slack.t0123abc.u9xyz.extra"));
        assert!(!subject.in_namespace(".slack"));

        let tenantless = "tel.16034700182"
            .parse::<ExternalSubject>()
            .expect("canonical form parses");
        assert!(tenantless.in_namespace("tel"));
        assert!(tenantless.in_namespace("tel.16034700182"));
        assert!(!tenantless.in_namespace("tel.1603470018"));
        assert!(!tenantless.in_namespace("tel.16034700182.extra"));
    }

    #[test]
    fn a_development_subject_is_canonical_and_tenanted() {
        let subject = ExternalSubject::development("Console", "Xavier").expect("dev subject");
        assert_eq!(subject.canonical(), "dev.console.xavier");
        assert_eq!(subject.service(), SubjectService::Dev);
        assert_eq!(subject.tenant(), Some("console"));
        assert_eq!(subject.subject(), "xavier");
        assert_eq!(
            "dev.console.xavier"
                .parse::<ExternalSubject>()
                .expect("round trips"),
            subject
        );
    }

    #[test]
    fn a_development_subject_names_the_surface_that_minted_it() {
        // The tenant segment is what lets a grant admit one development surface without admitting
        // every other, exactly as Slack's team segment does.
        let console = ExternalSubject::development("console", "xavier").expect("dev subject");
        assert!(console.in_namespace("dev.console"));
        assert!(console.in_namespace("dev"));
        assert!(!console.in_namespace("dev.ci"));
        assert!(!console.in_namespace("dev.consolex"));
    }

    #[test]
    fn a_development_subject_needs_both_segments() {
        assert!(matches!(
            "dev.xavier".parse::<ExternalSubject>(),
            Err(SubjectError::MissingSegment { segment: "subject" })
        ));
        assert!(ExternalSubject::development("", "xavier").is_err());
        assert!(ExternalSubject::development("console", "").is_err());
        assert!(
            ExternalSubject::development("con sole", "xavier").is_err(),
            "the canonical segment alphabet has no separators inside a segment"
        );
    }

    #[test]
    fn only_the_development_service_lacks_an_external_authenticator() {
        // The distinction a broker acts on before it will resolve a subject to a principal.
        assert!(!SubjectService::Dev.is_authenticated_externally());
        for service in [
            SubjectService::Slack,
            SubjectService::Discord,
            SubjectService::Telegram,
            SubjectService::Whatsapp,
            SubjectService::Tel,
        ] {
            assert!(
                service.is_authenticated_externally(),
                "{service:?} is authenticated by a real service"
            );
        }
    }

    #[test]
    fn a_development_subject_cannot_impersonate_a_numeric_service() {
        // `dev` is not numeric-constrained, but that laxity must not leak into the services whose
        // identifiers a real transport verified.
        assert!("dev.console.a1b2".parse::<ExternalSubject>().is_ok());
        assert!("tel.notanumber".parse::<ExternalSubject>().is_err());
        assert!("telegram.abc".parse::<ExternalSubject>().is_err());
    }
}
