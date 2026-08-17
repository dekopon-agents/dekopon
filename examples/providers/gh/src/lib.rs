//! A "fake `gh`": narrow GitHub operations as separately named Dekopon capabilities.
//!
//! Every operation is one fixed REST request shape (or one fixed pre-read plus one write) against
//! `api.github.com`, projected into a small bounded output. There is deliberately no generic
//! `gh.api.*` passthrough and no GraphQL: broker HTTP constraints bind host and method but not
//! path, so path discipline is exactly what this guest exists to provide. A grant of
//! `gh.pull-request.read` is authority to read pull requests, not authority over everything the
//! broker credential can reach.
//!
//! Review events are separate capabilities (`gh.pull-request.approve`, `.comment`,
//! `.request-changes`) rather than one capability with an `event` argument, so policy can grant
//! approval authority independently of the other two. The write capabilities pre-read their pull
//! request and pin the observed head SHA into the write, refusing closed, merged, and
//! (for approval) draft pull requests, and refusing when the caller's `expectedHeadSha` no longer
//! matches — a retry against the same head converges instead of blessing new commits.
//!
//! This guest never sets `authorization`; the host rejects the header from guests by construction,
//! and broker-owned credential injection is the only path a credential takes — added inside the
//! native HTTP engine, for destinations inside the binding, where no guest can observe it.
//! Transport failures are reported as the constant `http-failed` so host detail never reaches a
//! model.
//!
//! Unlike the workspace crates, this guest cannot `#![forbid(unsafe_code)]`: the generated
//! component bindings contain `unsafe` by construction. No hand-written code here is unsafe.

use dekopon_provider_http::{Header, HttpError, Request, Response, method};
use dekopon_provider_sdk::{
    CapabilityId, EffectKind, Idempotency, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

mod content;
mod issues;
mod pulls;
mod repos;
mod reviews;

const DEFAULT_ENDPOINT: &str = "https://api.github.com";
const PRODUCTION_HOST: &str = "api.github.com";
const MAX_ENDPOINT_BYTES: usize = 512;

const ACCEPT_JSON: &str = "application/vnd.github+json";
const ACCEPT_DIFF: &str = "application/vnd.github.diff";
const API_VERSION: &str = "2022-11-28";
/// Constant on purpose: GitHub rejects requests without a `user-agent`, and interpolating anything
/// input-derived into it would hand a script a header side channel.
const USER_AGENT: &str = "dekopon-gh-provider/0.1";

// Input bounds, re-validated natively; the JSON Schemas below are model-facing metadata only.
const MAX_OWNER_BYTES: usize = 39;
const MAX_REPO_BYTES: usize = 100;
const MAX_NUMBER: u32 = 1_000_000;
const MAX_REF_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 1024;
const MAX_BODY_IN_BYTES: usize = 4 * 1024;
const MAX_PAGE: u32 = 50;
const MAX_PER_PAGE: u32 = 50;

// Output projection bounds. The broker host already ceilings total serialized output; these keep
// each field useful instead of failing the whole invocation on one large response.
const MAX_TITLE_OUT_BYTES: usize = 256;
const MAX_PR_BODY_OUT_BYTES: usize = 16 * 1024;
const MAX_PATCH_OUT_BYTES: usize = 8 * 1024;
const MAX_CONTENT_OUT_BYTES: usize = 192 * 1024;
const MAX_DIFF_OUT_BYTES: usize = 192 * 1024;
const MAX_DIR_ENTRIES: usize = 200;
const MAX_COMMENT_OUT_BYTES: usize = 4 * 1024;
const MAX_MESSAGE_OUT_BYTES: usize = 4 * 1024;
const MAX_DESCRIPTION_OUT_BYTES: usize = 1024;
const MAX_TIMESTAMP_BYTES: usize = 64;
const MAX_LOGIN_OUT_BYTES: usize = 64;
const MAX_LIST_ITEMS: usize = 50;
const MAX_LABELS: usize = 20;

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct Gh;

impl Provider for Gh {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "gh".parse().expect("static provider ID is valid"),
            description:
                "Narrow GitHub repository, pull-request, and issue operations over broker HTTP"
                    .to_owned(),
            capabilities: capabilities(),
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        invoke_with(capability, input, dekopon_provider_http::send)
    }
}

