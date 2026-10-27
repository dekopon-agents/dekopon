//! A subject is routing metadata, not authority; trust comes only from the transport that
//! authenticated it and the broker's owner-controlled mapping to a principal.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use thiserror::Error;

use crate::MAX_IDENTIFIER_LENGTH;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PrincipalKind {
    Person,
    Space,
    Repository,
    Service,
}

impl PrincipalKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Space => "space",
            Self::Repository => "repository",
            Self::Service => "service",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum SubjectService {
    Fauxlack,
    Slack,
    Discord,
    Telegram,
    Whatsapp,
    Tel,
    Github,
}

impl SubjectService {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fauxlack => "fauxlack",
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Whatsapp => "whatsapp",
            Self::Tel => "tel",
            Self::Github => "github",
        }
    }

    #[must_use]
    pub const fn requires_tenant(self) -> bool {
        match self {
            Self::Fauxlack | Self::Slack | Self::Github => true,
            Self::Discord | Self::Telegram | Self::Whatsapp | Self::Tel => false,
        }
    }

    #[must_use]
    pub fn admits_segment(self, segment: SubjectSegment, value: &str) -> bool {
        match self {
            Self::Github => {
                let maximum = match segment {
                    SubjectSegment::Tenant => 39,
                    SubjectSegment::Subject => 100,
                };
                !value.is_empty()
                    && value.len() <= maximum
                    && value.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-')
                    })
            }
            Self::Fauxlack
            | Self::Slack
            | Self::Discord
            | Self::Telegram
            | Self::Whatsapp
            | Self::Tel => {
                !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectSegment {
    Tenant,
    Subject,
}

impl SubjectSegment {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Subject => "subject",
        }
    }
}

