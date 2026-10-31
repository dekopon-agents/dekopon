use std::{fmt, str::FromStr};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::SerializeSeq as _,
};
use thiserror::Error;

use crate::MAX_IDENTIFIER_LENGTH;

const DRN_PREFIX: &str = "drn";
const WILDCARD: &str = "*";

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
pub enum Namespace {
    Fauxlack,
    Slack,
    Discord,
    Telegram,
    Whatsapp,
    Tel,
    Github,
}

impl Namespace {
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
    pub const fn kinds(self) -> &'static [SubjectKind] {
        match self {
            Self::Fauxlack => &[SubjectKind::FauxlackUser, SubjectKind::FauxlackChannel],
            Self::Slack => &[SubjectKind::SlackUser, SubjectKind::SlackChannel],
            Self::Discord => &[SubjectKind::DiscordUser, SubjectKind::DiscordChannel],
            Self::Telegram => &[SubjectKind::TelegramUser],
            Self::Whatsapp => &[SubjectKind::WhatsappNumber],
            Self::Tel => &[SubjectKind::TelNumber],
            Self::Github => &[
                SubjectKind::GithubRepo,
                SubjectKind::GithubRepoId,
                SubjectKind::GithubUser,
                SubjectKind::GithubUserId,
            ],
        }
    }

    const fn reserves(self, kind: &str) -> bool {
        match self {
            Self::Whatsapp => matches!(kind.as_bytes(), b"user" | b"user-id"),
            Self::Fauxlack
            | Self::Slack
            | Self::Discord
            | Self::Telegram
            | Self::Tel
            | Self::Github => false,
        }
    }

    fn kind(self, field: &str) -> Result<SubjectKind, SubjectError> {
        if self.reserves(field) {
            return Err(SubjectError::ReservedKind {
                namespace: self,
                kind: field.to_owned(),
            });
        }
        self.kinds()
            .iter()
            .copied()
            .find(|kind| kind.as_str() == field)
            .ok_or_else(|| SubjectError::UnknownKind {
                namespace: self,
                kind: field.to_owned(),
            })
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Namespace {
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
            _ => Err(SubjectError::UnknownNamespace {
                namespace: value.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SubjectKind {
    FauxlackUser,
    FauxlackChannel,
    SlackUser,
    SlackChannel,
    DiscordUser,
    DiscordChannel,
    TelegramUser,
    WhatsappNumber,
    TelNumber,
    GithubRepo,
    GithubRepoId,
    GithubUser,
    GithubUserId,
}

impl SubjectKind {
    #[must_use]
    pub const fn namespace(self) -> Namespace {
        match self {
            Self::FauxlackUser | Self::FauxlackChannel => Namespace::Fauxlack,
            Self::SlackUser | Self::SlackChannel => Namespace::Slack,
            Self::DiscordUser | Self::DiscordChannel => Namespace::Discord,
            Self::TelegramUser => Namespace::Telegram,
            Self::WhatsappNumber => Namespace::Whatsapp,
            Self::TelNumber => Namespace::Tel,
            Self::GithubRepo | Self::GithubRepoId | Self::GithubUser | Self::GithubUserId => {
                Namespace::Github
            }
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FauxlackUser
            | Self::SlackUser
            | Self::DiscordUser
            | Self::TelegramUser
            | Self::GithubUser => "user",
            Self::FauxlackChannel | Self::SlackChannel | Self::DiscordChannel => "channel",
            Self::WhatsappNumber | Self::TelNumber => "number",
            Self::GithubRepo => "repo",
            Self::GithubRepoId => "repo-id",
            Self::GithubUserId => "user-id",
        }
    }

    #[must_use]
    pub const fn principal_kind(self) -> PrincipalKind {
        match self {
            Self::FauxlackUser
            | Self::SlackUser
            | Self::DiscordUser
            | Self::TelegramUser
            | Self::WhatsappNumber
            | Self::TelNumber
            | Self::GithubUser
            | Self::GithubUserId => PrincipalKind::Person,
            Self::FauxlackChannel | Self::SlackChannel | Self::DiscordChannel => {
                PrincipalKind::Space
            }
            Self::GithubRepo | Self::GithubRepoId => PrincipalKind::Repository,
        }
    }

    #[must_use]
    pub const fn is_id_form(self) -> bool {
        match self {
            Self::GithubRepoId | Self::GithubUserId => true,
            Self::FauxlackUser
            | Self::FauxlackChannel
            | Self::SlackUser
            | Self::SlackChannel
            | Self::DiscordUser
            | Self::DiscordChannel
            | Self::TelegramUser
            | Self::WhatsappNumber
            | Self::TelNumber
            | Self::GithubRepo
            | Self::GithubUser => false,
        }
    }

    const fn tenant_rule(self) -> Field {
        match self {
            Self::FauxlackUser | Self::FauxlackChannel => Field::Token { maximum: 63 },
            Self::SlackUser | Self::SlackChannel => Field::Token { maximum: 32 },
            Self::DiscordChannel => Field::Decimal,
            Self::GithubRepo => Field::GithubLogin,
            Self::DiscordUser
            | Self::TelegramUser
            | Self::WhatsappNumber
            | Self::TelNumber
            | Self::GithubRepoId
            | Self::GithubUser
            | Self::GithubUserId => Field::Empty,
        }
    }

    const fn id_rule(self) -> Field {
        match self {
            Self::FauxlackUser | Self::FauxlackChannel => Field::Token { maximum: 63 },
            Self::SlackUser | Self::SlackChannel => Field::Token { maximum: 32 },
            Self::DiscordUser
            | Self::DiscordChannel
            | Self::TelegramUser
            | Self::GithubRepoId
            | Self::GithubUserId => Field::Decimal,
            Self::WhatsappNumber | Self::TelNumber => Field::Phone,
            Self::GithubRepo => Field::GithubRepoName,
            Self::GithubUser => Field::GithubLogin,
        }
    }

    #[must_use]
    pub fn admits_tenant(self, value: &str) -> bool {
        self.tenant_rule().admits(value)
    }

    #[must_use]
    pub fn admits_id(self, value: &str) -> bool {
        self.id_rule().admits(value)
    }
}

impl fmt::Display for SubjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.namespace(), self.as_str())
    }
}

#[derive(Clone, Copy)]
enum Field {
    Empty,
    Token { maximum: usize },
    Decimal,
    Phone,
    GithubLogin,
    GithubRepoName,
}

impl Field {
    fn admits(self, value: &str) -> bool {
        match self {
            Self::Empty => value.is_empty(),
            Self::Token { maximum } => {
                bounded(value, maximum)
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            }
            Self::Decimal => value
                .parse::<u64>()
                .is_ok_and(|parsed| parsed != 0 && parsed.to_string() == value),
            Self::Phone => {
                bounded(value, 15)
                    && !value.starts_with('0')
                    && value.bytes().all(|byte| byte.is_ascii_digit())
            }
            Self::GithubLogin => {
                bounded(value, 39)
                    && value.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            }
            Self::GithubRepoName => {
                bounded(value, 100)
                    && value != "."
                    && value != ".."
                    && value.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'.' | b'_' | b'-')
                    })
            }
        }
    }
}