/// Routes one invocation to its capability implementation.
///
/// The send function is injected so native tests script exact request/response exchanges without
/// any network. It is `FnMut` rather than `FnOnce` because the write capabilities perform a
/// pre-read before their write, and `gh.pull-request.status` reads twice.
fn invoke_with<F>(
    capability: &CapabilityId,
    input: Value,
    mut send: F,
) -> Result<Value, ProviderError>
where
    F: FnMut(Request) -> Result<Response, HttpError>,
{
    let send: &mut dyn FnMut(Request) -> Result<Response, HttpError> = &mut send;
    match capability.as_str() {
        "gh.content.read" => content::read(input, send),
        "gh.pull-request.list" => pulls::list(input, send),
        "gh.pull-request.read" => pulls::read(input, send),
        "gh.pull-request.files" => pulls::files(input, send),
        "gh.pull-request.diff" => pulls::diff(input, send),
        "gh.pull-request.reviews" => pulls::reviews(input, send),
        "gh.pull-request.status" => pulls::status(input, send),
        "gh.pull-request.approve" => reviews::approve(input, send),
        "gh.pull-request.comment" => reviews::comment(input, send),
        "gh.pull-request.request-changes" => reviews::request_changes(input, send),
        "gh.pull-request.merge" => reviews::merge(input, send),
        "gh.repo.read" => repos::repo(input, send),
        "gh.branch.read" => repos::branch(input, send),
        "gh.commit.read" => repos::commit(input, send),
        "gh.user.read" => repos::user(input, send),
        "gh.issue.read" => issues::read(input, send),
        "gh.issue.list" => issues::list(input, send),
        "gh.issue-comments.read" => issues::comments(input, send),
        "gh.issue.comment" => issues::comment(input, send),
        _ => Err(ProviderError::new(
            "unknown-capability",
            "unsupported gh capability",
        )),
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

fn capabilities() -> Vec<ProviderCapability> {
    let read = |id: &str, description: &str, schema: Value| ProviderCapability {
        id: id.parse().expect("static capability ID is valid"),
        description: description.to_owned(),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        idempotency: Idempotency::Idempotent,
        input_schema: schema,
    };
    let write =
        |id: &str, description: &str, risk, idempotency, schema: Value| ProviderCapability {
            id: id.parse().expect("static capability ID is valid"),
            description: description.to_owned(),
            effect: EffectKind::ExternalWrite,
            risk,
            idempotency,
            input_schema: schema,
        };

    vec![
        // Tier 1 — the review workflow slice.
        read(
            "gh.content.read",
            "Reads one file or directory listing at a path and optional ref",
            repo_schema(
                json!({
                    "path": {"type": "string", "maxLength": MAX_PATH_BYTES, "description": "Repository-relative path; empty string lists the repository root."},
                    "ref": ref_property(),
                }),
                &["path"],
            ),
        ),
        read(
            "gh.pull-request.list",
            "Lists pull requests with optional state and author filters",
            repo_schema(
                json!({
                    "state": state_property(),
                    "author": {"type": "string", "maxLength": MAX_OWNER_BYTES, "description": "Optional login filter applied to the fetched page, after pagination."},
                    "page": page_property(),
                    "perPage": per_page_property(20),
                }),
                &[],
            ),
        ),
        read(
            "gh.pull-request.read",
            "Reads one pull request's metadata, state, and head/base",
            repo_schema(json!({"number": number_property()}), &["number"]),
        ),
        read(
            "gh.pull-request.files",
            "Lists one pull request's changed files with bounded patches",
            repo_schema(
                json!({
                    "number": number_property(),
                    "page": page_property(),
                    "perPage": per_page_property(30),
                    "includePatch": {"type": "boolean", "description": "Include a bounded unified patch per file; defaults to true."},
                }),
                &["number"],
            ),
        ),
        write(
            "gh.pull-request.approve",
            "Submits an APPROVE review pinned to the verified head SHA",
            RiskLevel::High,
            Idempotency::Conditional,
            repo_schema(
                json!({
                    "number": number_property(),
                    "body": body_property("Optional review comment."),
                    "expectedHeadSha": sha_property(),
                }),
                &["number"],
            ),
        ),
        // Tier 2 — review completeness.
        read(
            "gh.pull-request.reviews",
            "Lists existing reviews on one pull request",
            repo_schema(
                json!({
                    "number": number_property(),
                    "page": page_property(),
                    "perPage": per_page_property(20),
                }),
                &["number"],
            ),
        ),
        write(
            "gh.pull-request.comment",
            "Submits a COMMENT review pinned to the verified head SHA",
            RiskLevel::Medium,
            Idempotency::Conditional,
            repo_schema(
                json!({
                    "number": number_property(),
                    "body": body_property("Review comment; required."),
                    "expectedHeadSha": sha_property(),
                }),
                &["number", "body"],
            ),
        ),
        write(
            "gh.pull-request.request-changes",
            "Submits a REQUEST_CHANGES review pinned to the verified head SHA",
            RiskLevel::Medium,
            Idempotency::Conditional,
            repo_schema(
                json!({
                    "number": number_property(),
                    "body": body_property("Reason for requesting changes; required."),
                    "expectedHeadSha": sha_property(),
                }),
                &["number", "body"],
            ),
        ),
        read(
            "gh.pull-request.diff",
            "Reads one pull request's unified diff, truncated with a marker",
            repo_schema(json!({"number": number_property()}), &["number"]),
        ),
        read(
            "gh.pull-request.status",
            "Reads one pull request's head check runs",
            repo_schema(json!({"number": number_property()}), &["number"]),
        ),
        // Tier 3 — broader read surface plus the two remaining writes.
        read(
            "gh.repo.read",
            "Reads repository metadata: default branch, visibility, and flags",
            repo_schema(json!({}), &[]),
        ),
        read(
            "gh.branch.read",
            "Reads one branch's head SHA and protection flag",
            repo_schema(
                json!({"branch": {"type": "string", "maxLength": MAX_REF_BYTES, "description": "Branch name."}}),
                &["branch"],
            ),
        ),
        read(
            "gh.commit.read",
            "Reads one commit's message, author, stats, and bounded file list",
            repo_schema(json!({"ref": ref_property()}), &["ref"]),
        ),
        read(
            "gh.issue.read",
            "Reads one issue with a bounded body",
            repo_schema(json!({"number": number_property()}), &["number"]),
        ),
        read(
            "gh.issue.list",
            "Lists issues (GitHub includes pull requests; each item is flagged)",
            repo_schema(
                json!({
                    "state": state_property(),
                    "page": page_property(),
                    "perPage": per_page_property(20),
                }),
                &[],
            ),
        ),
        read(
            "gh.issue-comments.read",
            "Lists comments on one issue or pull request",
            repo_schema(
                json!({
                    "number": number_property(),
                    "page": page_property(),
                    "perPage": per_page_property(20),
                }),
                &["number"],
            ),
        ),
        write(
            "gh.issue.comment",
            "Posts one comment on an issue or pull request",
            RiskLevel::Medium,
            Idempotency::NonIdempotent,
            repo_schema(
                json!({
                    "number": number_property(),
                    "body": body_property("Comment body; required."),
                }),
                &["number", "body"],
            ),
        ),
        write(
            "gh.pull-request.merge",
            "Merges one pull request, pinned to the verified head SHA",
            RiskLevel::High,
            Idempotency::Conditional,
            repo_schema(
                json!({
                    "number": number_property(),
                    "mergeMethod": {"type": "string", "enum": ["merge", "squash", "rebase"], "description": "Merge strategy; defaults to merge."},
                    "expectedHeadSha": sha_property(),
                }),
                &["number"],
            ),
        ),
        read(
            "gh.user.read",
            "Reads one user's public profile",
            object_schema(
                json!({
                    "login": {"type": "string", "maxLength": MAX_OWNER_BYTES, "description": "GitHub login."},
                    "endpoint": endpoint_property(),
                }),
                &["login"],
            ),
        ),
    ]
}

/// Builds an object schema whose properties always include `owner`, `repo`, and `endpoint`.
fn repo_schema(mut extra: Value, required: &[&str]) -> Value {
    let properties = extra.as_object_mut().expect("schema fragments are objects");
    properties.insert(
        "owner".to_owned(),
        json!({"type": "string", "maxLength": MAX_OWNER_BYTES, "description": "Repository owner login or organization."}),
    );
    properties.insert(
        "repo".to_owned(),
        json!({"type": "string", "maxLength": MAX_REPO_BYTES, "description": "Repository name."}),
    );
    properties.insert("endpoint".to_owned(), endpoint_property());
    let mut all_required = vec!["owner", "repo"];
    all_required.extend_from_slice(required);
    object_schema(Value::Object(properties.clone()), &all_required)
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn endpoint_property() -> Value {
    json!({
        "type": "string",
        "maxLength": MAX_ENDPOINT_BYTES,
        "description": "Optional broker-constrained endpoint; defaults to the GitHub API. Plain HTTP accepts only literal loopback test endpoints."
    })
}

fn number_property() -> Value {
    json!({"type": "integer", "minimum": 1, "maximum": MAX_NUMBER})
}

fn page_property() -> Value {
    json!({"type": "integer", "minimum": 1, "maximum": MAX_PAGE, "description": "Result page, starting at 1."})
}

fn per_page_property(default: u32) -> Value {
    json!({"type": "integer", "minimum": 1, "maximum": MAX_PER_PAGE, "description": format!("Items per page; defaults to {default}.")})
}

fn state_property() -> Value {
    json!({"type": "string", "enum": ["open", "closed", "all"], "description": "State filter; defaults to open."})
}

fn ref_property() -> Value {
    json!({"type": "string", "maxLength": MAX_REF_BYTES, "description": "Branch, tag, or commit SHA."})
}

fn sha_property() -> Value {
    json!({"type": "string", "minLength": 40, "maxLength": 40, "description": "Optional 40-hex expected head SHA; the write refuses if the head moved."})
}

fn body_property(description: &str) -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": MAX_BODY_IN_BYTES, "description": description})
}

// ---------------------------------------------------------------------------
// Shared input validation
// ---------------------------------------------------------------------------

/// Validates a GitHub owner or user login: 1–39 of `[A-Za-z0-9-]`, no leading, trailing, or
/// doubled hyphen. The same grammar guards every URL segment interpolation of a login.
fn validate_login(value: &str) -> Result<(), ProviderError> {
    let bytes = value.as_bytes();
    if value.is_empty()
        || value.len() > MAX_OWNER_BYTES
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        || bytes.first() == Some(&b'-')
        || bytes.last() == Some(&b'-')
        || value.contains("--")
    {
        return Err(invalid_input());
    }
    Ok(())
}

/// Validates a repository name: 1–100 of `[A-Za-z0-9._-]`, and never a dot-only name.
fn validate_repo(value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > MAX_REPO_BYTES
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid_input());
    }
    Ok(())
}

