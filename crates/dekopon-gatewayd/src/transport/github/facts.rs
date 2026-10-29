use super::GithubEvent as E;
use dekopon_core::{
    ExternalSubject,
    github::{self, *},
};
use serde_json::Value;

fn text<T>(
    root: &Value,
    path: &str,
    parse: impl FnOnce(&str) -> Result<T, GithubFactError>,
) -> Option<T> {
    parse(root.pointer(path)?.as_str()?).ok()
}
fn id(root: &Value, path: &str) -> Option<GithubId> {
    GithubId::new(root.pointer(path)?.as_u64()?).ok()
}
fn number(root: &Value, path: &str) -> Option<GithubNumber> {
    GithubNumber::new(root.pointer(path)?.as_u64()?).ok()
}
fn word(root: &Value, path: &str) -> Option<GithubWord> {
    text(root, path, GithubWord::new)
}
fn reference(root: &Value, path: &str) -> Option<GitRefName> {
    text(root, path, GitRefName::new)
}
fn qualified_reference(root: &Value, path: &str) -> Option<GitRefName> {
    text(root, path, |value| {
        GitRefName::new(
            value
                .strip_prefix("refs/heads/")
                .or_else(|| value.strip_prefix("refs/tags/"))
                .unwrap_or(value),
        )
    })
}
fn sha(root: &Value, path: &str) -> Option<Sha> {
    text(root, path, Sha::new)
}
fn boolean(root: &Value, path: &str) -> Option<bool> {
    root.pointer(path)?.as_bool()
}
fn count(root: &Value, path: &str) -> Option<i64> {
    root.pointer(path)?.as_i64().filter(|n| *n >= 0)
}
fn labels(root: &Value) -> Option<BoundedVec<BoundedName<50>, 100>> {
    BoundedVec::new(
        root.get("labels")?
            .as_array()?
            .iter()
            .filter_map(|v| text(v, "/name", BoundedName::new))
            .take(100)
            .collect(),
    )
    .ok()
}
fn pull_requests(root: &Value, repository: GithubId) -> Option<BoundedVec<GithubNumber, 16>> {
    let mut numbers = Vec::new();
    for pr in root.get("pull_requests")?.as_array()? {
        if id(pr, "/base/repo/id") == Some(repository)
            && let Some(n) = number(pr, "/number")
            && !numbers.contains(&n)
            && numbers.len() < 16
        {
            numbers.push(n);
        }
    }
    BoundedVec::new(numbers).ok()
}
fn timestamp(value: &Value) -> Option<UnixSeconds> {
    if let Some(epoch) = value.as_i64() {
        return UnixSeconds::new(i128::from(epoch)).ok();
    }
    let time = humantime::parse_rfc3339(value.as_str()?).ok()?;
    let seconds = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i128::from(duration.as_secs()),
        Err(before) => {
            -i128::from(before.duration().as_secs())
                - i128::from(before.duration().subsec_nanos() != 0)
        }
    };
    UnixSeconds::new(seconds).ok()
}
fn pr(root: &Value, a: &mut AuthorSetFacts) -> Option<PullRequestFacts> {
    a.head_ref = Some(reference(root, "/head/ref")?);
    a.head_sha = Some(sha(root, "/head/sha")?);
    a.head_repository = text(root, "/head/repo/full_name", |s| {
        RepoFullName::new(&s.to_ascii_lowercase())
    });
    a.draft = Some(boolean(root, "/draft")?);
    a.state = Some(word(root, "/state")?);
    let base = id(root, "/base/repo/id")?;
    Some(PullRequestFacts {
        base_ref: reference(root, "/base/ref")?,
        base_sha: sha(root, "/base/sha")?,
        merged: boolean(root, "/merged")?,
        cross_repository: id(root, "/head/repo/id") != Some(base),
        author_id: id(root, "/user/id")?,
        author_association: word(root, "/author_association")?,
        labels: labels(root)?,
        merged_by: id(root, "/merged_by/id"),
    })
}
fn issue(root: &Value, a: &mut AuthorSetFacts) -> Option<IssueFacts> {
    a.state = Some(word(root, "/state")?);
    let assignees = BoundedVec::new(
        root.get("assignees")?
            .as_array()?
            .iter()
            .filter_map(|v| id(v, "/id"))
            .take(10)
            .collect(),
    )
    .ok()?;
    Some(IssueFacts {
        author_id: id(root, "/user/id")?,
        author_association: word(root, "/author_association")?,
        labels: labels(root)?,
        assignees,
        milestone: number(root, "/milestone/number"),
        is_pull_request: root.get("pull_request").is_some(),
    })
}
fn comment(root: &Value) -> Option<CommentFacts> {
    Some(CommentFacts {
        id: id(root, "/id")?,
        author_association: word(root, "/author_association")?,
    })
}
fn discussion(root: &Value, a: &mut AuthorSetFacts) -> Option<DiscussionFacts> {
    a.state = Some(word(root, "/state")?);
    a.category = word(root, "/category/slug");
    a.answered =
        boolean(root, "/answered").or_else(|| root.get("answer_chosen_at").map(|v| !v.is_null()));
    Some(DiscussionFacts {
        author_association: word(root, "/author_association")?,
    })
}
fn deployment(root: &Value, a: &mut AuthorSetFacts) -> Option<DeploymentFacts> {
    a.deployment_ref = Some(reference(root, "/ref")?);
    a.deployment_sha = Some(sha(root, "/sha")?);
    a.environment = reference(root, "/environment");
    a.task = word(root, "/task");
    Some(DeploymentFacts {
        id: Some(id(root, "/id")?),
    })
}
fn run_author(root: &Value, a: &mut AuthorSetFacts) -> Option<()> {
    a.run_status = Some(word(root, "/status")?);
    a.head_sha = Some(sha(root, "/head_sha")?);
    a.conclusion = word(root, "/conclusion");
    a.head_ref = reference(root, "/head_branch");
    Some(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive match owns the frozen family-to-facts table"
)]
pub(super) fn parse(root: &Value, event: E, delivery: &str) -> Option<github::GithubEvent> {
    let subject = ExternalSubject::github(
        root.pointer("/repository/owner/login")?.as_str()?,
        root.pointer("/repository/name")?.as_str()?,
    )
    .ok()?;
    let repository = RepositoryFacts {
        id: id(root, "/repository/id")?,
        full_name: RepoFullName::new(&format!("{}/{}", subject.tenant()?, subject.subject()))
            .ok()?,
    };
    let actor_kind = match root.pointer("/sender/type").and_then(Value::as_str) {
        Some("User") => Some(ActorKind::User),
        Some("Bot") => Some(ActorKind::Bot),
        Some("Organization") => Some(ActorKind::Organization),
        Some(_) => {
            tracing::debug!(
                event = "gateway_github_actor_kind_ignored",
                reason = "unknown-actor-kind",
                "gateway.github.actor.kind.ignored"
            );
            None
        }
        None => None,
    };
    let actor = ActorFacts {
        id: id(root, "/sender/id")?,
        kind: actor_kind,
    };
    let mut author = AuthorSetFacts {
        actor_login: text(root, "/sender/login", GithubLogin::new)?,
        head_ref: None,
        head_sha: None,
        head_repository: None,
        draft: None,
        state: None,
        review_state: None,
        review_commit: None,
        comment_commit: None,
        run_status: None,
        conclusion: None,
        run_path: None,
        alert_ref: None,
        fork_repository: None,
        category: None,
        answered: None,
        environment: None,
        alert_state: None,
        deployment_ref: None,
        deployment_sha: None,
        deployment_state: None,
        task: None,
    };
    let action = word(root, "/action");
    if matches!(event, E::PullRequest | E::WorkflowRun) && action.is_none() {
        return None;
    }
    let mut n = None;
    let mut at = None;
    let target = match event {
        E::PullRequest
        | E::PullRequestReview
        | E::PullRequestReviewComment
        | E::PullRequestReviewThread => {
            let payload = root.get("pull_request")?;
            n = Some(number(payload, "/number").or_else(|| number(root, "/number"))?);
            let pull_request = pr(payload, &mut author)?;
            if event == E::PullRequest {
                at = payload.get("updated_at").and_then(timestamp);
                Target::PullRequest { pull_request }
            } else if event == E::PullRequestReview {
                let review = root.get("review")?;
                author.review_state = Some(word(review, "/state")?);
                author.review_commit = Some(sha(review, "/commit_id")?);
                at = review.get("submitted_at").and_then(timestamp);
                Target::Review {
                    pull_request,
                    review: ReviewFacts {
                        id: id(review, "/id")?,
                        author_association: word(review, "/author_association")?,
                    },
                }
            } else if event == E::PullRequestReviewComment {
                let c = root.get("comment")?;
                author.comment_commit = sha(c, "/commit_id");
                at = c.get("updated_at").and_then(timestamp);
                Target::ReviewComment {
                    pull_request,
                    comment: comment(c)?,
                }
            } else {
                Target::PullRequest { pull_request }
            }
        }
        E::Issues | E::IssueComment => {
            let payload = root.get("issue")?;
            n = Some(number(payload, "/number")?);
            let issue = issue(payload, &mut author)?;
            if event == E::IssueComment {
                let c = root.get("comment")?;
                at = c.get("updated_at").and_then(timestamp);
                Target::IssueComment {
                    issue,
                    comment: comment(c)?,
                }
            } else {
                at = payload.get("updated_at").and_then(timestamp);
                Target::Issue { issue }
            }
        }
        E::SubIssues | E::IssueDependencies => {
            let (key, repo, local_actions) = if event == E::SubIssues {
                (
                    "parent_issue",
                    "parent_issue_repo",
                    ["sub_issue_added", "sub_issue_removed"],
                )
            } else {
                (
                    "blocked_issue",
                    "blocked_issue_repo",
                    ["blocked_by_added", "blocked_by_removed"],
                )
            };
            let local = action
                .as_ref()
                .is_some_and(|a| local_actions.contains(&a.as_str()))
                || root.get(repo).and_then(|r| id(r, "/id")) == Some(repository.id);
            if local {
                let payload = root.get(key)?;
                n = Some(number(payload, "/number")?);
                Target::Issue {
                    issue: issue(payload, &mut author)?,
                }
            } else {
                Target::None {}
            }
        }
        E::Discussion | E::DiscussionComment => {
            let payload = root.get("discussion")?;
            n = Some(number(payload, "/number")?);
            let discussion = discussion(payload, &mut author)?;
            if event == E::DiscussionComment {
                let c = root.get("comment")?;
                at = c.get("updated_at").and_then(timestamp);
                Target::DiscussionComment {
                    discussion,
                    comment: comment(c)?,
                }
            } else {
                at = payload.get("updated_at").and_then(timestamp);
                Target::Discussion { discussion }
            }
        }
        E::CommitComment => {
            let c = root.get("comment")?;
            at = c.get("created_at").and_then(timestamp);
            Target::CommitComment {
                comment: comment(c)?,
            }
        }
        E::Push => {
            let raw = root.get("ref")?.as_str()?;
            let ref_type = if raw.starts_with("refs/heads/") {
                RefType::Branch
            } else if raw.starts_with("refs/tags/") {
                RefType::Tag
            } else {
                return None;
            };
            at = root.pointer("/repository/pushed_at").and_then(timestamp);
            Target::Push {
                push: PushFacts {
                    git_ref: qualified_reference(root, "/ref")?,
                    ref_type,
                    before: sha(root, "/before")?,
                    after: sha(root, "/after")?,
                    created: boolean(root, "/created")?,
                    deleted: boolean(root, "/deleted")?,
                    forced: boolean(root, "/forced")?,
                    commits: i64::try_from(root.get("commits")?.as_array()?.len()).ok()?,
                    base_ref: qualified_reference(root, "/base_ref"),
                },
            }
        }
        E::Create | E::Delete | E::WorkflowDispatch => {
            let name = Some(if event == E::WorkflowDispatch {
                qualified_reference(root, "/ref")?
            } else {
                reference(root, "/ref")?
            });
            let ref_type = if event == E::WorkflowDispatch {
                None
            } else {
                Some(serde_json::from_value(root.get("ref_type")?.clone()).ok()?)
            };
            Target::Ref {
                git_ref: RefFacts { name, ref_type },
            }
        }
        E::WorkflowRun => {
            let r = root.get("workflow_run")?;
            run_author(r, &mut author)?;
            author.run_path =
                reference(r, "/path").filter(|p| p.as_str().starts_with(".github/workflows/"));
            at = r.get("updated_at").and_then(timestamp);
            Target::Run {
                run: RunFacts {
                    id: Some(id(r, "/id")?),
                    run_number: Some(count(r, "/run_number")?),
                    attempt: Some(count(r, "/run_attempt")?),
                    trigger: Some(word(r, "/event")?),
                    pull_requests: pull_requests(r, repository.id),
                    ..RunFacts::default()
                },
            }
        }
        E::WorkflowJob => {
            let r = root.get("workflow_job")?;
            run_author(r, &mut author)?;
            at = r
                .get("completed_at")
                .and_then(timestamp)
                .or_else(|| r.get("started_at").and_then(timestamp));
            Target::Run {
                run: RunFacts {
                    id: Some(id(r, "/run_id")?),
                    job_id: Some(id(r, "/id")?),
                    attempt: Some(count(r, "/run_attempt")?),
                    ..RunFacts::default()
                },
            }
        }
        E::CheckRun | E::CheckSuite => {
            let r = root.get(if event == E::CheckRun {
                "check_run"
            } else {
                "check_suite"
            })?;
            run_author(r, &mut author)?;
            let app_id = Some(id(r, "/app/id")?);
            let suite_id = Some(if event == E::CheckRun {
                id(r, "/check_suite/id")?
            } else {
                id(r, "/id")?
            });
            let own_id = if event == E::CheckRun {
                Some(id(r, "/id")?)
            } else {
                None
            };
            if event == E::CheckRun {
                author.head_ref = None;
                at = r
                    .get("completed_at")
                    .and_then(timestamp)
                    .or_else(|| r.get("started_at").and_then(timestamp));
            } else {
                at = r.get("updated_at").and_then(timestamp);
            }
            Target::Check {
                check: CheckFacts {
                    id: own_id,
                    app_id,
                    suite_id,
                    pull_requests: pull_requests(r, repository.id),
                },
            }
        }
        E::Status => {
            author.head_sha = Some(sha(root, "/sha")?);
            author.run_status = Some(word(root, "/state")?);
            at = root.get("updated_at").and_then(timestamp);
            Target::Check {
                check: CheckFacts {
                    id: Some(id(root, "/id")?),
                    ..CheckFacts::default()
                },
            }
        }
        E::Release => {
            let r = root.get("release")?;
            at = r
                .get("published_at")
                .and_then(timestamp)
                .or_else(|| r.get("created_at").and_then(timestamp));
            Target::Release {
                release: ReleaseFacts {
                    id: id(r, "/id")?,
                    tag: reference(r, "/tag_name")?,
                    draft: boolean(r, "/draft")?,
                    prerelease: boolean(r, "/prerelease")?,
                    target_commitish: reference(r, "/target_commitish"),
                },
            }
        }
        E::Deployment | E::DeploymentStatus => {
            let r = root.get("deployment")?;
            let deployment = deployment(r, &mut author)?;
            if event == E::DeploymentStatus {
                author.deployment_state = Some(word(root, "/deployment_status/state")?);
                at = root
                    .pointer("/deployment_status/created_at")
                    .and_then(timestamp);
            } else {
                at = r.get("created_at").and_then(timestamp);
            }
            Target::Deployment { deployment }
        }
        E::DeploymentReview | E::DeploymentProtectionRule => {
            author.environment = reference(root, "/environment");
            Target::Deployment {
                deployment: DeploymentFacts {
                    id: id(root, "/deployment/id"),
                },
            }
        }
        E::CodeScanningAlert
        | E::DependabotAlert
        | E::SecretScanningAlert
        | E::SecretScanningAlertLocation => {
            let r = root.get("alert")?;
            at = r.get("updated_at").and_then(timestamp);
            let mut alert = AlertFacts {
                number: Some(number(r, "/number")?),
                ..AlertFacts::default()
            };
            if event == E::CodeScanningAlert {
                author.alert_state = word(r, "/state");
                author.alert_ref = qualified_reference(root, "/ref")
                    .or_else(|| qualified_reference(r, "/most_recent_instance/ref"));
            } else if event == E::DependabotAlert {
                alert.state = Some(word(r, "/state")?);
                alert.severity = word(r, "/security_advisory/severity");
                alert.ecosystem = word(r, "/dependency/package/ecosystem");
                alert.package = text(r, "/dependency/package/name", BoundedName::new);
                alert.ghsa = text(r, "/security_advisory/ghsa_id", BoundedName::new);
            } else {
                alert.state = word(r, "/state");
                alert.resolution = word(r, "/resolution");
            }
            Target::Alert { alert }
        }
        E::RepositoryVulnerabilityAlert | E::RepositoryAdvisory => {
            let r = root.get(if event == E::RepositoryAdvisory {
                "repository_advisory"
            } else {
                "alert"
            })?;
            Target::Alert {
                alert: AlertFacts {
                    ghsa: Some(text(r, "/ghsa_id", BoundedName::new)?),
                    state: word(r, "/state"),
                    ..AlertFacts::default()
                },
            }
        }
        E::Label => {
            let r = root.get("label")?;
            Target::Label {
                label: LabelFacts {
                    id: id(r, "/id")?,
                    name: text(r, "/name", BoundedName::new)?,
                },
            }
        }
        E::Milestone => {
            let r = root.get("milestone")?;
            at = r.get("updated_at").and_then(timestamp);
            Target::Milestone {
                milestone: MilestoneFacts {
                    id: id(r, "/id")?,
                    number: number(r, "/number")?,
                    state: word(r, "/state")?,
                },
            }
        }
        E::MergeGroup => {
            let r = root.get("merge_group")?;
            Target::MergeGroup {
                merge_group: MergeGroupFacts {
                    head_sha: sha(r, "/head_sha")?,
                    base_sha: sha(r, "/base_sha")?,
                    base_ref: qualified_reference(r, "/base_ref")?,
                    head_ref: qualified_reference(r, "/head_ref"),
                },
            }
        }
        E::Package | E::RegistryPackage => {
            let r = root.get(if event == E::Package {
                "package"
            } else {
                "registry_package"
            })?;
            at = r.get("updated_at").and_then(timestamp);
            Target::Package {
                package: PackageFacts {
                    id: id(r, "/id")?,
                    ecosystem: word(r, "/package_type")?,
                    name: text(r, "/name", BoundedName::new),
                },
            }
        }
        E::Member => Target::Member {
            member: id(root, "/member/id")?,
        },
        E::Fork => {
            author.fork_repository = text(root, "/forkee/full_name", |s| {
                RepoFullName::new(&s.to_ascii_lowercase())
            });
            at = root.pointer("/forkee/created_at").and_then(timestamp);
            Target::None {}
        }
        E::BranchProtectionConfiguration
        | E::BranchProtectionRule
        | E::CustomPropertyValues
        | E::DeployKey
        | E::Gollum
        | E::Meta
        | E::PageBuild
        | E::Project
        | E::ProjectCard
        | E::ProjectColumn
        | E::Public
        | E::Repository
        | E::RepositoryDispatch
        | E::RepositoryImport
        | E::RepositoryRuleset
        | E::SecretScanningScan
        | E::SecurityAndAnalysis
        | E::Star
        | E::TeamAdd
        | E::Watch => Target::None {},
    };
    Some(github::GithubEvent {
        delivery: GithubDeliveryId::new(delivery).ok()?,
        event,
        action,
        at,
        repository,
        actor,
        number: n,
        target,
        author_set: author,
    })
}