const fn bounded(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectField {
    Tenant,
    Id,
}

impl SubjectField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Id => "id",
        }
    }
}

impl fmt::Display for SubjectField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

struct Fields<'a> {
    namespace: &'a str,
    kind: &'a str,
    tenant: &'a str,
    id: &'a str,
}

impl<'a> Fields<'a> {
    fn split(folded: &'a str) -> Result<Self, SubjectError> {
        if folded.len() > MAX_IDENTIFIER_LENGTH {
            return Err(SubjectError::TooLong {
                maximum: MAX_IDENTIFIER_LENGTH,
            });
        }
        let mut parts = folded.split(':');
        if parts.next() != Some(DRN_PREFIX) {
            return Err(SubjectError::NotDrn);
        }
        let (Some(namespace), Some(kind), Some(tenant), Some(id), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(SubjectError::FieldCount);
        };
        Ok(Self {
            namespace,
            kind,
            tenant,
            id,
        })
    }

    fn namespace(&self) -> Result<Namespace, SubjectError> {
        if self.namespace == WILDCARD {
            return Err(SubjectError::WildcardNamespace);
        }
        self.namespace.parse()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Subject {
    kind: SubjectKind,
    tenant: String,
    id: String,
}

impl Subject {
    pub fn new(kind: SubjectKind, tenant: &str, id: &str) -> Result<Self, SubjectError> {
        let tenant = tenant.to_ascii_lowercase();
        let id = id.to_ascii_lowercase();
        if tenant.contains('*') || id.contains('*') {
            return Err(SubjectError::WildcardOutsideScope);
        }
        if !kind.admits_tenant(&tenant) {
            return Err(SubjectError::InvalidField {
                field: SubjectField::Tenant,
                value: tenant,
            });
        }
        if !kind.admits_id(&id) {
            return Err(SubjectError::InvalidField {
                field: SubjectField::Id,
                value: id,
            });
        }
        let subject = Self { kind, tenant, id };
        if subject.canonical().len() > MAX_IDENTIFIER_LENGTH {
            return Err(SubjectError::TooLong {
                maximum: MAX_IDENTIFIER_LENGTH,
            });
        }
        Ok(subject)
    }

    pub fn fauxlack_user(server: &str, id: &str) -> Result<Self, SubjectError> {
        Self::new(SubjectKind::FauxlackUser, server, id)
    }

    pub fn slack_user(team: &str, user: &str) -> Result<Self, SubjectError> {
        Self::new(SubjectKind::SlackUser, team, user)
    }

    pub fn discord_user(user: &str) -> Result<Self, SubjectError> {
        Self::new(SubjectKind::DiscordUser, "", user)
    }

    pub fn telegram_user(user: &str) -> Result<Self, SubjectError> {
        Self::new(SubjectKind::TelegramUser, "", user)
    }

    pub fn whatsapp_number(wa_id: &str) -> Result<Self, SubjectError> {
        Self::new(SubjectKind::WhatsappNumber, "", wa_id)
    }

    pub fn github_repo(owner: &str, name: &str) -> Result<Self, SubjectError> {
        Self::new(SubjectKind::GithubRepo, owner, name)
    }

    pub fn telephone(number: &str) -> Result<Self, SubjectError> {
        Self::new(
            SubjectKind::TelNumber,
            "",
            number.strip_prefix('+').unwrap_or(number),
        )
    }

    #[must_use]
    pub const fn kind(&self) -> SubjectKind {
        self.kind
    }

    #[must_use]
    pub const fn namespace(&self) -> Namespace {
        self.kind.namespace()
    }

    #[must_use]
    pub const fn principal_kind(&self) -> PrincipalKind {
        self.kind.principal_kind()
    }

    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        (!self.tenant.is_empty()).then_some(self.tenant.as_str())
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn canonical(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{DRN_PREFIX}:{}:{}:{}:{}",
            self.kind.namespace(),
            self.kind.as_str(),
            self.tenant,
            self.id
        )
    }
}

impl FromStr for Subject {
    type Err = SubjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let folded = value.to_ascii_lowercase();
        let fields = Fields::split(&folded)?;
        if [fields.kind, fields.tenant, fields.id]
            .iter()
            .any(|field| field.contains('*'))
        {
            return Err(SubjectError::WildcardOutsideScope);
        }
        let kind = fields.namespace()?.kind(fields.kind)?;
        Self::new(kind, fields.tenant, fields.id)
    }
}