fn validate_number(value: u32) -> Result<(), ProviderError> {
    if !(1..=MAX_NUMBER).contains(&value) {
        return Err(invalid_input());
    }
    Ok(())
}

/// Validates a git ref (branch, tag, or SHA): bounded, `[A-Za-z0-9._/-]`, no `..` segment, no
/// leading `-` or `/`, no empty segment.
fn validate_ref(value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value.len() > MAX_REF_BYTES
        || value.starts_with('-')
        || value.starts_with('/')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "..")
    {
        return Err(invalid_input());
    }
    Ok(())
}

/// Validates a repository path. The empty path is the repository root; otherwise every segment
/// must be non-empty, not `.` or `..`, and free of control bytes.
fn validate_path(value: &str) -> Result<(), ProviderError> {
    if value.len() > MAX_PATH_BYTES {
        return Err(invalid_input());
    }
    if value.is_empty() {
        return Ok(());
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(invalid_input());
    }
    for segment in value.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(invalid_input());
        }
    }
    Ok(())
}

fn validate_body(value: &str) -> Result<(), ProviderError> {
    if value.is_empty() || value.len() > MAX_BODY_IN_BYTES {
        return Err(invalid_input());
    }
    Ok(())
}

fn validate_page(
    page: Option<u32>,
    per_page: Option<u32>,
) -> Result<(u32, Option<u32>), ProviderError> {
    let page = page.unwrap_or(1);
    if !(1..=MAX_PAGE).contains(&page) {
        return Err(invalid_input());
    }
    if let Some(per_page) = per_page
        && !(1..=MAX_PER_PAGE).contains(&per_page)
    {
        return Err(invalid_input());
    }
    Ok((page, per_page))
}

