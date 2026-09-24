//! A subject is routing metadata, not authority; trust comes only from the transport that
//! authenticated it and the broker's owner-controlled mapping to a principal.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

use crate::MAX_IDENTIFIER_LENGTH;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum SubjectService {
    Slack,
    Discord,
    Telegram,
    Whatsapp,
    Tel,
}

impl SubjectService {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Whatsapp => "whatsapp",
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
            "discord" => Ok(Self::Discord),
            "telegram" => Ok(Self::Telegram),
            "whatsapp" => Ok(Self::Whatsapp),
            "tel" => Ok(Self::Tel),
            _ => Err(SubjectError::UnknownService {
                service: value.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExternalSubject {
    service: SubjectService,
    tenant: Option<String>,
    subject: String,
}

impl ExternalSubject {
    pub fn slack(team: &str, user: &str) -> Result<Self, SubjectError> {
        let tenant = normalize_segment(team, "tenant")?;
        let subject = normalize_segment(user, "subject")?;
        Self::build(SubjectService::Slack, Some(tenant), subject)
    }

    /// Discord user identifiers are global, not server-scoped, so unlike Slack there's no tenant
    /// segment; the guild is just routing context.
    pub fn discord(user: &str) -> Result<Self, SubjectError> {
        let subject = numeric_segment(user, "subject")?;
        Self::build(SubjectService::Discord, None, subject)
    }

    pub fn telegram(user: &str) -> Result<Self, SubjectError> {
        let subject = digits_segment(user, "subject")?;
        Self::build(SubjectService::Telegram, None, subject)
    }

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

    pub fn telephone(number: &str) -> Result<Self, SubjectError> {
        let digits = number.strip_prefix('+').unwrap_or(number);
        let subject = digits_segment(digits, "subject")?;
        Self::build(SubjectService::Tel, None, subject)
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

    #[must_use]
    pub fn service(&self) -> SubjectService {
        self.service
    }

    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    #[must_use]
    pub fn canonical(&self) -> String {
        match &self.tenant {
            Some(tenant) => format!("{}.{tenant}.{}", self.service.as_str(), self.subject),
            None => format!("{}.{}", self.service.as_str(), self.subject),
        }
    }

    /// Matching is segment-boundary exact: a scope covers its own subtree but never a sibling with
    /// a longer shared prefix, unlike a naive string check.
    #[must_use]
    pub fn in_namespace(&self, scope: &str) -> bool {
        let mut wanted = scope.split('.');
        for segment in self.segments() {
            match wanted.next() {
                None => return true,
                Some(value) if value == segment => {}
                Some(_) => return false,
            }
        }
        wanted.next().is_none()
    }

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
            SubjectService::Slack => true,
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

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SubjectError {
    #[error("external subject must not be empty")]
    Empty,
    #[error("unknown external subject service {service:?}")]
    UnknownService { service: String },
    #[error("external subject is missing its {segment} segment")]
    MissingSegment { segment: &'static str },
    #[error("external subject has more segments than its service defines")]
    TooManySegments,
    #[error("external subject {segment} segment {value:?} is not canonical")]
    InvalidSegment {
        segment: &'static str,
        value: String,
    },
    #[error("external subject exceeds {maximum} bytes")]
    TooLong { maximum: usize },
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
    fn a_service_with_no_authenticator_behind_it_is_not_a_service_here() {
        assert!(matches!(
            "dev.console.xavier".parse::<ExternalSubject>(),
            Err(SubjectError::UnknownService { .. })
        ));
    }

    #[test]
    fn numeric_services_reject_identifiers_a_transport_never_issued() {
        assert!("tel.notanumber".parse::<ExternalSubject>().is_err());
        assert!("telegram.abc".parse::<ExternalSubject>().is_err());
    }
}