impl Serialize for Subject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Subject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SubjectScope {
    namespace: Namespace,
    kind: Option<SubjectKind>,
    tenant: Option<String>,
    id: Option<String>,
}

impl SubjectScope {
    #[must_use]
    pub fn covers(&self, subject: &Subject) -> bool {
        self.namespace == subject.namespace()
            && self.kind.is_none_or(|kind| kind == subject.kind)
            && self
                .tenant
                .as_deref()
                .is_none_or(|tenant| tenant == subject.tenant)
            && self.id.as_deref().is_none_or(|id| id == subject.id)
    }

    #[must_use]
    pub const fn names_every_kind(&self) -> bool {
        self.kind.is_none()
    }

    #[must_use]
    pub fn kinds(&self) -> Vec<SubjectKind> {
        self.namespace
            .kinds()
            .iter()
            .copied()
            .filter(|kind| self.kind.is_none_or(|wanted| wanted == *kind))
            .filter(|kind| {
                self.tenant
                    .as_deref()
                    .is_none_or(|tenant| kind.admits_tenant(tenant))
                    && self.id.as_deref().is_none_or(|id| kind.admits_id(id))
            })
            .collect()
    }
}

fn scope_field(value: &str) -> Result<Option<&str>, SubjectError> {
    if value == WILDCARD {
        Ok(None)
    } else if value.contains('*') {
        Err(SubjectError::PartialWildcard {
            value: value.to_owned(),
        })
    } else {
        Ok(Some(value))
    }
}