fn is_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_expected_sha(value: Option<&str>) -> Result<(), ProviderError> {
    if let Some(value) = value
        && !is_sha(value)
    {
        return Err(invalid_input());
    }
    Ok(())
}

/// Percent-encodes one path segment, keeping only RFC 3986 unreserved bytes.
///
/// The validators above make this nearly a no-op, but encoding anyway means a future relaxation
/// of a charset cannot silently become path injection.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            other => {
                encoded.push('%');
                encoded.push(
                    char::from_digit(u32::from(other >> 4), 16)
                        .expect("nibble")
                        .to_ascii_uppercase(),
                );
                encoded.push(
                    char::from_digit(u32::from(other & 0xf), 16)
                        .expect("nibble")
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    encoded
}

/// Percent-encodes a validated multi-segment path, preserving `/` separators.
fn encode_path(value: &str) -> String {
    value
        .split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Endpoint and request construction
// ---------------------------------------------------------------------------

/// Resolves the request origin: production GitHub HTTPS or a literal loopback test endpoint.
fn endpoint(value: Option<&str>) -> Result<String, ProviderError> {
    let value = value.unwrap_or(DEFAULT_ENDPOINT);
    if value.len() > MAX_ENDPOINT_BYTES {
        return Err(invalid_endpoint());
    }
    if matches!(value, "https://api.github.com" | "https://api.github.com/") {
        return Ok(format!("https://{PRODUCTION_HOST}"));
    }
    let authority = value.strip_prefix("http://").ok_or_else(invalid_endpoint)?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    let address = authority
        .parse::<std::net::SocketAddr>()
        .map_err(|_| invalid_endpoint())?;
    if address.port() == 0 || !address.ip().is_loopback() {
        return Err(invalid_endpoint());
    }
    Ok(format!("http://{address}"))
}

/// Builds a request carrying exactly the constant GitHub headers.
///
/// `authorization` is never set here or anywhere else in this guest: the host rejects guest-set
/// credential headers, and broker-owned injection is the only path a credential may take.
fn github_request(
    http_method: &'static str,
    uri: String,
    accept: &'static str,
) -> Result<Request, ProviderError> {
    Ok(Request::new(http_method, uri)
        .map_err(|_| invalid_request())?
        .with_header(header("accept", accept)?)
        .with_header(header("x-github-api-version", API_VERSION)?)
        .with_header(header("user-agent", USER_AGENT)?))
}

/// Builds a JSON write request: the constant headers plus `content-type` and a serialized body.
fn github_json_request(
    http_method: &'static str,
    uri: String,
    body: &Value,
) -> Result<Request, ProviderError> {
    let body = serde_json::to_vec(body).map_err(|_| invalid_request())?;
    Ok(github_request(http_method, uri, ACCEPT_JSON)?
        .with_header(header("content-type", "application/json")?)
        .with_body(body))
}

fn header(name: &'static str, value: &'static str) -> Result<Header, ProviderError> {
    Header::text(name, value).map_err(|_| invalid_request())
}

/// Sends one GET and maps every non-200 status to its stable error code.
fn send_get(
    send: &mut dyn FnMut(Request) -> Result<Response, HttpError>,
    uri: String,
    accept: &'static str,
) -> Result<Response, ProviderError> {
    let response = send(github_request(method::GET, uri, accept)?).map_err(|_| http_failed())?;
    if response.status != 200 {
        return Err(status_error(&response));
    }
    Ok(response)
}

/// Maps a non-success GitHub status to a stable, model-safe error code.
///
/// A 403 is rate limiting only when GitHub says the primary quota is exhausted; a plain 403 is an
/// authorization refusal. A 429 is rate limiting by definition, with or without the header.
fn status_error(response: &Response) -> ProviderError {
    match response.status {
        401 => ProviderError::new("unauthorized", "endpoint rejected the request credentials"),
        403 if rate_limit_exhausted(response) => rate_limited(),
        403 => ProviderError::new("forbidden", "endpoint refused the request"),
        404 => ProviderError::new("not-found", "the requested resource was not found"),
        422 => ProviderError::new("unprocessable", "endpoint refused the request as invalid"),
        429 => rate_limited(),
        _ => unexpected_status(),
    }
}

fn rate_limited() -> ProviderError {
    ProviderError::new("rate-limited", "endpoint rate limit is exhausted")
}

fn rate_limit_exhausted(response: &Response) -> bool {
    response
        .header_values("x-ratelimit-remaining")
        .any(|value| value == b"0")
}

/// Reports whether a paginated response advertises another page via `Link: rel="next"`.
fn has_next_link(response: &Response) -> bool {
    response.header_values("link").any(|value| {
        core::str::from_utf8(value)
            .is_ok_and(|link| link.split(',').any(|part| part.contains("rel=\"next\"")))
    })
}

// ---------------------------------------------------------------------------
// Shared response handling
// ---------------------------------------------------------------------------

/// Decodes a JSON response body, collapsing every parse failure to `invalid-response`.
fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice::<T>(body).map_err(|_| invalid_response())
}