impl FromStr for SubjectService {
    type Err = SubjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fauxlack" => Ok(Self::Fauxlack),
            "slack" => Ok(Self::Slack),
            "discord" => Ok(Self::Discord),
            "telegram" => Ok(Self::Telegram),
            "whatsapp" => Ok(Self::Whatsapp),
            "tel" => Ok(Self::Tel),
            "github" => Ok(Self::Github),
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
        let tenant = normalize_segment(SubjectService::Slack, SubjectSegment::Tenant, team)?;
        let subject = normalize_segment(SubjectService::Slack, SubjectSegment::Subject, user)?;
        Self::build(SubjectService::Slack, Some(tenant), subject)
    }

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

    pub fn github(owner: &str, repo: &str) -> Result<Self, SubjectError> {
        let tenant = normalize_segment(SubjectService::Github, SubjectSegment::Tenant, owner)?;
        let subject = normalize_segment(SubjectService::Github, SubjectSegment::Subject, repo)?;
        Self::build(SubjectService::Github, Some(tenant), subject)
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
    pub fn kind(&self) -> PrincipalKind {
        match self.service {
            SubjectService::Fauxlack if self.subject.starts_with('c') => PrincipalKind::Space,
            SubjectService::Slack if self.subject.starts_with(['c', 'g']) => PrincipalKind::Space,
            SubjectService::Discord if self.tenant.is_some() => PrincipalKind::Space,
            SubjectService::Github => PrincipalKind::Repository,
            SubjectService::Fauxlack
            | SubjectService::Slack
            | SubjectService::Discord
            | SubjectService::Telegram
            | SubjectService::Whatsapp
            | SubjectService::Tel => PrincipalKind::Person,
        }
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
        let (tenant, subject) = if service.requires_tenant()
            || (service == SubjectService::Discord && third.is_some())
        {
            let subject = third.ok_or(SubjectError::MissingSegment { segment: "subject" })?;
            (Some(second), subject)
        } else {
            if third.is_some() {
                return Err(SubjectError::TooManySegments);
            }
            (None, second)
        };
        let tenant = tenant
            .map(|tenant| require_canonical_segment(service, SubjectSegment::Tenant, tenant))
            .transpose()?;
        if service == SubjectService::Discord
            && let Some(tenant) = &tenant
        {
            numeric_segment(tenant, "tenant")?;
        }
        let subject = require_canonical_segment(service, SubjectSegment::Subject, subject)?;
        let numeric = match service {
            SubjectService::Discord => is_discord_snowflake(&subject),
            SubjectService::Whatsapp => {
                !subject.starts_with('0') && subject.bytes().all(|byte| byte.is_ascii_digit())
            }
            SubjectService::Telegram | SubjectService::Tel => {
                subject.bytes().all(|byte| byte.is_ascii_digit())
            }
            SubjectService::Fauxlack | SubjectService::Slack | SubjectService::Github => true,
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

fn normalize_segment(
    service: SubjectService,
    segment: SubjectSegment,
    value: &str,
) -> Result<String, SubjectError> {
    require_canonical_segment(service, segment, value.to_ascii_lowercase())
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
    service: SubjectService,
    segment: SubjectSegment,
    value: impl Into<String>,
) -> Result<String, SubjectError> {
    let value = value.into();
    if !service.admits_segment(segment, &value) {
        return Err(SubjectError::InvalidSegment {
            segment: segment.as_str(),
            value,
        });
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
    fn github_owner_and_repo_are_bounded_canonical_segments() {
        let subject = ExternalSubject::github("Owner", "Repo_Name").expect("signed identifiers");
        assert_eq!(subject.canonical(), "github.owner.repo_name");
        assert_eq!(
            subject.canonical().parse::<ExternalSubject>(),
            Ok(subject.clone())
        );
        assert!(subject.in_namespace("github.owner"));
        assert!(!subject.in_namespace("github.other"));
        assert!(!ExternalSubject::github("owner", "repo.extra").is_ok());
        assert!(
            "github.owner.repo.extra"
                .parse::<ExternalSubject>()
                .is_err()
        );
        assert!(
            "github.owner.repo-extra"
                .parse::<ExternalSubject>()
                .unwrap()
                .in_namespace("github.owner")
        );
        assert!(
            !"github.owner.repo-extra"
                .parse::<ExternalSubject>()
                .unwrap()
                .in_namespace("github.owner.repo")
        );
        assert!(ExternalSubject::github(&"x".repeat(40), "repo").is_err());
        assert!(ExternalSubject::github("owner", &"x".repeat(101)).is_err());
    }

    #[test]
    fn an_invalid_discord_tenant_is_reported_before_the_subject() {
        for form in ["discord.0.123", "discord.0.0"] {
            assert_eq!(
                form.parse::<ExternalSubject>(),
                Err(super::SubjectError::InvalidSegment {
                    segment: "tenant",
                    value: "0".to_owned(),
                })
            );
        }
    }

    #[test]
    fn subject_kind_follows_the_subject_form() {
        use super::PrincipalKind;
        for (form, kind) in [
            ("slack.team.channel", PrincipalKind::Space),
            ("fauxlack.acme.cteam", PrincipalKind::Space),
            ("fauxlack.acme.ualice", PrincipalKind::Person),
            ("slack.team.group", PrincipalKind::Space),
            ("slack.team.user", PrincipalKind::Person),
            ("slack.team.worker", PrincipalKind::Person),
            ("discord.123.456", PrincipalKind::Space),
            ("discord.123", PrincipalKind::Person),
            ("github.owner.repo", PrincipalKind::Repository),
            ("telegram.123", PrincipalKind::Person),
            ("whatsapp.123", PrincipalKind::Person),
            ("tel.123", PrincipalKind::Person),
        ] {
            let subject: ExternalSubject = form.parse().expect("canonical subject");
            assert_eq!(subject.kind(), kind);
            assert_eq!(subject.canonical(), form);
        }
        assert!("discord.123.456.789".parse::<ExternalSubject>().is_err());
    }

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
            "fauxlack.acme.cteam",
            "fauxlack.acme.ualice",
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
            "fauxlack.acme.cteam",
            "fauxlack.acme.ualice",
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
            "fauxlack",
            "fauxlack.ualice",
            "fauxlack..ualice",
            "fauxlack.acme.ualice.extra",
            "fauxlack.Acme.ualice",
            "fauxlack.acme.u-alice",
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

        let fauxlack: ExternalSubject = "fauxlack.acme.ualice".parse().unwrap();
        assert!(fauxlack.in_namespace("fauxlack.acme"));
        assert!(!fauxlack.in_namespace("fauxlack.acm"));
        assert!(!fauxlack.in_namespace("slack.acme"));
        assert!(!subject.in_namespace("fauxlack"));

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