impl fmt::Display for SubjectScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{DRN_PREFIX}:{}:{}:{}:{}",
            self.namespace,
            self.kind.map_or(WILDCARD, SubjectKind::as_str),
            self.tenant.as_deref().unwrap_or(WILDCARD),
            self.id.as_deref().unwrap_or(WILDCARD)
        )
    }
}

impl FromStr for SubjectScope {
    type Err = SubjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let folded = value.to_ascii_lowercase();
        let fields = Fields::split(&folded)?;
        let namespace = fields.namespace()?;
        let kind = scope_field(fields.kind)?
            .map(|kind| namespace.kind(kind))
            .transpose()?;
        let tenant = scope_field(fields.tenant)?;
        let id = scope_field(fields.id)?;
        if let Some(kind) = kind {
            for (field, value, admitted) in [
                (
                    SubjectField::Tenant,
                    tenant,
                    tenant.is_none_or(|t| kind.admits_tenant(t)),
                ),
                (SubjectField::Id, id, id.is_none_or(|i| kind.admits_id(i))),
            ] {
                if !admitted {
                    return Err(SubjectError::InvalidField {
                        field,
                        value: value.unwrap_or_default().to_owned(),
                    });
                }
            }
        }
        let scope = Self {
            namespace,
            kind,
            tenant: tenant.map(str::to_owned),
            id: id.map(str::to_owned),
        };
        if scope.kinds().is_empty() {
            return Err(SubjectError::ScopeAdmitsNoKind {
                scope: scope.to_string(),
            });
        }
        Ok(scope)
    }
}

impl Serialize for SubjectScope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SubjectScope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AttestedSubject(Forms);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Forms {
    One(Subject),
    Twin { name: Subject, id: Subject },
}

impl AttestedSubject {
    pub fn one(subject: Subject) -> Result<Self, SubjectError> {
        match subject.namespace() {
            Namespace::Github => Err(SubjectError::GithubNeedsTwin),
            Namespace::Fauxlack
            | Namespace::Slack
            | Namespace::Discord
            | Namespace::Telegram
            | Namespace::Whatsapp
            | Namespace::Tel => Ok(Self(Forms::One(subject))),
        }
    }

    // Never decoded from a peer: the wire refuses a single GitHub form.
    #[must_use]
    pub const fn configured(subject: Subject) -> Self {
        Self(Forms::One(subject))
    }

    pub fn github_repository(owner: &str, name: &str, id: u64) -> Result<Self, SubjectError> {
        Self::paired(
            Subject::new(SubjectKind::GithubRepo, owner, name)?,
            Subject::new(SubjectKind::GithubRepoId, "", &id.to_string())?,
        )
    }

    pub fn github_user(login: &str, id: u64) -> Result<Self, SubjectError> {
        Self::paired(
            Subject::new(SubjectKind::GithubUser, "", login)?,
            Subject::new(SubjectKind::GithubUserId, "", &id.to_string())?,
        )
    }

    pub fn paired(name: Subject, id: Subject) -> Result<Self, SubjectError> {
        match (name.kind, id.kind) {
            (SubjectKind::GithubRepo, SubjectKind::GithubRepoId)
            | (SubjectKind::GithubUser, SubjectKind::GithubUserId) => {
                Ok(Self(Forms::Twin { name, id }))
            }
            _ => Err(SubjectError::Unpaired),
        }
    }

    #[must_use]
    pub const fn name(&self) -> &Subject {
        match &self.0 {
            Forms::One(subject) | Forms::Twin { name: subject, .. } => subject,
        }
    }

    #[must_use]
    pub const fn id_form(&self) -> Option<&Subject> {
        match &self.0 {
            Forms::One(_) => None,
            Forms::Twin { id, .. } => Some(id),
        }
    }

    pub fn forms(&self) -> impl Iterator<Item = &Subject> {
        std::iter::once(self.name()).chain(self.id_form())
    }
}

impl fmt::Display for AttestedSubject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.name(), formatter)
    }
}