/// Returns a bounded copy of `value` and whether it was truncated, never splitting a character.
fn truncate_text(value: &str, max: usize) -> (String, bool) {
    if value.len() <= max {
        return (value.to_owned(), false);
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

/// Projects an optional free-text field into `(text, truncated)` JSON values.
fn bounded_optional(value: Option<&str>, max: usize) -> (Value, bool) {
    match value {
        Some(text) => {
            let (text, truncated) = truncate_text(text, max);
            (Value::String(text), truncated)
        }
        None => (Value::Null, false),
    }
}

/// Accepts a response timestamp only in its expected bounded shape.
fn timestamp(value: &str) -> Result<&str, ProviderError> {
    if value.is_empty() || value.len() > MAX_TIMESTAMP_BYTES {
        return Err(invalid_response());
    }
    Ok(value)
}

/// Projects an optional response login, bounding it rather than trusting response sizes.
fn login_out(value: Option<&RawUser>) -> Value {
    match value {
        Some(user) if !user.login.is_empty() && user.login.len() <= MAX_LOGIN_OUT_BYTES => {
            Value::String(user.login.clone())
        }
        _ => Value::Null,
    }
}

/// The `user`/`author` object shape GitHub embeds in most resources.
#[derive(Debug, serde::Deserialize)]
struct RawUser {
    login: String,
}

// ---------------------------------------------------------------------------
// Stable errors
// ---------------------------------------------------------------------------

fn invalid_input() -> ProviderError {
    ProviderError::new(
        "invalid-input",
        "input does not match the capability contract",
    )
}

fn invalid_endpoint() -> ProviderError {
    ProviderError::new(
        "invalid-endpoint",
        "endpoint must be production GitHub HTTPS or explicit loopback HTTP",
    )
}

fn invalid_request() -> ProviderError {
    ProviderError::new(
        "invalid-request",
        "could not construct bounded HTTP request",
    )
}

fn http_failed() -> ProviderError {
    ProviderError::new("http-failed", "broker HTTP request failed")
}

fn unexpected_status() -> ProviderError {
    ProviderError::new(
        "unexpected-status",
        "endpoint returned an unexpected status",
    )
}

fn invalid_response() -> ProviderError {
    ProviderError::new("invalid-response", "endpoint returned an invalid resource")
}

dekopon_provider_sdk::export_provider_with_bindings!(Gh, bindings);

// ---------------------------------------------------------------------------
// Test support and shared tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testutil {
    use std::collections::VecDeque;

    use dekopon_provider_http::{Header, HttpError, Request, Response};
    use serde_json::Value;

    /// One scripted exchange: assertions to run on the request, then the canned reply.
    pub(crate) struct Step {
        pub check: Box<dyn Fn(&Request)>,
        pub reply: Result<Response, HttpError>,
    }

    pub(crate) fn step(
        check: impl Fn(&Request) + 'static,
        reply: Result<Response, HttpError>,
    ) -> Step {
        Step {
            check: Box::new(check),
            reply,
        }
    }

    /// Builds a scripted send function that fails loudly on any extra or out-of-order request.
    ///
    /// Every request is also held to the guest's constant-header contract: `user-agent` and
    /// `x-github-api-version` present, `authorization` absent — the latter asserted here so no
    /// individual test can forget it.
    pub(crate) fn scripted(steps: Vec<Step>) -> impl FnMut(Request) -> Result<Response, HttpError> {
        let mut steps = steps.into_iter().collect::<VecDeque<_>>();
        move |request: Request| {
            let step = steps.pop_front().expect("unexpected extra HTTP request");
            assert_standard_headers(&request);
            (step.check)(&request);
            step.reply
        }
    }

    pub(crate) fn assert_standard_headers(request: &Request) {
        assert!(
            !request
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("authorization")),
            "guest must never set authorization"
        );
        assert!(
            request
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("user-agent")),
            "user-agent is required by GitHub"
        );
        assert!(
            request.headers.iter().any(|header| header
                .name
                .eq_ignore_ascii_case("x-github-api-version")
                && header.value == b"2022-11-28"),
            "api version pin is required"
        );
    }

    pub(crate) fn json_response(status: u16, body: &Value) -> Result<Response, HttpError> {
        Ok(Response {
            status,
            headers: Vec::new(),
            body: serde_json::to_vec(body).expect("mock body serializes"),
        })
    }

    pub(crate) fn response_with_headers(
        status: u16,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<Response, HttpError> {
        Ok(Response {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| Header::text(*name, *value).expect("mock header"))
                .collect(),
            body: serde_json::to_vec(body).expect("mock body serializes"),
        })
    }

    pub(crate) fn capability(value: &str) -> dekopon_provider_sdk::CapabilityId {
        value.parse().expect("valid capability fixture")
    }

    pub(crate) fn accept_of(request: &Request) -> Vec<u8> {
        request
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("accept"))
            .map(|header| header.value.clone())
            .expect("accept header present")
    }
}

