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
        GithubNumber::new(
            value
                .parse()
                .map_err(|_invalid_decimal| GithubNumberError)?,
        )
        .map(Self::Number)
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
                .pointer("/blocked_issue_repo/id")
                .and_then(Value::as_u64)
                .and_then(|id| GithubNumber::new(id).ok());
            let local = match body.get("action").and_then(Value::as_str) {
                Some("blocked_by_added" | "blocked_by_removed") => own_repository.is_some(),
                _ => own_repository.is_some() && own_repository == blocked_repository,
            };
            if local {
                number("/blocked_issue/number")
            } else {
                Ok(GithubThread::Repository)
            }
        }
        E::WorkflowRun => Ok(associated_thread(body, "/workflow_run/pull_requests")),
        E::CheckRun => Ok(associated_thread(body, "/check_run/pull_requests")),
        E::CheckSuite => Ok(associated_thread(body, "/check_suite/pull_requests")),
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

fn associated_thread(body: &Value, path: &str) -> GithubThread {
    let repository = body
        .pointer("/repository/id")
        .and_then(Value::as_u64)
        .and_then(|id| GithubNumber::new(id).ok());
    let mut number = None;
    if let Some(prs) = body.pointer(path).and_then(Value::as_array) {
        for pr in prs {
            let base = pr
                .pointer("/base/repo/id")
                .and_then(Value::as_u64)
                .and_then(|id| GithubNumber::new(id).ok());
            if repository.is_some()
                && repository == base
                && let Some(next) = pr
                    .get("number")
                    .and_then(Value::as_u64)
                    .and_then(|n| GithubNumber::new(n).ok())
            {
                if number.is_some_and(|previous| previous != next) {
                    return GithubThread::Repository;
                }
                number = Some(next);
            }
        }
    }
    number.map_or(GithubThread::Repository, GithubThread::Number)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GithubId(GithubNumber);

impl GithubId {
    pub fn new(value: u64) -> Result<Self, GithubNumberError> {
        GithubNumber::new(value).map(Self)
    }
}

impl fmt::Display for GithubId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_family_keys_by_its_number_or_the_repository() {
        use GithubEventName as E;
        let numbered = [
            (E::PullRequest, json!({"number":7})),
            (E::PullRequestReview, json!({"pull_request":{"number":7}})),
            (
                E::PullRequestReviewComment,
                json!({"pull_request":{"number":7}}),
            ),
            (
                E::PullRequestReviewThread,
                json!({"pull_request":{"number":7}}),
            ),
            (E::Issues, json!({"issue":{"number":7}})),
            (
                E::IssueComment,
                json!({"issue":{"number":7,"pull_request":{}}}),
            ),
            (E::Discussion, json!({"discussion":{"number":7}})),
            (E::DiscussionComment, json!({"discussion":{"number":7}})),
            (
                E::SubIssues,
                json!({"repository":{"id":10},"parent_issue_repo":{"id":10},"parent_issue":{"number":7}}),
            ),
        ];
        for (event, body) in numbered {
            assert_eq!(thread(event, &body).unwrap().to_string(), "7", "{event:?}");
        }
        for event in [
            E::BranchProtectionConfiguration,
            E::BranchProtectionRule,
            E::CodeScanningAlert,
            E::CommitComment,
            E::Create,
            E::CustomPropertyValues,
            E::Delete,
            E::DependabotAlert,
            E::DeployKey,
            E::Deployment,
            E::DeploymentProtectionRule,
            E::DeploymentReview,
            E::DeploymentStatus,
            E::Fork,
            E::Gollum,
            E::Label,
            E::Member,
            E::MergeGroup,
            E::Meta,
            E::Milestone,
            E::Package,
            E::PageBuild,
            E::ProjectCard,
            E::ProjectColumn,
            E::Project,
            E::Public,
            E::Push,
            E::RegistryPackage,
            E::Release,
            E::Repository,
            E::RepositoryAdvisory,
            E::RepositoryDispatch,
            E::RepositoryImport,
            E::RepositoryRuleset,
            E::RepositoryVulnerabilityAlert,
            E::SecretScanningAlert,
            E::SecretScanningAlertLocation,
            E::SecretScanningScan,
            E::SecurityAndAnalysis,
            E::Star,
            E::Status,
            E::TeamAdd,
            E::Watch,
            E::WorkflowDispatch,
            E::WorkflowJob,
        ] {
            assert_eq!(
                thread(event, &json!({"number":7,"repository":{"id":10}})).unwrap(),
                GithubThread::Repository,
                "{event:?}"
            );
        }
        for (event, key) in [
            (E::WorkflowRun, "workflow_run"),
            (E::CheckRun, "check_run"),
            (E::CheckSuite, "check_suite"),
        ] {
            for (prs, expected) in [
                (json!([]), "repository"),
                (json!([{"number":7,"base":{"repo":{"id":10}}}]), "7"),
                (
                    json!([{"number":7,"base":{"repo":{"id":10}}},{"number":8,"base":{"repo":{"id":10}}}]),
                    "repository",
                ),
                (
                    json!([{"number":987,"base":{"repo":{"id":20}}}]),
                    "repository",
                ),
            ] {
                let body = json!({"repository":{"id":10},key:{"pull_requests":prs}});
                let selected = thread(event, &body).unwrap();
                assert_eq!(selected.to_string(), expected, "{event:?}");
                assert!(!format!("{selected:?}").contains("987"));
            }
        }
        let foreign_parent = json!({"repository":{"id":10},"parent_issue_repo":{"id":20},"parent_issue":{"number":987}});
        assert_eq!(
            thread(E::SubIssues, &foreign_parent).unwrap(),
            GithubThread::Repository
        );
    }

    #[test]
    fn issue_dependencies_key_only_by_a_local_blocked_issue() {
        let local_blocked = json!({
            "action": "blocked_by_added", "repository": {"id": 10},
            "blocked_issue": {"number": 7}, "blocking_issue": {"number": 987},
            "blocking_issue_repo": {"id": 20}
        });
        let foreign_blocked = json!({
            "action": "blocking_added", "repository": {"id": 10},
            "blocked_issue": {"number": 987}, "blocked_issue_repo": {"id": 20},
            "blocking_issue": {"number": 7}
        });
        let local_dependency = json!({
            "action": "blocking_removed", "repository": {"id": 10},
            "blocked_issue": {"number": 7}, "blocked_issue_repo": {"id": 10},
            "blocking_issue": {"number": 8}
        });
        for (body, expected) in [
            (local_blocked, "7"),
            (foreign_blocked, "repository"),
            (local_dependency, "7"),
        ] {
            let selected = thread(GithubEventName::IssueDependencies, &body).unwrap();
            assert_eq!(selected.to_string(), expected);
            assert!(!format!("{selected:?}").contains("987"));
        }
    }
}
