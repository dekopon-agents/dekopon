use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::{fmt, num::NonZeroU64, str::FromStr};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubEventName {
    BranchProtectionConfiguration,
    BranchProtectionRule,
    CheckRun,
    CheckSuite,
    CodeScanningAlert,
    CommitComment,
    Create,
    CustomPropertyValues,
    Delete,
    DependabotAlert,
    DeployKey,
    Deployment,
    DeploymentProtectionRule,
    DeploymentReview,
    DeploymentStatus,
    Discussion,
    DiscussionComment,
    Fork,
    Gollum,
    IssueComment,
    IssueDependencies,
    Issues,
    Label,
    Member,
    MergeGroup,
    Meta,
    Milestone,
    Package,
    PageBuild,
    ProjectCard,
    ProjectColumn,
    Project,
    Public,
    PullRequest,
    PullRequestReview,
    PullRequestReviewComment,
    PullRequestReviewThread,
    Push,
    RegistryPackage,
    Release,
    Repository,
    RepositoryAdvisory,
    RepositoryDispatch,
    RepositoryImport,
    RepositoryRuleset,
    RepositoryVulnerabilityAlert,
    SecretScanningAlert,
    SecretScanningAlertLocation,
    SecretScanningScan,
    SecurityAndAnalysis,
    Star,
    Status,
    SubIssues,
    TeamAdd,
    Watch,
    WorkflowDispatch,
    WorkflowJob,
    WorkflowRun,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("GitHub number must be a positive integer no greater than i64::MAX")]
pub struct GithubNumberError;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct GithubNumber(NonZeroU64);
impl GithubNumber {
    pub fn new(value: u64) -> Result<Self, GithubNumberError> {
        NonZeroU64::new(value)
            .filter(|v| v.get() <= i64::MAX as u64)
            .map(Self)
            .ok_or(GithubNumberError)
    }
}
impl TryFrom<u64> for GithubNumber {
    type Error = GithubNumberError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<GithubNumber> for u64 {
    fn from(value: GithubNumber) -> Self {
        value.0.get()
    }
}
impl fmt::Display for GithubNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct GithubId(NonZeroU64);
impl GithubId {
    pub fn new(value: u64) -> Result<Self, GithubNumberError> {
        NonZeroU64::new(value)
            .filter(|v| v.get() <= i64::MAX as u64)
            .map(Self)
            .ok_or(GithubNumberError)
    }
    pub fn get(self) -> i64 {
        self.0.get() as i64
    }
}
impl TryFrom<u64> for GithubId {
    type Error = GithubNumberError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<GithubId> for u64 {
    fn from(value: GithubId) -> Self {
        value.0.get()
    }
}
impl fmt::Display for GithubId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubThread {
    Number(GithubNumber),
    Repository,
}

impl fmt::Display for GithubThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => n.fmt(f),
            Self::Repository => f.write_str("repository"),
        }
    }
}

impl FromStr for GithubThread {
    type Err = GithubNumberError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "repository" {
            return Ok(Self::Repository);
        }
        if value.is_empty()
            || value.len() > 19
            || value.starts_with('0')
            || !value.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(GithubNumberError);
        }
        GithubNumber::new(
            value
                .parse()
                .map_err(|_invalid_decimal| GithubNumberError)?,
        )
        .map(Self::Number)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("GitHub fact is outside its bound")]
pub struct GithubFactError;

macro_rules! string_fact {
    ($name:ident, $validate:expr) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: &str) -> Result<Self, GithubFactError> {
                ($validate)(value)
                    .then(|| Self(value.to_owned()))
                    .ok_or(GithubFactError)
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::new(&String::deserialize(d)?).map_err(D::Error::custom)
            }
        }
    };
}
string_fact!(GithubDeliveryId, |v: &str| !v.is_empty()
    && v.len() <= 64
    && v.bytes().all(|b| b.is_ascii_lowercase()
        || b.is_ascii_digit()
        || b".-_:".contains(&b)));