#[cfg(test)]
mod tests {
    use dekopon_provider_http::HttpErrorCode;
    use dekopon_provider_sdk::{EffectKind, Provider, RiskLevel};
    use serde_json::json;

    use super::testutil::{capability, scripted, step};
    use super::{Gh, endpoint, invoke_with, truncate_text};

    #[test]
    fn manifest_covers_the_full_designed_surface() {
        let manifest = Gh::manifest();
        assert_eq!(manifest.id.as_str(), "gh");
        assert_eq!(manifest.capabilities.len(), 19);

        let external_writes = manifest
            .capabilities
            .iter()
            .filter(|capability| capability.effect == EffectKind::ExternalWrite)
            .map(|capability| capability.id.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            external_writes,
            vec![
                "gh.pull-request.approve",
                "gh.pull-request.comment",
                "gh.pull-request.request-changes",
                "gh.issue.comment",
                "gh.pull-request.merge",
            ]
        );

        for capability in &manifest.capabilities {
            assert_eq!(
                capability.input_schema["type"], "object",
                "{}",
                capability.id
            );
            assert_eq!(
                capability.input_schema["additionalProperties"],
                json!(false),
                "{}",
                capability.id
            );
            // No passthrough capability wears the gh costume.
            assert!(
                !capability.id.as_str().starts_with("gh.api"),
                "{}",
                capability.id
            );
        }

        let high_risk = manifest
            .capabilities
            .iter()
            .filter(|capability| capability.risk == RiskLevel::High)
            .map(|capability| capability.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            high_risk,
            vec!["gh.pull-request.approve", "gh.pull-request.merge"]
        );
    }

