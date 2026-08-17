//! Write capabilities: review submission (approve, comment, request changes) and merge.
//!
//! Every write pre-reads its pull request and pins the observed head SHA into the write body.
//! The pre-read is what makes these `conditional` rather than `non-idempotent`: a retry against
//! an unchanged head converges on the same review or merge state, and a moved head is refused
//! with `head-changed` instead of silently blessing commits the caller never saw.

use dekopon_provider_http::{HttpError, Request, Response, method};
use dekopon_provider_sdk::ProviderError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::pulls::{RawPull, fetch_pull};
use crate::{
    MAX_MESSAGE_OUT_BYTES, RawUser, decode, endpoint, github_json_request, http_failed,
    invalid_input, invalid_response, login_out, percent_encode, status_error, truncate_text,
    validate_body, validate_expected_sha, validate_login, validate_number, validate_repo,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ReviewInput {
    owner: String,
    repo: String,
    number: u32,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    expected_head_sha: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct MergeInput {
    owner: String,
    repo: String,
    number: u32,
    #[serde(default)]
    merge_method: Option<MergeMethod>,
    #[serde(default)]
    expected_head_sha: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}

/// One review event, fixed per capability so policy separates the three authority levels.
#[derive(Clone, Copy)]
enum ReviewEvent {
    Approve,
    Comment,
    RequestChanges,
}

impl ReviewEvent {
    fn wire(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::Comment => "COMMENT",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }

    /// The state GitHub echoes for a submitted review of this event.
    fn expected_state(self) -> &'static str {
        match self {
            Self::Approve => "APPROVED",
            Self::Comment => "COMMENTED",
            Self::RequestChanges => "CHANGES_REQUESTED",
        }
    }

    fn body_required(self) -> bool {
        !matches!(self, Self::Approve)
    }
}

#[derive(Debug, Deserialize)]
struct RawReview {
    id: u64,
    state: String,
    commit_id: String,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    submitted_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawMerge {
    sha: String,
    merged: bool,
    #[serde(default)]
    message: Option<String>,
}

pub(crate) fn approve(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    submit_review(input, send, ReviewEvent::Approve)
}

pub(crate) fn comment(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    submit_review(input, send, ReviewEvent::Comment)
}

pub(crate) fn request_changes(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    submit_review(input, send, ReviewEvent::RequestChanges)
}

fn submit_review(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
    event: ReviewEvent,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<ReviewInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_number(input.number)?;
    validate_expected_sha(input.expected_head_sha.as_deref())?;
    match input.body.as_deref() {
        Some(body) => validate_body(body)?,
        None if event.body_required() => return Err(invalid_input()),
        None => {}
    }
    let endpoint = endpoint(input.endpoint.as_deref())?;

    // Pre-read: the refusals below happen before any write leaves this guest, so a mock that
    // panics on a second request proves the state machine wrote nothing.
    let pull = fetch_pull(send, &endpoint, &input.owner, &input.repo, input.number)?;
    require_open(&pull)?;
    if matches!(event, ReviewEvent::Approve) && pull.draft {
        return Err(ProviderError::new(
            "pr-draft",
            "the pull request is a draft; approving a draft is refused",
        ));
    }
    require_expected_head(&pull, input.expected_head_sha.as_deref())?;

    let mut body = json!({
        "event": event.wire(),
        "commit_id": pull.head.sha,
    });
    if let Some(text) = input.body.as_deref() {
        body["body"] = Value::String(text.to_owned());
    }
    let uri = format!(
        "{endpoint}/repos/{}/{}/pulls/{}/reviews",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        input.number,
    );
    let response =
        send(github_json_request(method::POST, uri, &body)?).map_err(|_| http_failed())?;
    if response.status != 200 {
        return Err(status_error(&response));
    }

    let review = decode::<RawReview>(&response.body)?;
    // The echo must confirm both the event and the pinned commit, or the write is reported as
    // invalid rather than trusted.
    if review.state != event.expected_state()
        || !review.commit_id.eq_ignore_ascii_case(&pull.head.sha)
    {
        return Err(invalid_response());
    }

    Ok(json!({
        "reviewId": review.id,
        "state": review.state,
        "pullNumber": pull.number,
        "headSha": pull.head.sha,
        "author": login_out(review.user.as_ref()),
        "submittedAt": review.submitted_at,
    }))
}

pub(crate) fn merge(
    input: Value,
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
) -> Result<Value, ProviderError> {
    let input = serde_json::from_value::<MergeInput>(input).map_err(|_| invalid_input())?;
    validate_login(&input.owner)?;
    validate_repo(&input.repo)?;
    validate_number(input.number)?;
    validate_expected_sha(input.expected_head_sha.as_deref())?;
    let merge_method = input.merge_method.unwrap_or(MergeMethod::Merge);
    let endpoint = endpoint(input.endpoint.as_deref())?;

    let pull = fetch_pull(send, &endpoint, &input.owner, &input.repo, input.number)?;
    require_open(&pull)?;
    require_expected_head(&pull, input.expected_head_sha.as_deref())?;

    let uri = format!(
        "{endpoint}/repos/{}/{}/pulls/{}/merge",
        percent_encode(&input.owner),
        percent_encode(&input.repo),
        input.number,
    );
    let body = json!({
        "sha": pull.head.sha,
        "merge_method": merge_method.as_str(),
    });
    let response =
        send(github_json_request(method::PUT, uri, &body)?).map_err(|_| http_failed())?;
    // GitHub refuses a blocked or conflicting merge with 405, and a stale head with 409. The
    // sha pin makes 409 reachable only in the race window between pre-read and merge.
    if matches!(response.status, 405 | 409) {
        return Err(ProviderError::new(
            "merge-conflict",
            "the merge was refused as conflicting or blocked",
        ));
    }
    if response.status != 200 {
        return Err(status_error(&response));
    }

    let merged = decode::<RawMerge>(&response.body)?;
    if !merged.merged {
        return Err(invalid_response());
    }
    let (message, _) = truncate_text(
        merged.message.as_deref().unwrap_or(""),
        MAX_MESSAGE_OUT_BYTES,
    );

    Ok(json!({
        "merged": true,
        "sha": merged.sha,
        "pullNumber": pull.number,
        "headSha": pull.head.sha,
        "message": message,
    }))
}

/// Refuses closed or merged pull requests before any write is constructed.
fn require_open(pull: &RawPull) -> Result<(), ProviderError> {
    if pull.merged.unwrap_or(false) || pull.state != "open" {
        return Err(ProviderError::new(
            "pr-closed",
            "the pull request is closed or already merged",
        ));
    }
    Ok(())
}

/// Refuses a head that moved past the caller's expectation.
fn require_expected_head(pull: &RawPull, expected: Option<&str>) -> Result<(), ProviderError> {
    if let Some(expected) = expected
        && !pull.head.sha.eq_ignore_ascii_case(expected)
    {
        return Err(ProviderError::new(
            "head-changed",
            "the pull request head no longer matches expectedHeadSha",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::invoke_with;
    use crate::testutil::{capability, json_response, scripted, step};

    fn pull(state: &str, draft: bool, merged: bool) -> Value {
        json!({
            "number": 7,
            "title": "A change",
            "state": state,
            "draft": draft,
            "merged": merged,
            "user": {"login": "cpetersen"},
            "head": {"ref": "feature/x", "sha": "a".repeat(40)},
            "base": {"ref": "main", "sha": "b".repeat(40)},
            "created_at": "2026-08-01T00:00:00Z",
            "updated_at": "2026-08-02T00:00:00Z",
        })
    }

    fn review_echo(state: &str) -> Value {
        json!({
            "id": 4242,
            "state": state,
            "commit_id": "a".repeat(40),
            "user": {"login": "xavier"},
            "submitted_at": "2026-08-16T00:00:00Z",
        })
    }

    #[test]
    fn approve_pins_the_observed_head_into_the_review() {
        let output = invoke_with(
            &capability("gh.pull-request.approve"),
            json!({"owner": "octo", "repo": "hello", "number": 7, "body": "lgtm"}),
            scripted(vec![
                step(
                    |request| {
                        assert_eq!(request.method, "GET");
                        assert_eq!(
                            request.uri,
                            "https://api.github.com/repos/octo/hello/pulls/7"
                        );
                    },
                    json_response(200, &pull("open", false, false)),
                ),
                step(
                    |request| {
                        assert_eq!(request.method, "POST");
                        assert_eq!(
                            request.uri,
                            "https://api.github.com/repos/octo/hello/pulls/7/reviews"
                        );
                        let body: Value =
                            serde_json::from_slice(&request.body).expect("body is JSON");
                        // The pin is the whole point: the POST carries the GET's head SHA.
                        assert_eq!(body["commit_id"], "a".repeat(40));
                        assert_eq!(body["event"], "APPROVE");
                        assert_eq!(body["body"], "lgtm");
                        assert!(
                            request
                                .headers
                                .iter()
                                .any(|header| header.name.eq_ignore_ascii_case("content-type")
                                    && header.value == b"application/json")
                        );
                    },
                    json_response(200, &review_echo("APPROVED")),
                ),
            ]),
        )
        .expect("approve succeeds");

        assert_eq!(output["reviewId"], 4242);
        assert_eq!(output["state"], "APPROVED");
        assert_eq!(output["headSha"], "a".repeat(40));
        assert_eq!(output["author"], "xavier");
    }

    #[test]
    fn approve_refuses_closed_pulls_before_any_write() {
        for (state, merged) in [("closed", false), ("open", true)] {
            let error = invoke_with(
                &capability("gh.pull-request.approve"),
                json!({"owner": "octo", "repo": "hello", "number": 7}),
                scripted(vec![step(
                    |_| {},
                    json_response(200, &pull(state, false, merged)),
                )]),
            )
            .expect_err("closed pull refuses");
            // A second request would panic the scripted mock: refusal proves zero writes.
            assert_eq!(error.code(), "pr-closed");
        }
    }

    #[test]
    fn approve_refuses_drafts_before_any_write() {
        let error = invoke_with(
            &capability("gh.pull-request.approve"),
            json!({"owner": "octo", "repo": "hello", "number": 7}),
            scripted(vec![step(
                |_| {},
                json_response(200, &pull("open", true, false)),
            )]),
        )
        .expect_err("draft refuses");
        assert_eq!(error.code(), "pr-draft");
    }

    #[test]
    fn approve_refuses_a_moved_head_before_any_write() {
        let error = invoke_with(
            &capability("gh.pull-request.approve"),
            json!({
                "owner": "octo",
                "repo": "hello",
                "number": 7,
                "expectedHeadSha": "c".repeat(40),
            }),
            scripted(vec![step(
                |_| {},
                json_response(200, &pull("open", false, false)),
            )]),
        )
        .expect_err("moved head refuses");
        assert_eq!(error.code(), "head-changed");
    }

    #[test]
    fn approve_rejects_an_echo_that_contradicts_the_pin() {
        let mut echo = review_echo("APPROVED");
        echo["commit_id"] = json!("d".repeat(40));
        let error = invoke_with(
            &capability("gh.pull-request.approve"),
            json!({"owner": "octo", "repo": "hello", "number": 7}),
            scripted(vec![
                step(|_| {}, json_response(200, &pull("open", false, false))),
                step(|_| {}, json_response(200, &echo)),
            ]),
        )
        .expect_err("contradictory echo fails");
        assert_eq!(error.code(), "invalid-response");
    }

    #[test]
    fn comment_and_request_changes_require_a_body_and_use_their_event() {
        for (capability_id, event, state) in [
            ("gh.pull-request.comment", "COMMENT", "COMMENTED"),
            (
                "gh.pull-request.request-changes",
                "REQUEST_CHANGES",
                "CHANGES_REQUESTED",
            ),
        ] {
            let error = invoke_with(
                &capability(capability_id),
                json!({"owner": "octo", "repo": "hello", "number": 7}),
                |_| unreachable!("missing body must not call HTTP"),
            )
            .expect_err("missing body fails");
            assert_eq!(error.code(), "invalid-input", "{capability_id}");

            let output = invoke_with(
                &capability(capability_id),
                json!({"owner": "octo", "repo": "hello", "number": 7, "body": "needs work"}),
                scripted(vec![
                    step(|_| {}, json_response(200, &pull("open", false, false))),
                    step(
                        move |request| {
                            let body: Value =
                                serde_json::from_slice(&request.body).expect("body is JSON");
                            assert_eq!(body["event"], event);
                        },
                        json_response(200, &review_echo(state)),
                    ),
                ]),
            )
            .expect("review succeeds");
            assert_eq!(output["state"], state, "{capability_id}");
        }
    }

    #[test]
    fn comment_reviews_are_allowed_on_drafts() {
        // Only approval refuses drafts; a COMMENT review of a draft is ordinary feedback.
        invoke_with(
            &capability("gh.pull-request.comment"),
            json!({"owner": "octo", "repo": "hello", "number": 7, "body": "early note"}),
            scripted(vec![
                step(|_| {}, json_response(200, &pull("open", true, false))),
                step(|_| {}, json_response(200, &review_echo("COMMENTED"))),
            ]),
        )
        .expect("draft comment succeeds");
    }

    #[test]
    fn merge_pins_the_sha_and_maps_conflicts() {
        let output = invoke_with(
            &capability("gh.pull-request.merge"),
            json!({"owner": "octo", "repo": "hello", "number": 7, "mergeMethod": "squash"}),
            scripted(vec![
                step(|_| {}, json_response(200, &pull("open", false, false))),
                step(
                    |request| {
                        assert_eq!(request.method, "PUT");
                        assert_eq!(
                            request.uri,
                            "https://api.github.com/repos/octo/hello/pulls/7/merge"
                        );
                        let body: Value =
                            serde_json::from_slice(&request.body).expect("body is JSON");
                        assert_eq!(body["sha"], "a".repeat(40));
                        assert_eq!(body["merge_method"], "squash");
                    },
                    json_response(
                        200,
                        &json!({"sha": "e".repeat(40), "merged": true, "message": "Pull Request successfully merged"}),
                    ),
                ),
            ]),
        )
        .expect("merge succeeds");
        assert_eq!(output["merged"], true);
        assert_eq!(output["sha"], "e".repeat(40));

        for conflict_status in [405_u16, 409] {
            let error = invoke_with(
                &capability("gh.pull-request.merge"),
                json!({"owner": "octo", "repo": "hello", "number": 7}),
                scripted(vec![
                    step(|_| {}, json_response(200, &pull("open", false, false))),
                    step(|_| {}, json_response(conflict_status, &json!({}))),
                ]),
            )
            .expect_err("conflict fails");
            assert_eq!(error.code(), "merge-conflict", "status {conflict_status}");
        }
    }

    #[test]
    fn merge_refuses_closed_and_moved_heads_before_any_write() {
        let error = invoke_with(
            &capability("gh.pull-request.merge"),
            json!({"owner": "octo", "repo": "hello", "number": 7}),
            scripted(vec![step(
                |_| {},
                json_response(200, &pull("closed", false, false)),
            )]),
        )
        .expect_err("closed refuses");
        assert_eq!(error.code(), "pr-closed");

        let error = invoke_with(
            &capability("gh.pull-request.merge"),
            json!({
                "owner": "octo",
                "repo": "hello",
                "number": 7,
                "expectedHeadSha": "f".repeat(40),
            }),
            scripted(vec![step(
                |_| {},
                json_response(200, &pull("open", false, false)),
            )]),
        )
        .expect_err("moved head refuses");
        assert_eq!(error.code(), "head-changed");
    }
}