string_fact!(Sha, |v: &str| matches!(v.len(), 40 | 64)
    && v.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
string_fact!(RepoFullName, |v: &str| v.split_once('/').is_some_and(
    |(owner, repo)| !owner.is_empty()
        && owner.len() <= 39
        && !repo.is_empty()
        && repo.len() <= 100
        && owner
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && repo
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
));
string_fact!(GithubLogin, |v: &str| !v.is_empty()
    && v.len() <= 64
    && v.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_[]".contains(&b)));
string_fact!(GithubWord, |v: &str| !v.is_empty()
    && v.len() <= 64
    && v.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b)));

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GitRefName(String);
impl GitRefName {
    pub fn new(value: &str) -> Result<Self, GithubFactError> {
        (!value.is_empty()
            && value.len() <= 255
            && !value.contains("..")
            && !value
                .bytes()
                .any(|b| b.is_ascii_control() || b" ~^:?*[\\".contains(&b)))
        .then(|| Self(value.to_owned()))
        .ok_or(GithubFactError)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for GitRefName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedName<const N: usize>(String);
impl<const N: usize> BoundedName<N> {
    pub fn new(value: &str) -> Result<Self, GithubFactError> {
        let value = value.trim();
        (!value.is_empty() && value.chars().count() <= N && !value.chars().any(char::is_control))
            .then(|| Self(value.to_owned()))
            .ok_or(GithubFactError)
    }
}
impl<'de, const N: usize> Deserialize<'de> for BoundedName<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedVec<T, const N: usize>(Vec<T>);
impl<T, const N: usize> BoundedVec<T, N> {
    pub fn new(values: Vec<T>) -> Result<Self, GithubFactError> {
        (values.len() <= N)
            .then_some(Self(values))
            .ok_or(GithubFactError)
    }
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for BoundedVec<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(Vec::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixSeconds(i64);
impl UnixSeconds {
    pub fn new(value: i128) -> Result<Self, GithubFactError> {
        i64::try_from(value)
            .map(Self)
            .map_err(|_out_of_range| GithubFactError)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    User,
    Bot,
    Organization,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefType {
    Branch,
    Tag,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RepositoryFacts {
    pub id: GithubId,
    pub full_name: RepoFullName,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ActorFacts {
    pub id: GithubId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ActorKind>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PullRequestFacts {
    pub base_ref: GitRefName,
    pub base_sha: Sha,
    pub merged: bool,
    pub cross_repository: bool,
    pub author_id: GithubId,
    pub author_association: GithubWord,
    pub labels: BoundedVec<BoundedName<50>, 100>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_by: Option<GithubId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct IssueFacts {
    pub author_id: GithubId,
    pub author_association: GithubWord,
    pub labels: BoundedVec<BoundedName<50>, 100>,
    pub assignees: BoundedVec<GithubId, 10>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone: Option<GithubNumber>,
    pub is_pull_request: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReviewFacts {
    pub id: GithubId,
    pub author_association: GithubWord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CommentFacts {
    pub id: GithubId,
    pub author_association: GithubWord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DiscussionFacts {
    pub author_association: GithubWord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PushFacts {
    #[serde(rename = "ref")]
    pub git_ref: GitRefName,
    pub ref_type: RefType,
    pub before: Sha,
    pub after: Sha,
    pub created: bool,
    pub deleted: bool,
    pub forced: bool,
    pub commits: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<GitRefName>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RefFacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<GitRefName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_type: Option<RefType>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RunFacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<GithubId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<GithubId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_requests: Option<BoundedVec<GithubNumber, 16>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CheckFacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<GithubId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<GithubId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite_id: Option<GithubId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_requests: Option<BoundedVec<GithubNumber, 16>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReleaseFacts {
    pub id: GithubId,
    pub tag: GitRefName,
    pub draft: bool,
    pub prerelease: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_commitish: Option<GitRefName>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeploymentFacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<GithubId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AlertFacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<GithubNumber>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ecosystem: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<BoundedName<214>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ghsa: Option<BoundedName<32>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LabelFacts {
    pub id: GithubId,
    pub name: BoundedName<50>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MilestoneFacts {
    pub id: GithubId,
    pub number: GithubNumber,
    pub state: GithubWord,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MergeGroupFacts {
    pub head_sha: Sha,
    pub base_sha: Sha,
    pub base_ref: GitRefName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<GitRefName>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PackageFacts {
    pub id: GithubId,
    pub ecosystem: GithubWord,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<BoundedName<214>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AuthorSetFacts {
    pub actor_login: GithubLogin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<GitRefName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<Sha>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_repository: Option<RepoFullName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_state: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_commit: Option<Sha>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_commit: Option<Sha>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_status: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_path: Option<GitRefName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_ref: Option<GitRefName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_repository: Option<RepoFullName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<GitRefName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_state: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_ref: Option<GitRefName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_sha: Option<Sha>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_state: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<GithubWord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Target {
    None {},
    PullRequest {
        pull_request: PullRequestFacts,
    },
    Review {
        pull_request: PullRequestFacts,
        review: ReviewFacts,
    },
    ReviewComment {
        pull_request: PullRequestFacts,
        comment: CommentFacts,
    },
    Issue {
        issue: IssueFacts,
    },
    IssueComment {
        issue: IssueFacts,
        comment: CommentFacts,
    },
    Discussion {
        discussion: DiscussionFacts,
    },
    DiscussionComment {
        discussion: DiscussionFacts,
        comment: CommentFacts,
    },
    CommitComment {
        comment: CommentFacts,
    },
    Push {
        push: PushFacts,
    },
    Ref {
        git_ref: RefFacts,
    },
    Run {
        run: RunFacts,
    },
    Check {
        check: CheckFacts,
    },
    Release {
        release: ReleaseFacts,
    },
    Deployment {
        deployment: DeploymentFacts,
    },
    Alert {
        alert: AlertFacts,
    },
    Label {
        label: LabelFacts,
    },
    Milestone {
        milestone: MilestoneFacts,
    },
    MergeGroup {
        merge_group: MergeGroupFacts,
    },
    Package {
        package: PackageFacts,
    },
    Member {
        member: GithubId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubEvent {
    pub delivery: GithubDeliveryId,
    pub event: GithubEventName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<GithubWord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<UnixSeconds>,
    pub repository: RepositoryFacts,
    pub actor: ActorFacts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<GithubNumber>,
    #[serde(flatten)]
    pub target: Target,
    pub author_set: AuthorSetFacts,
}

impl GithubEvent {
    pub fn thread(&self) -> GithubThread {
        use GithubEventName as E;
        let number = match self.event {
            E::PullRequest
            | E::PullRequestReview
            | E::PullRequestReviewComment
            | E::PullRequestReviewThread
            | E::Issues
            | E::IssueComment
            | E::SubIssues
            | E::IssueDependencies
            | E::Discussion
            | E::DiscussionComment => self.number,
            E::WorkflowRun | E::CheckRun | E::CheckSuite => {
                let prs = match &self.target {
                    Target::Run { run } => run.pull_requests.as_ref(),
                    Target::Check { check } => check.pull_requests.as_ref(),
                    Target::None {}
                    | Target::PullRequest { .. }
                    | Target::Review { .. }
                    | Target::ReviewComment { .. }
                    | Target::Issue { .. }
                    | Target::IssueComment { .. }
                    | Target::Discussion { .. }
                    | Target::DiscussionComment { .. }
                    | Target::CommitComment { .. }
                    | Target::Push { .. }
                    | Target::Ref { .. }
                    | Target::Release { .. }
                    | Target::Deployment { .. }
                    | Target::Alert { .. }
                    | Target::Label { .. }
                    | Target::Milestone { .. }
                    | Target::MergeGroup { .. }
                    | Target::Package { .. }
                    | Target::Member { .. } => None,
                };
                prs.and_then(|prs| match prs.as_slice() {
                    [number] => Some(*number),
                    _ => None,
                })
            }
            E::BranchProtectionConfiguration
            | E::BranchProtectionRule
            | E::CodeScanningAlert
            | E::CommitComment
            | E::Create
            | E::CustomPropertyValues
            | E::Delete
            | E::DependabotAlert
            | E::DeployKey
            | E::Deployment
            | E::DeploymentProtectionRule
            | E::DeploymentReview
            | E::DeploymentStatus
            | E::Fork
            | E::Gollum
            | E::Label
            | E::Member
            | E::MergeGroup
            | E::Meta
            | E::Milestone
            | E::Package
            | E::PageBuild
            | E::ProjectCard
            | E::ProjectColumn
            | E::Project
            | E::Public
            | E::Push
            | E::RegistryPackage
            | E::Release
            | E::Repository
            | E::RepositoryAdvisory
            | E::RepositoryDispatch
            | E::RepositoryImport
            | E::RepositoryRuleset
            | E::RepositoryVulnerabilityAlert
            | E::SecretScanningAlert
            | E::SecretScanningAlertLocation
            | E::SecretScanningScan
            | E::SecurityAndAnalysis
            | E::Star
            | E::Status
            | E::TeamAdd
            | E::Watch
            | E::WorkflowDispatch
            | E::WorkflowJob => None,
        };
        number.map_or(GithubThread::Repository, GithubThread::Number)
    }
    pub fn label(&self) -> String {
        let name = serde_json::to_value(self.event)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        self.action
            .as_ref()
            .map_or_else(|| name.clone(), |action| format!("{name}.{action}"))
    }
    pub fn note(&self) -> String {
        let mut line = format!(
            "[gateway: unverified GitHub {} claim] repository {}; action {}; {}; delivery {}.",
            self.label().split('.').next().unwrap_or_default(),
            self.repository.full_name,
            self.action.as_ref().map_or("none", GithubWord::as_str),
            match self.thread() {
                GithubThread::Number(n) => format!("#{n}"),
                GithubThread::Repository => "repository".to_owned(),
            },
            self.delivery
        );
        if let Some(sha) = &self.author_set.head_sha {
            line.push_str(&format!(" head {sha};"));
        }
        if let Target::Run { run } = &self.target
            && let Some(id) = run.id
        {
            let association = match self.thread() {
                GithubThread::Number(n) => format!("#{n}"),
                GithubThread::Repository => "none".to_owned(),
            };
            line.push_str(&format!(" run {id}; associated PR {association};"));
        }
        if let Some(conclusion) = &self.author_set.conclusion {
            line.push_str(&format!(" conclusion {conclusion};"));
        }
        line
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GithubBatch(Vec<GithubEvent>);
impl GithubBatch {
    pub fn new(event: GithubEvent) -> Self {
        Self(vec![event])
    }
    pub fn from_events(events: Vec<GithubEvent>) -> Result<Self, GithubFactError> {
        if events.is_empty()
            || events.len() > 16
            || events.iter().any(|e| e.actor.id != events[0].actor.id)
        {
            return Err(GithubFactError);
        }
        Ok(Self(events))
    }
    pub fn lead(&self) -> &GithubEvent {
        &self.0[0]
    }
    pub fn events(&self) -> &[GithubEvent] {
        &self.0
    }
    pub fn deliveries(&self) -> Vec<&str> {
        self.0.iter().map(|e| e.delivery.as_str()).collect()
    }
    pub fn append(&mut self, other: &Self) -> bool {
        if self.0.len() + other.0.len() > 16 || self.lead().actor.id != other.lead().actor.id {
            return false;
        }
        self.0.extend_from_slice(&other.0);
        true
    }
    pub fn note(&self) -> String {
        let mut lines = self.0.iter().map(GithubEvent::note).collect::<Vec<_>>();
        lines.push("No associated PR means report only.".to_owned());
        lines.push("Read current state through a broker-granted capability before acting. Call wake_reason for every fact.".to_owned());
        lines.join("\n")
    }
}
impl<'de> Deserialize<'de> for GithubBatch {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_events(Vec::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn event(name: &str) -> GithubEvent {
        serde_json::from_value(json!({"delivery":"delivery-1","event":name,"repository":{"id":10,"fullName":"owner/repo"},"actor":{"id":1},"number":7,"kind":"none","authorSet":{"actorLogin":"name[bot]"}})).unwrap()
    }
    #[test]
    fn run_note_names_the_associated_pull_request_or_none() {
        for (numbers, thread, association) in
            [(vec![7], "#7", "#7"), (vec![], "repository", "none")]
        {
            let mut e = event("workflow_run");
            e.target = Target::Run {
                run: RunFacts {
                    id: Some(GithubId::new(3).unwrap()),
                    pull_requests: Some(
                        BoundedVec::new(
                            numbers
                                .into_iter()
                                .map(|n| GithubNumber::new(n).unwrap())
                                .collect(),
                        )
                        .unwrap(),
                    ),
                    ..RunFacts::default()
                },
            };
            assert_eq!(
                e.note(),
                format!(
                    "[gateway: unverified GitHub workflow_run claim] repository owner/repo; action none; {thread}; delivery delivery-1. run 3; associated PR {association};"
                )
            );
        }
    }
    #[test]
    fn every_family_keys_by_its_number_or_the_repository() {
        for name in [
            "pull_request",
            "pull_request_review",
            "pull_request_review_comment",
            "pull_request_review_thread",
            "issues",
            "issue_comment",
            "discussion",
            "discussion_comment",
            "sub_issues",
            "issue_dependencies",
        ] {
            assert_eq!(event(name).thread().to_string(), "7");
        }
        for name in [
            "push",
            "workflow_job",
            "status",
            "release",
            "repository",
            "milestone",
        ] {
            assert_eq!(event(name).thread(), GithubThread::Repository);
        }
        for name in ["workflow_run", "check_run", "check_suite"] {
            let mut e = event(name);
            for (numbers, expected) in [
                (vec![], "repository"),
                (vec![7], "7"),
                (vec![7, 8], "repository"),
            ] {
                let prs = BoundedVec::new(
                    numbers
                        .into_iter()
                        .map(|n| GithubNumber::new(n).unwrap())
                        .collect(),
                )
                .unwrap();
                e.target = if name == "workflow_run" {
                    Target::Run {
                        run: RunFacts {
                            pull_requests: Some(prs),
                            ..RunFacts::default()
                        },
                    }
                } else {
                    Target::Check {
                        check: CheckFacts {
                            pull_requests: Some(prs),
                            ..CheckFacts::default()
                        },
                    }
                };
                assert_eq!(e.thread().to_string(), expected);
            }
        }
        for invalid in [
            "0",
            "01",
            "-1",
            "9223372036854775808",
            "12345678901234567890",
        ] {
            assert!(invalid.parse::<GithubThread>().is_err());
        }
    }
    #[test]
    fn invalid_numbers_strings_and_batches_cannot_decode() {
        for value in [0, u64::MAX] {
            assert!(serde_json::from_value::<GithubId>(json!(value)).is_err());
            assert!(serde_json::from_value::<GithubNumber>(json!(value)).is_err());
        }
        for word in ["OWNER", "q-a", "deploy:migrations"] {
            assert!(GithubWord::new(word).is_ok());
        }
        assert!(GithubWord::new("推荐项目").is_err());
        assert!(Sha::new(&"a".repeat(41)).is_err());
        assert!(BoundedName::<50>::new(&"🍋".repeat(50)).is_ok());
        assert!(GitRefName::new("feature..bad").is_err());
        assert!(GithubBatch::from_events(vec![]).is_err());
        assert!(GithubBatch::from_events(vec![event("repository"); 17]).is_err());
        let mut other = event("repository");
        other.actor.id = GithubId::new(2).unwrap();
        assert!(GithubBatch::from_events(vec![event("repository"), other]).is_err());
    }
}