    #[test]
    fn endpoints_fail_closed() {
        assert_eq!(
            endpoint(None).expect("default endpoint is valid"),
            "https://api.github.com"
        );
        assert_eq!(
            endpoint(Some("https://api.github.com/")).expect("trailing slash accepted"),
            "https://api.github.com"
        );
        for denied in [
            "https://api.github.com.evil.com",
            "https://github.com",
            "https://example.com",
            "https://user@api.github.com",
            "https://api.github.com/repos",
            "https://api.github.com?x=1",
            "http://api.github.com",
            "http://127.0.0.1",
            "http://192.168.1.10:8080",
            "http://localhost:8080",
        ] {
            assert!(endpoint(Some(denied)).is_err(), "accepted {denied}");
        }
        assert!(
            endpoint(Some("http://127.0.0.1:43123")).is_ok(),
            "literal loopback with port is the test escape hatch"
        );
    }

    #[test]
    fn unknown_capabilities_are_rejected_without_http() {
        let error = invoke_with(&capability("gh.api.get"), json!({}), |_| {
            unreachable!("unknown capability must not call HTTP")
        })
        .expect_err("unknown capability fails");
        assert_eq!(error.code(), "unknown-capability");
    }

    #[test]
    fn transport_failures_never_expose_host_detail() {
        let error = invoke_with(
            &capability("gh.repo.read"),
            json!({"owner": "octo", "repo": "hello"}),
            scripted(vec![step(
                |_| {},
                Err(dekopon_provider_http::HttpError {
                    code: HttpErrorCode::Denied,
                    message: "secret internal path and header detail".to_owned(),
                }),
            )]),
        )
        .expect_err("host denial fails the invocation");
        assert_eq!(error.code(), "http-failed");
        assert_eq!(error.message(), "broker HTTP request failed");
    }

