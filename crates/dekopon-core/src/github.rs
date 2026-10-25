use std::{fmt, num::NonZeroU64, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GithubNumber(NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("GitHub number must be a positive integer no greater than i64::MAX")]
pub struct GithubNumberError;

impl GithubNumber {
    pub fn new(value: u64) -> Result<Self, GithubNumberError> {
        NonZeroU64::new(value)
            .filter(|value| value.get() <= i64::MAX as u64)
            .map(Self)
            .ok_or(GithubNumberError)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for GithubNumber {
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
        GithubNumber::new(value.parse().map_err(|_| GithubNumberError)?).map(Self::Number)
    }
}

pub fn thread(event: GithubEventName, body: &Value) -> Result<GithubThread, GithubNumberError> {
    use GithubEventName as E;
    let number = |path: &str| {
        body.pointer(path)
            .and_then(Value::as_u64)
            .ok_or(GithubNumberError)
            .and_then(GithubNumber::new)
            .map(GithubThread::Number)
    };
    let own_repository = body
        .pointer("/repository/id")
        .and_then(Value::as_u64)
        .and_then(|id| GithubNumber::new(id).ok());
    match event {
        E::PullRequest => number("/number"),
        E::PullRequestReview | E::PullRequestReviewComment | E::PullRequestReviewThread => {
            number("/pull_request/number")
        }
        E::Issues | E::IssueComment => number("/issue/number"),
        E::Discussion | E::DiscussionComment => number("/discussion/number"),
        E::SubIssues => {
            let parent_repository = body
                .pointer("/parent_issue_repo/id")
                .and_then(Value::as_u64)
                .and_then(|id| GithubNumber::new(id).ok());
            if own_repository.is_some() && own_repository == parent_repository {
                number("/parent_issue/number")
            } else {
                Ok(GithubThread::Repository)
            }
        }
        E::IssueDependencies => {
            let blocked_repository = body
                .pointer("/blocked_issue/repository/id")
                .and_then(Value::as_u64)
                .and_then(|id| GithubNumber::new(id).ok());
            if own_repository.is_some() && own_repository == blocked_repository {
                number("/blocked_issue/number")
            } else {
                Ok(GithubThread::Repository)
            }
        }
        E::WorkflowRun | E::CheckRun | E::CheckSuite => {
            let path = match event {
                E::WorkflowRun => "/workflow_run/pull_requests",
                E::CheckRun => "/check_run/pull_requests",
                E::CheckSuite => "/check_suite/pull_requests",
                _ => unreachable!(),
            };
            let mut numbers = Vec::new();
            if let Some(prs) = body.pointer(path).and_then(Value::as_array) {
                for pr in prs {
                    let repository = pr
                        .pointer("/base/repo/id")
                        .and_then(Value::as_u64)
                        .and_then(|id| GithubNumber::new(id).ok());
                    if own_repository.is_some()
                        && repository == own_repository
                        && let Some(n) = pr
                            .get("number")
                            .and_then(Value::as_u64)
                            .and_then(|n| GithubNumber::new(n).ok())
                        && !numbers.contains(&n)
                    {
                        numbers.push(n);
                    }
                }
            }
            match numbers.as_slice() {
                [n] => Ok(GithubThread::Number(*n)),
                _ => Ok(GithubThread::Repository),
            }
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
        | E::WorkflowJob => Ok(GithubThread::Repository),
    }
}