impl Serialize for AttestedSubject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut forms = serializer.serialize_seq(None)?;
        for form in self.forms() {
            forms.serialize_element(form)?;
        }
        forms.end()
    }
}

impl<'de> Deserialize<'de> for AttestedSubject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = AttestedSubject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one subject DRN, or a GitHub name and id pair")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let first: Subject = seq
                    .next_element()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let second: Option<Subject> = seq.next_element()?;
                if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(A::Error::invalid_length(3, &self));
                }
                match second {
                    None => AttestedSubject::one(first),
                    Some(second) => AttestedSubject::paired(first, second),
                }
                .map_err(A::Error::custom)
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SubjectError {
    #[error("subject must be a drn: name")]
    NotDrn,
    #[error("subject must have exactly the fields drn:<namespace>:<kind>:<tenant>:<id>")]
    FieldCount,
    #[error("unknown subject namespace {namespace:?}")]
    UnknownNamespace { namespace: String },
    #[error("namespace {namespace} has no subject kind {kind:?}")]
    UnknownKind { namespace: Namespace, kind: String },
    #[error("subject kind {namespace}:{kind} is reserved")]
    ReservedKind { namespace: Namespace, kind: String },
    #[error("subject {field} {value:?} is not valid for its kind")]
    InvalidField { field: SubjectField, value: String },
    #[error("`*` is allowed only in a subject scope")]
    WildcardOutsideScope,
    #[error("`*` must replace a whole field, not part of {value:?}")]
    PartialWildcard { value: String },
    #[error("`*` cannot replace the namespace")]
    WildcardNamespace,
    #[error("scope {scope} admits no subject kind")]
    ScopeAdmitsNoKind { scope: String },
    #[error("a GitHub subject is attested as a name and id pair")]
    GithubNeedsTwin,
    #[error("an attested pair must be a GitHub name and its matching id kind")]
    Unpaired,
    #[error("subject exceeds {maximum} bytes")]
    TooLong { maximum: usize },
}

#[cfg(test)]
mod tests {
    use super::{AttestedSubject, PrincipalKind, Subject, SubjectError, SubjectKind, SubjectScope};

    fn subject(value: &str) -> Subject {
        value.parse().expect("canonical subject")
    }

    fn scope(value: &str) -> SubjectScope {
        value.parse().expect("valid scope")
    }

    #[test]
    fn every_transport_subject_round_trips_and_names_its_principal_kind() {
        for (form, kind) in [
            (
                "drn:slack:user:t045p84s5:u07a1mjhnux",
                PrincipalKind::Person,
            ),
            (
                "drn:slack:channel:t0brehnayhw:c0cdh8pfhkk",
                PrincipalKind::Space,
            ),
            (
                "drn:discord:user::578258790881951745",
                PrincipalKind::Person,
            ),
            (
                "drn:discord:channel:1548494885361946626:1548494886146150451",
                PrincipalKind::Space,
            ),
            ("drn:telegram:user::5551234", PrincipalKind::Person),
            ("drn:whatsapp:number::16034700182", PrincipalKind::Person),
            ("drn:tel:number::16034700182", PrincipalKind::Person),
            (
                "drn:github:repo:dekopon-agents:dekopon",
                PrincipalKind::Repository,
            ),
            (
                "drn:github:repo:dekopon-agents:dekopon-agents.github.io",
                PrincipalKind::Repository,
            ),
            ("drn:github:repo-id::42", PrincipalKind::Repository),
            ("drn:github:user::octocat", PrincipalKind::Person),
            ("drn:github:user-id::583231", PrincipalKind::Person),
            ("drn:fauxlack:user:acme:ualice", PrincipalKind::Person),
            ("drn:fauxlack:channel:acme:cteam", PrincipalKind::Space),
        ] {
            let parsed = subject(form);
            assert_eq!(parsed.principal_kind(), kind, "{form}");
            assert_eq!(parsed.canonical(), form);
            let encoded = serde_json::to_string(&parsed).expect("serializes");
            assert_eq!(
                serde_json::from_str::<Subject>(&encoded).expect("deserializes"),
                parsed
            );
        }
    }

