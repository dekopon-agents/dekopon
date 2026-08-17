//! Repository, branch, commit, and user read capabilities.

use dekopon_provider_http::{HttpError, Request, Response};
use dekopon_provider_sdk::ProviderError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    ACCEPT_JSON, MAX_DESCRIPTION_OUT_BYTES, MAX_LIST_ITEMS, MAX_MESSAGE_OUT_BYTES, RawUser,
    bounded_optional, decode, encode_path, endpoint, invalid_input, invalid_response, is_sha,
    login_out, percent_encode, send_get, timestamp, truncate_text, validate_login, validate_ref,
    validate_repo,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RepoInput {
    owner: String,
    repo: String,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BranchInput {
    owner: String,
    repo: String,
    branch: String,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CommitInput {
    owner: String,
    repo: String,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UserInput {
    login: String,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRepo {
    name: String,
    #[serde(default)]
    owner: Option<RawUser>,
    private: bool,
    #[serde(default)]
    visibility: Option<String>,
    default_branch: String,
    fork: bool,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    pushed_at: Option<String>,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct RawBranch {
    name: String,
    commit: RawBranchCommit,
    #[serde(default)]
    protected: bool,
}

#[derive(Debug, Deserialize)]
struct RawBranchCommit {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct RawCommit {
    sha: String,
    commit: RawCommitDetail,
    #[serde(default)]
    author: Option<RawUser>,
    #[serde(default)]
    stats: Option<RawStats>,
    #[serde(default)]
    files: Vec<RawCommitFile>,
}

#[derive(Debug, Deserialize)]
struct RawCommitDetail {
    message: String,
    #[serde(default)]
    author: Option<RawCommitAuthor>,
}

#[derive(Debug, Deserialize)]
struct RawCommitAuthor {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStats {
    additions: u64,
    deletions: u64,
    total: u64,
}

#[derive(Debug, Deserialize)]
struct RawCommitFile {
    filename: String,
    status: String,
    additions: u64,
    deletions: u64,
}

#[derive(Debug, Deserialize)]
struct RawProfile {
    login: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    public_repos: Option<u64>,
    #[serde(default)]
    followers: Option<u64>,
    created_at: String,
}

pub(crate) fn repo(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<RepoInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
    );
    let response = send_get(send, uri, ACCEPT_JSON)?;
    let repo = decode::<RawRepo>(&response.body)?;
    // The name echo binds the response to the request without being case-brittle: GitHub
    // canonicalizes repository casing.
    if !repo.name.eq_ignore_ascii_case(&input.repo) {
        return Err(invalid_response());
    }

    let (description, description_truncated) =
        bounded_optional(repo.description.as_deref(), MAX_DESCRIPTION_OUT_BYTES);
    Ok(json!({
        "name": repo.name,
        "owner": login_out(repo.owner.as_ref()),
        "private": repo.private,
        "visibility": repo.visibility,
        "defaultBranch": repo.default_branch,
        "fork": repo.fork,
        "archived": repo.archived,
        "description": description,
        "descriptionTruncated": description_truncated,
        "pushedAt": repo.pushed_at,
        "updatedAt": timestamp(&repo.updated_at)?,
    }))
}

pub(crate) fn branch(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<BranchInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_ref(&input.branch)?;
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}/branches/{}",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        encode_path(&input.branch),
    );
    let response = send_get(send, uri, ACCEPT_JSON)?;
    let branch = decode::<RawBranch>(&response.body)?;
    if branch.name != input.branch || !is_sha(&branch.commit.sha) {
        return Err(invalid_response());
    }

    Ok(json!({
        "name": branch.name,
        "headSha": branch.commit.sha,
        "protected": branch.protected,
    }))
}

pub(crate) fn commit(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<CommitInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_ref(&input.reference)?;
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}/commits/{}",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        encode_path(&input.reference),
    );
    let response = send_get(send, uri, ACCEPT_JSON)?;
    let commit = decode::<RawCommit>(&response.body)?;
    if !is_sha(&commit.sha) {
        return Err(invalid_response());
    }

    let (message, message_truncated) = truncate_text(&commit.commit.message, MAX_MESSAGE_OUT_BYTES);
    let files_truncated = commit.files.len() > MAX_LIST_ITEMS;
    let files = commit
        .files
        .iter()
        .take(MAX_LIST_ITEMS)
        .map(|file| {
            json!({
                "path": file.filename,
                "status": file.status,
                "additions": file.additions,
                "deletions": file.deletions,
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "sha": commit.sha,
        "author": login_out(commit.author.as_ref()),
        "authorName": commit.commit.author.as_ref().and_then(|author| author.name.as_deref()),
        "authoredAt": commit.commit.author.as_ref().and_then(|author| author.date.as_deref()),
        "message": message,
        "messageTruncated": message_truncated,
        "stats": commit.stats.map(|stats| json!({
            "additions": stats.additions,
            "deletions": stats.deletions,
            "total": stats.total,
        })),
        "files": files,
        "filesTruncated": files_truncated,
    }))
}

pub(crate) fn user(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<UserInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.login)?;
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!("{endpoint}/users/{}", percent_encode(&input.login));
    let response = send_get(send, uri, ACCEPT_JSON)?;
    let profile = decode::<RawProfile>(&response.body)?;
    if !profile.login.eq_ignore_ascii_case(&input.login) {
        return Err(invalid_response());
    }

    let (name, _) = bounded_optional(profile.name.as_deref(), 256);
    Ok(json!({
        "login": profile.login,
        "type": profile.kind,
        "name": name,
        "publicRepos": profile.public_repos,
        "followers": profile.followers,
        "createdAt": timestamp(&profile.created_at)?,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::invoke_with;
    use crate::testutil::{capability, json_response, scripted, step};

    #[test]
    fn repo_read_projects_metadata() {
        let output = invoke_with(
            &capability("gh.repo.read"),
            json!({"owner": "octo", "repo": "Hello"}),
            scripted(vec![step(
                |request| {
                    assert_eq!(request.method, "GET");
                    assert_eq!(request.uri, "https://api.github.com/repos/octo/Hello");
                },
                json_response(
                    200,
                    &json!({
                        "name": "hello",
                        "owner": {"login": "octo"},
                        "private": true,
                        "visibility": "private",
                        "default_branch": "main",
                        "fork": false,
                        "archived": false,
                        "description": "A demo",
                        "pushed_at": "2026-08-10T00:00:00Z",
                        "updated_at": "2026-08-11T00:00:00Z",
                    }),
                ),
            )]),
        )
        .expect("repo read succeeds");

        assert_eq!(output["defaultBranch"], "main");
        assert_eq!(output["private"], true);
        assert_eq!(output["owner"], "octo");
        assert_eq!(output["descriptionTruncated"], false);
    }

    #[test]
    fn branch_read_validates_the_echo_and_sha() {
        let output = invoke_with(
            &capability("gh.branch.read"),
            json!({"owner": "octo", "repo": "hello", "branch": "release/1.2"}),
            scripted(vec![step(
                |request| {
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/branches/release/1.2"
                    );
                },
                json_response(
                    200,
                    &json!({
                        "name": "release/1.2",
                        "commit": {"sha": "a".repeat(40)},
                        "protected": true,
                    }),
                ),
            )]),
        )
        .expect("branch read succeeds");

        assert_eq!(output["headSha"], "a".repeat(40));
        assert_eq!(output["protected"], true);

        let error = invoke_with(
            &capability("gh.branch.read"),
            json!({"owner": "octo", "repo": "hello", "branch": "main"}),
            scripted(vec![step(
                |_| {},
                json_response(
                    200,
                    &json!({"name": "other", "commit": {"sha": "a".repeat(40)}, "protected": false}),
                ),
            )]),
        )
        .expect_err("echo mismatch fails");
        assert_eq!(error.code(), "invalid-response");
    }

    #[test]
    fn commit_read_bounds_message_and_files() {
        let output = invoke_with(
            &capability("gh.commit.read"),
            json!({"owner": "octo", "repo": "hello", "ref": "main"}),
            scripted(vec![step(
                |request| {
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/commits/main"
                    );
                },
                json_response(
                    200,
                    &json!({
                        "sha": "c".repeat(40),
                        "commit": {
                            "message": "fix: everything",
                            "author": {"name": "Xavier", "date": "2026-08-15T00:00:00Z"},
                        },
                        "author": {"login": "xavier"},
                        "stats": {"additions": 3, "deletions": 1, "total": 4},
                        "files": [
                            {"filename": "src/lib.rs", "status": "modified", "additions": 3, "deletions": 1},
                        ],
                    }),
                ),
            )]),
        )
        .expect("commit read succeeds");

        assert_eq!(output["sha"], "c".repeat(40));
        assert_eq!(output["author"], "xavier");
        assert_eq!(output["authorName"], "Xavier");
        assert_eq!(output["stats"]["total"], 4);
        assert_eq!(output["files"][0]["path"], "src/lib.rs");
        assert_eq!(output["filesTruncated"], false);
    }

    #[test]
    fn user_read_matches_login_case_insensitively() {
        let output = invoke_with(
            &capability("gh.user.read"),
            json!({"login": "CPetersen"}),
            scripted(vec![step(
                |request| {
                    assert_eq!(request.uri, "https://api.github.com/users/CPetersen");
                },
                json_response(
                    200,
                    &json!({
                        "login": "cpetersen",
                        "type": "User",
                        "name": "Chris Petersen",
                        "public_repos": 42,
                        "followers": 7,
                        "created_at": "2010-01-01T00:00:00Z",
                    }),
                ),
            )]),
        )
        .expect("user read succeeds");

        assert_eq!(output["login"], "cpetersen");
        assert_eq!(output["type"], "User");
        assert_eq!(output["name"], "Chris Petersen");
    }
}