    #[test]
    fn rate_limit_discrimination_follows_the_remaining_header() {
        for (status, headers, expected) in [
            (
                403_u16,
                vec![("x-ratelimit-remaining", "0")],
                "rate-limited",
            ),
            (403, vec![("x-ratelimit-remaining", "12")], "forbidden"),
            (403, vec![], "forbidden"),
            (401, vec![], "unauthorized"),
            (429, vec![], "rate-limited"),
            (422, vec![], "unprocessable"),
            (404, vec![], "not-found"),
            (500, vec![], "unexpected-status"),
        ] {
            let error = invoke_with(
                &capability("gh.repo.read"),
                json!({"owner": "octo", "repo": "hello"}),
                scripted(vec![step(
                    |_| {},
                    super::testutil::response_with_headers(status, &headers, &json!({})),
                )]),
            )
            .expect_err("non-200 fails");
            assert_eq!(error.code(), expected, "status {status}");
        }
    }

    #[test]
    fn malformed_bodies_are_invalid_response() {
        let error = invoke_with(
            &capability("gh.repo.read"),
            json!({"owner": "octo", "repo": "hello"}),
            scripted(vec![step(|_| {}, {
                Ok(dekopon_provider_http::Response {
                    status: 200,
                    headers: Vec::new(),
                    body: b"not json".to_vec(),
                })
            })]),
        )
        .expect_err("malformed body fails");
        assert_eq!(error.code(), "invalid-response");
    }

    #[test]
    fn input_grammar_failures_never_reach_http() {
        for (capability_id, input) in [
            ("gh.repo.read", json!({"owner": "-bad", "repo": "x"})),
            ("gh.repo.read", json!({"owner": "a--b", "repo": "x"})),
            ("gh.repo.read", json!({"owner": "octo", "repo": ".."})),
            (
                "gh.pull-request.read",
                json!({"owner": "octo", "repo": "x", "number": 0}),
            ),
            (
                "gh.pull-request.read",
                json!({"owner": "octo", "repo": "x", "number": 1_000_001}),
            ),
            (
                "gh.content.read",
                json!({"owner": "octo", "repo": "x", "path": "a/../b"}),
            ),
            (
                "gh.content.read",
                json!({"owner": "octo", "repo": "x", "path": "/leading"}),
            ),
            (
                "gh.commit.read",
                json!({"owner": "octo", "repo": "x", "ref": "-rev"}),
            ),
            (
                "gh.commit.read",
                json!({"owner": "octo", "repo": "x", "ref": "a//b"}),
            ),
            (
                "gh.pull-request.approve",
                json!({"owner": "octo", "repo": "x", "number": 1, "expectedHeadSha": "short"}),
            ),
            (
                "gh.pull-request.comment",
                json!({"owner": "octo", "repo": "x", "number": 1, "body": ""}),
            ),
            (
                "gh.pull-request.list",
                json!({"owner": "octo", "repo": "x", "page": 51}),
            ),
            (
                "gh.pull-request.list",
                json!({"owner": "octo", "repo": "x", "state": "merged"}),
            ),
            ("gh.user.read", json!({"login": "bad login"})),
            (
                "gh.repo.read",
                json!({"owner": "octo", "repo": "x", "extra": true}),
            ),
        ] {
            let error = invoke_with(&capability(capability_id), input.clone(), |_| {
                unreachable!("invalid input must not call HTTP: {capability_id} {input}")
            })
            .expect_err("invalid input fails");
            assert_eq!(error.code(), "invalid-input", "{capability_id} {input}");
        }
    }

    #[test]
    fn truncation_is_exact_and_character_safe() {
        let (text, truncated) = truncate_text("abcdef", 6);
        assert_eq!((text.as_str(), truncated), ("abcdef", false));
        let (text, truncated) = truncate_text("abcdefg", 6);
        assert_eq!((text.as_str(), truncated), ("abcdef", true));
        // A four-byte scalar straddling the boundary is dropped whole.
        let (text, truncated) = truncate_text("abcd😀", 6);
        assert_eq!((text.as_str(), truncated), ("abcd", true));
    }
}