    #[test]
    fn mixed_case_input_folds_to_one_lowercase_drn() {
        let pasted = subject("DRN:Slack:User:T045P84S5:U07A1MJHNUX");
        assert_eq!(pasted, subject("drn:slack:user:t045p84s5:u07a1mjhnux"));
        assert_eq!(pasted.canonical(), "drn:slack:user:t045p84s5:u07a1mjhnux");
        assert_eq!(
            Subject::slack_user("T045P84S5", "U07A1MJHNUX").expect("raw Slack ids"),
            pasted
        );
        assert_eq!(
            scope("drn:SLACK:*:T045P84S5:*").to_string(),
            "drn:slack:*:t045p84s5:*"
        );
    }

    #[test]
    fn transport_constructors_build_their_kinds() {
        assert_eq!(
            Subject::discord_user("578258790881951745")
                .unwrap()
                .canonical(),
            "drn:discord:user::578258790881951745"
        );
        assert_eq!(
            Subject::telegram_user("5551234").unwrap().canonical(),
            "drn:telegram:user::5551234"
        );
        assert_eq!(
            Subject::whatsapp_number("16034700182").unwrap().canonical(),
            "drn:whatsapp:number::16034700182"
        );
        assert_eq!(
            Subject::telephone("+16034700182").unwrap().canonical(),
            "drn:tel:number::16034700182"
        );
        assert_eq!(
            Subject::fauxlack_user("acme", "ualice")
                .unwrap()
                .canonical(),
            "drn:fauxlack:user:acme:ualice"
        );
    }

    #[test]
    fn malformed_subjects_fail_closed() {
        for invalid in [
            "",
            "slack.t0123abc.u9xyz",
            "drn:slack:user:t0123abc",
            "drn:slack:user:t0123abc:u9xyz:extra",
            "drn:slack:user::u9xyz",
            "drn:slack:member:t0123abc:u9xyz",
            "drn:slack:user:t0123abc:u-9",
            "drn:discord:user:123:456",
            "drn:discord:user::0",
            "drn:discord:user::00123",
            "drn:discord:user::18446744073709551616",
            "drn:discord:channel::456",
            "drn:telegram:user::0123",
            "drn:telegram:user::alice",
            "drn:whatsapp:number::01603",
            "drn:whatsapp:number::1234567890123456",
            "drn:tel:number::+1603",
            "drn:github:repo:dekopon-agents:.",
            "drn:github:repo:dekopon-agents:..",
            "drn:github:repo:owner_name:repo",
            "drn:github:repo::repo",
            "drn:github:user::octo[bot]",
            "drn:github:repo-id:owner:42",
            "drn:sms:number::5551234",
            "drn:com.example.chat:user:a:b",
        ] {
            assert!(
                invalid.parse::<Subject>().is_err(),
                "{invalid:?} must not parse"
            );
        }
        assert!(Subject::new(SubjectKind::GithubRepo, &"x".repeat(40), "repo").is_err());
        assert!(Subject::new(SubjectKind::GithubRepo, "owner", &"x".repeat(101)).is_err());
    }

    #[test]
    fn reserved_kinds_never_parse() {
        for reserved in [
            "drn:whatsapp:user::alice",
            "drn:whatsapp:user-id:portfolio:42",
        ] {
            assert!(matches!(
                reserved.parse::<Subject>(),
                Err(SubjectError::ReservedKind { .. })
            ));
            assert!(matches!(
                reserved.parse::<SubjectScope>(),
                Err(SubjectError::ReservedKind { .. })
            ));
        }
        assert_eq!(
            scope("drn:whatsapp:*:*:*").kinds(),
            [SubjectKind::WhatsappNumber]
        );
    }

    #[test]
    fn a_wildcard_is_refused_when_partial_in_the_namespace_or_outside_a_scope() {
        for partial in ["drn:github:repo:dekopon*:*", "drn:slack:*:t045*:*"] {
            assert!(matches!(
                partial.parse::<SubjectScope>(),
                Err(SubjectError::PartialWildcard { .. })
            ));
        }
        assert_eq!(
            "drn:*:user:t045p84s5:*".parse::<SubjectScope>(),
            Err(SubjectError::WildcardNamespace)
        );
        for subject in [
            "drn:slack:*:t045p84s5:u1",
            "drn:github:repo:dekopon-agents:*",
        ] {
            assert_eq!(
                subject.parse::<Subject>(),
                Err(SubjectError::WildcardOutsideScope)
            );
        }
    }

