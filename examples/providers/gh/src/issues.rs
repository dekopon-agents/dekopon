//! Issue capabilities: read, list, comment listing, and the one issue write.

use dekopon_provider_http::{HttpError, Request, Response, method};
use dekopon_provider_sdk::ProviderError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    ACCEPT_JSON, MAX_COMMENT_OUT_BYTES, MAX_LABELS, MAX_LIST_ITEMS, MAX_PR_BODY_OUT_BYTES,
    MAX_TITLE_OUT_BYTES, RawUser, bounded_optional, decode, endpoint, github_json_request,
    has_next_link, http_failed, invalid_input, invalid_response, login_out, percent_encode,
    send_get, status_error, timestamp, truncate_text, validate_body, validate_login,
    validate_number, validate_page, validate_repo,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ReadInput {
    owner: String,
    repo: String,
    number: u32,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ListInput {
    owner: String,
    repo: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    per_page: Option<u32>,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PagedInput {
    owner: String,
    repo: String,
    number: u32,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    per_page: Option<u32>,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CommentInput {
    owner: String,
    repo: String,
    number: u32,
    body: String,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    number: u32,
    title: String,
    state: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    comments: Option<u64>,
    #[serde(default)]
    pull_request: Option<Value>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct RawComment {
    id: u64,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    body: Option<String>,
    created_at: String,
}

fn validate_state(state: Option<&str>) -> Result<&'static str, ProviderError> {
    match state {
        None | Some("open") => Ok("open"),
        Some("closed") => Ok("closed"),
        Some("all") => Ok("all"),
        Some(_) => Err(invalid_input()),
    }
}

pub(crate) fn read(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<ReadInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_number(input.number)?;
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}/issues/{}",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        input.number,
    );
    let response = send_get(send, uri, ACCEPT_JSON)?;
    let issue = decode::<RawIssue>(&response.body)?;
    if issue.number != input.number {
        return Err(invalid_response());
    }

    let (title, _) = truncate_text(&issue.title, MAX_TITLE_OUT_BYTES);
    let (body, body_truncated) = bounded_optional(issue.body.as_deref(), MAX_PR_BODY_OUT_BYTES);
    let labels = issue
        .labels
        .iter()
        .take(MAX_LABELS)
        .map(|label| truncate_text(&label.name, MAX_TITLE_OUT_BYTES).0)
        .collect::<Vec<_>>();
    Ok(json!({
        "number": issue.number,
        "title": title,
        "state": issue.state,
        "author": login_out(issue.user.as_ref()),
        "body": body,
        "bodyTruncated": body_truncated,
        "labels": labels,
        "comments": issue.comments,
        "isPullRequest": issue.pull_request.is_some(),
        "createdAt": timestamp(&issue.created_at)?,
        "updatedAt": timestamp(&issue.updated_at)?,
    }))
}

pub(crate) fn list(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<ListInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    let state = validate_state(input.state.as_deref())?;
    let (page, per_page) = validate_page(input.page, input.per_page)?;
    let per_page = per_page.unwrap_or(20);
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}/issues?state={state}&page={page}&per_page={per_page}",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
    );
    let response = send_get(send, uri, ACCEPT_JSON)?;
    let has_more = has_next_link(&response);
    let issues = decode::<Vec<RawIssue>>(&response.body)?;

    let items = issues
        .into_iter()
        .take(MAX_LIST_ITEMS)
        .map(|issue| {
            let (title, _) = truncate_text(&issue.title, MAX_TITLE_OUT_BYTES);
            Ok(json!({
                "number": issue.number,
                "title": title,
                "state": issue.state,
                "author": login_out(issue.user.as_ref()),
                "comments": issue.comments,
                "isPullRequest": issue.pull_request.is_some(),
                "createdAt": timestamp(&issue.created_at)?.to_owned(),
                "updatedAt": timestamp(&issue.updated_at)?.to_owned(),
            }))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;

    Ok(json!({
        "issues": items,
        "page": page,
        "hasMore": has_more,
    }))
}

pub(crate) fn comments(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<PagedInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_number(input.number)?;
    let (page, per_page) = validate_page(input.page, input.per_page)?;
    let per_page = per_page.unwrap_or(20);
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}/issues/{}/comments?page={page}&per_page={per_page}",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        input.number,
    );
    let response = send_get(send, uri, ACCEPT_JSON)?;
    let has_more = has_next_link(&response);
    let comments = decode::<Vec<RawComment>>(&response.body)?;

    let items = comments
        .into_iter()
        .take(MAX_LIST_ITEMS)
        .map(|comment| {
            let (body, body_truncated) =
                bounded_optional(comment.body.as_deref(), MAX_COMMENT_OUT_BYTES);
            Ok(json!({
                "commentId": comment.id,
                "author": login_out(comment.user.as_ref()),
                "body": body,
                "bodyTruncated": body_truncated,
                "createdAt": timestamp(&comment.created_at)?.to_owned(),
            }))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;

    Ok(json!({
        "comments": items,
        "page": page,
        "hasMore": has_more,
    }))
}

pub(crate) fn comment(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<CommentInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_number(input.number)?;
    validate_body(&input.body)?;
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}/issues/{}/comments",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        input.number,
    );
    let body = json!({"body": &input.body});
    let response =
        send(github_json_request(method::POST, uri, &body)?).map_err(|_| http_failed())?;
    if response.status != 201 {
        return Err(status_error(&response));
    }

    let comment = decode::<RawComment>(&response.body)?;
    if comment.id == 0 {
        return Err(invalid_response());
    }
    Ok(json!({
        "commentId": comment.id,
        "issueNumber": input.number,
        "author": login_out(comment.user.as_ref()),
        "createdAt": timestamp(&comment.created_at)?,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::invoke_with;
    use crate::testutil::{capability, json_response, scripted, step};

    fn issue(number: u32, pull: bool) -> Value {
        let mut value = json!({
            "number": number,
            "title": "Something is broken",
            "state": "open",
            "body": "details",
            "user": {"login": "reporter"},
            "labels": [{"name": "bug"}, {"name": "p1"}],
            "comments": 3,
            "created_at": "2026-08-01T00:00:00Z",
            "updated_at": "2026-08-02T00:00:00Z",
        });
        if pull {
            value["pull_request"] = json!({"url": "https://api.github.com/..."});
        }
        value
    }

    #[test]
    fn read_projects_labels_and_the_pull_flag() {
        let output = invoke_with(
            &capability("gh.issue.read"),
            json!({"owner": "octo", "repo": "hello", "number": 5}),
            scripted(vec![step(
                |request| {
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/issues/5"
                    );
                },
                json_response(200, &issue(5, false)),
            )]),
        )
        .expect("issue read succeeds");

        assert_eq!(output["number"], 5);
        assert_eq!(output["labels"], json!(["bug", "p1"]));
        assert_eq!(output["isPullRequest"], false);
        assert_eq!(output["author"], "reporter");
    }

    #[test]
    fn list_flags_pull_requests_hiding_among_issues() {
        let output = invoke_with(
            &capability("gh.issue.list"),
            json!({"owner": "octo", "repo": "hello", "state": "all"}),
            scripted(vec![step(
                |request| {
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/issues?state=all&page=1&per_page=20"
                    );
                },
                json_response(200, &json!([issue(1, false), issue(2, true)])),
            )]),
        )
        .expect("issue list succeeds");

        let items = output["issues"].as_array().expect("items");
        assert_eq!(items[0]["isPullRequest"], false);
        assert_eq!(items[1]["isPullRequest"], true);
    }

    #[test]
    fn comments_are_bounded_projections() {
        let output = invoke_with(
            &capability("gh.issue-comments.read"),
            json!({"owner": "octo", "repo": "hello", "number": 5, "perPage": 2}),
            scripted(vec![step(
                |request| {
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/issues/5/comments?page=1&per_page=2"
                    );
                },
                json_response(
                    200,
                    &json!([
                        {"id": 11, "user": {"login": "a"}, "body": "first", "created_at": "2026-08-01T00:00:00Z"},
                    ]),
                ),
            )]),
        )
        .expect("comments succeed");

        assert_eq!(output["comments"][0]["commentId"], 11);
        assert_eq!(output["comments"][0]["body"], "first");
    }

    #[test]
    fn comment_posts_the_body_and_projects_the_echo() {
        let output = invoke_with(
            &capability("gh.issue.comment"),
            json!({"owner": "octo", "repo": "hello", "number": 5, "body": "on it"}),
            scripted(vec![step(
                |request| {
                    assert_eq!(request.method, "POST");
                    assert_eq!(
                        request.uri,
                        "https://api.github.com/repos/octo/hello/issues/5/comments"
                    );
                    let body: Value = serde_json::from_slice(&request.body).expect("body is JSON");
                    assert_eq!(body, json!({"body": "on it"}));
                },
                json_response(
                    201,
                    &json!({"id": 77, "user": {"login": "xavier"}, "created_at": "2026-08-16T00:00:00Z"}),
                ),
            )]),
        )
        .expect("comment succeeds");

        assert_eq!(output["commentId"], 77);
        assert_eq!(output["issueNumber"], 5);
        assert_eq!(output["author"], "xavier");
    }

    #[test]
    fn comment_requires_a_nonempty_bounded_body() {
        for body in ["", &"x".repeat(crate::MAX_BODY_IN_BYTES + 1)] {
            let error = invoke_with(
                &capability("gh.issue.comment"),
                json!({"owner": "octo", "repo": "hello", "number": 5, "body": body}),
                |_| unreachable!("invalid body must not call HTTP"),
            )
            .expect_err("invalid body fails");
            assert_eq!(error.code(), "invalid-input");
        }
    }
}