    #[test]
    fn a_scope_that_admits_no_kind_is_refused() {
        assert!(matches!(
            "drn:discord:*:abc:*".parse::<SubjectScope>(),
            Err(SubjectError::ScopeAdmitsNoKind { .. })
        ));
        assert!(matches!(
            "drn:github:repo-id:owner:*".parse::<SubjectScope>(),
            Err(SubjectError::InvalidField { .. })
        ));
    }

    #[test]
    fn a_wildcard_kind_scope_names_the_kinds_it_covers() {
        assert_eq!(
            scope("drn:github:*:*:42").kinds(),
            [
                SubjectKind::GithubRepo,
                SubjectKind::GithubRepoId,
                SubjectKind::GithubUser,
                SubjectKind::GithubUserId
            ]
        );
        assert_eq!(
            scope("drn:github:*:dekopon-agents:*").kinds(),
            [SubjectKind::GithubRepo]
        );
    }

    #[test]
    fn a_repo_scope_never_covers_the_dotted_repository_beside_it() {
        let website = subject("drn:github:repo:dekopon-agents:dekopon-agents.github.io");
        assert!(!scope("drn:github:repo:dekopon-agents:dekopon-agents").covers(&website));
        assert!(scope("drn:github:repo:dekopon-agents:*").covers(&website));
        assert!(
            scope("drn:github:repo:dekopon-agents:*")
                .covers(&subject("drn:github:repo:dekopon-agents:dekopon-agents"))
        );
    }

    #[test]
    fn a_workspace_scope_never_covers_a_longer_team_id() {
        let workspace = scope("drn:slack:*:t045p84s5:*");
        assert!(!workspace.covers(&subject("drn:slack:user:t045p84s5x:u1")));
        assert!(workspace.covers(&subject("drn:slack:user:t045p84s5:u1")));
        assert!(workspace.covers(&subject("drn:slack:channel:t045p84s5:c1")));
    }

    #[test]
    fn a_tenant_scope_never_covers_an_empty_tenant_kind() {
        let owner = scope("drn:github:*:dekopon-agents:*");
        assert!(!owner.covers(&subject("drn:github:repo-id::42")));
        assert!(scope("drn:github:*:*:42").covers(&subject("drn:github:repo-id::42")));
    }

    #[test]
    fn a_github_attestation_is_always_a_twin() {
        let twin =
            AttestedSubject::github_repository("Dekopon-Agents", "Dekopon-Agents.github.io", 42)
                .expect("webhook facts");
        assert_eq!(
            twin.name().canonical(),
            "drn:github:repo:dekopon-agents:dekopon-agents.github.io"
        );
        assert_eq!(
            twin.id_form().map(Subject::canonical).as_deref(),
            Some("drn:github:repo-id::42")
        );
        let encoded = serde_json::to_string(&twin).expect("serializes");
        assert_eq!(
            encoded,
            r#"["drn:github:repo:dekopon-agents:dekopon-agents.github.io","drn:github:repo-id::42"]"#
        );
        assert_eq!(
            serde_json::from_str::<AttestedSubject>(&encoded).expect("decodes"),
            twin
        );
        for one in [
            "drn:github:repo:dekopon-agents:dekopon",
            "drn:github:repo-id::42",
            "drn:github:user::octocat",
            "drn:github:user-id::583231",
        ] {
            assert_eq!(
                AttestedSubject::one(subject(one)),
                Err(SubjectError::GithubNeedsTwin)
            );
            assert!(serde_json::from_str::<AttestedSubject>(&format!("[{one:?}]")).is_err());
        }
        for unpaired in [
            r#"["drn:github:repo:a:b","drn:github:user-id::1"]"#,
            r#"["drn:github:repo-id::1","drn:github:repo:a:b"]"#,
            r#"["drn:slack:user:t1:u1","drn:slack:user:t1:u2"]"#,
            r#"["drn:github:repo:a:b","drn:github:repo-id::1","drn:github:repo-id::2"]"#,
            "[]",
        ] {
            assert!(
                serde_json::from_str::<AttestedSubject>(unpaired).is_err(),
                "{unpaired}"
            );
        }
        let one = AttestedSubject::one(subject("drn:slack:user:t1:u1")).expect("slack");
        assert_eq!(
            serde_json::to_string(&one).expect("serializes"),
            r#"["drn:slack:user:t1:u1"]"#
        );
    }
}
