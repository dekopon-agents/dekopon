//! An explicitly unsupported proof of concept for two reads from Skylight's private app API.
//!
//! This broker-only guest fixes both request paths, accepts no selectors or transport fields, and
//! projects untrusted JSON:API responses into small typed outputs. It never sets `authorization`:
//! the broker may inject one destination-bound credential only after validating the guest request.
//!
//! Route evidence and the `attributes.name` / `attributes.label` fallback were adapted from
//! `joshuaswarren/pyskylight` commit `69e4576b9035d71aacda9ade7a4afea05a663e94` (MIT). See
//! `../THIRD_PARTY_NOTICES.md`. This is a native Rust reimplementation; Python is not embedded.

use std::collections::BTreeSet;

use dekopon_provider_http::{Header, HttpError, Request, Response, method};
use dekopon_provider_sdk::{
    CapabilityId, EffectKind, Idempotency, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

const ACCOUNT_CAPABILITY: &str = "skylight.private.account.read";
const FRAMES_CAPABILITY: &str = "skylight.private.frames.list";
const ACCOUNT_URI: &str = "https://app.ourskylight.com/api/user";
const FRAMES_URI: &str = "https://app.ourskylight.com/api/frames";
const ACCEPT_JSON: &str = "application/json";
/// Constant and Dekopon-specific so the guest neither impersonates upstream software nor exposes
/// input through a header side channel.
const USER_AGENT: &str =
    "dekopon-skylight-private-provider/0.1 (+https://github.com/dekopon-agents/dekopon)";

const MAX_ID_BYTES: usize = 128;
const MAX_NAME_BYTES: usize = 256;
const MAX_FRAMES: usize = 32;
const MAX_RESPONSE_BODY_BYTES: usize = 256 * 1024;
const MAX_COMPONENT_OUTPUT_BYTES: usize = 32 * 1024;
/// Leave room for the SDK's fixed `{"outcome":"succeeded","output":...}` envelope so the complete
/// component result, not just the projected value, remains below 32 KiB.
const COMPONENT_OUTPUT_ENVELOPE_RESERVE: usize = 128;
const MAX_PROJECTED_OUTPUT_BYTES: usize =
    MAX_COMPONENT_OUTPUT_BYTES - COMPONENT_OUTPUT_ENVELOPE_RESERVE;
const NAME_TRUNCATION_MARKER: &str = "…";

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct SkylightPrivate;

impl Provider for SkylightPrivate {
    fn manifest() -> ProviderManifest {
        let capability = |id: &str, description: &str| ProviderCapability {
            id: id.parse().expect("static capability ID is valid"),
            description: description.to_owned(),
            effect: EffectKind::ReadOnly,
            risk: RiskLevel::Medium,
            idempotency: Idempotency::Idempotent,
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        };

        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "skylight-private"
                .parse()
                .expect("static provider ID is valid"),
            description: "Unsupported private Skylight account and frame reads over broker HTTP"
                .to_owned(),
            command_words: Vec::new(),
            capabilities: vec![
                capability(
                    ACCOUNT_CAPABILITY,
                    "Reads only the bearer-selected account identifier",
                ),
                capability(
                    FRAMES_CAPABILITY,
                    "Lists bounded identifiers and optional names for visible frames",
                ),
            ],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        invoke_with(capability, input, dekopon_provider_http::send)
    }
}

/// Empty only by construction: the explicit object check below distinguishes `{}` from every other
/// JSON value, while `deny_unknown_fields` keeps this Rust decoder aligned with the manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

fn invoke_with<F>(capability: &CapabilityId, input: Value, send: F) -> Result<Value, ProviderError>
where
    F: FnOnce(Request) -> Result<Response, HttpError>,
{
    match capability.as_str() {
        ACCOUNT_CAPABILITY => read_account(input, send),
        FRAMES_CAPABILITY => list_frames(input, send),
        _ => Err(unknown_capability()),
    }
}

fn read_account<F>(input: Value, send: F) -> Result<Value, ProviderError>
where
    F: FnOnce(Request) -> Result<Response, HttpError>,
{
    validate_empty_input(input)?;
    let body = send_once(ACCOUNT_URI, send)?;
    let envelope = decode_account(&body)?;
    validate_id(&envelope.data.id)?;
    bounded_output(json!({"account": {"id": envelope.data.id}}))
}

fn list_frames<F>(input: Value, send: F) -> Result<Value, ProviderError>
where
    F: FnOnce(Request) -> Result<Response, HttpError>,
{
    validate_empty_input(input)?;
    let body = send_once(FRAMES_URI, send)?;
    let envelope = decode_frames(&body)?;
    project_frames(envelope.data)
}

fn validate_empty_input(input: Value) -> Result<(), ProviderError> {
    if !matches!(&input, Value::Object(fields) if fields.is_empty()) {
        return Err(invalid_input());
    }
    serde_json::from_value::<EmptyInput>(input)
        .map(|_| ())
        .map_err(|_| invalid_input())
}

/// Performs exactly one broker call with a fixed GET request and maps all host detail away.
fn send_once<F>(uri: &'static str, send: F) -> Result<Vec<u8>, ProviderError>
where
    F: FnOnce(Request) -> Result<Response, HttpError>,
{
    let request = Request::new(method::GET, uri)
        .map_err(|_| invalid_request())?
        .with_header(header("accept", ACCEPT_JSON)?)
        .with_header(header("user-agent", USER_AGENT)?);
    let response = send(request).map_err(|_| http_failed())?;
    if response.status != 200 {
        return Err(status_error(response.status));
    }
    if response.body.len() > MAX_RESPONSE_BODY_BYTES {
        return Err(invalid_response());
    }
    Ok(response.body)
}

fn header(name: &'static str, value: &'static str) -> Result<Header, ProviderError> {
    Header::text(name, value).map_err(|_| invalid_request())
}

#[derive(Debug, Deserialize)]
struct AccountEnvelope {
    data: IdentityResource,
}

#[derive(Debug, Deserialize)]
struct IdentityResource {
    id: String,
}

#[derive(Debug, Deserialize)]
struct FramesEnvelope {
    data: Vec<FrameResource>,
}

#[derive(Debug, Deserialize)]
struct FrameResource {
    id: String,
    /// Missing attributes mean an unnamed frame; a present non-object is invalid.
    #[serde(default = "empty_attributes")]
    attributes: Value,
}

fn empty_attributes() -> Value {
    Value::Object(Default::default())
}

#[derive(Debug, Default, Deserialize)]
struct FrameAttributes {
    /// A present value must be a string. `null` is not silently treated as an absent field.
    #[serde(default, deserialize_with = "present_string")]
    name: Option<String>,
    #[serde(default, deserialize_with = "present_string")]
    label: Option<String>,
}

fn present_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectedFrame {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    name_truncated: bool,
}

#[derive(Serialize)]
struct FramesOutput<'a> {
    frames: &'a [ProjectedFrame],
    truncated: bool,
}

fn project_frames(resources: Vec<FrameResource>) -> Result<Value, ProviderError> {
    let mut seen = BTreeSet::new();
    let mut frames = Vec::with_capacity(resources.len());
    for resource in resources {
        validate_id(&resource.id)?;
        if !seen.insert(resource.id.clone()) {
            return Err(invalid_response());
        }
        if !resource.attributes.is_object() {
            return Err(invalid_response());
        }
        let attributes = serde_json::from_value::<FrameAttributes>(resource.attributes)
            .map_err(|_| invalid_response())?;
        let selected_name = attributes
            .name
            .filter(|name| !name.is_empty())
            .or_else(|| attributes.label.filter(|label| !label.is_empty()));
        let (name, name_truncated) = match selected_name {
            Some(name) => {
                let (name, truncated) = truncate_name(&name);
                (Some(name), truncated)
            }
            None => (None, false),
        };
        frames.push(ProjectedFrame {
            id: resource.id,
            name,
            name_truncated,
        });
    }

    frames.sort_by(|left, right| left.id.cmp(&right.id));
    let total = frames.len();
    let mut selected = Vec::with_capacity(total.min(MAX_FRAMES));
    for frame in frames.into_iter().take(MAX_FRAMES) {
        selected.push(frame);
        let records_remain = selected.len() < total;
        if serialized_frames_len(&selected, records_remain)? > MAX_PROJECTED_OUTPUT_BYTES {
            selected.pop();
            break;
        }
    }

    let output = serde_json::to_value(FramesOutput {
        truncated: selected.len() < total,
        frames: &selected,
    })
    .map_err(|_| invalid_response())?;
    bounded_output(output)
}

fn serialized_frames_len(
    frames: &[ProjectedFrame],
    truncated: bool,
) -> Result<usize, ProviderError> {
    serde_json::to_vec(&FramesOutput { frames, truncated })
        .map(|encoded| encoded.len())
        .map_err(|_| invalid_response())
}

fn truncate_name(name: &str) -> (String, bool) {
    if name.len() <= MAX_NAME_BYTES {
        return (name.to_owned(), false);
    }
    let budget = MAX_NAME_BYTES - NAME_TRUNCATION_MARKER.len();
    let mut end = budget;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{NAME_TRUNCATION_MARKER}", &name[..end]), true)
}

fn validate_id(id: &str) -> Result<(), ProviderError> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(invalid_response());
    }
    Ok(())
}

fn decode_account(body: &[u8]) -> Result<AccountEnvelope, ProviderError> {
    let value = decode_value(body)?;
    if !value.is_object() || !value.get("data").is_some_and(Value::is_object) {
        return Err(invalid_response());
    }
    serde_json::from_value(value).map_err(|_| invalid_response())
}

fn decode_frames(body: &[u8]) -> Result<FramesEnvelope, ProviderError> {
    let value = decode_value(body)?;
    let Some(data) = value.get("data").and_then(Value::as_array) else {
        return Err(invalid_response());
    };
    if !value.is_object() || data.iter().any(|resource| !resource.is_object()) {
        return Err(invalid_response());
    }
    serde_json::from_value(value).map_err(|_| invalid_response())
}

fn decode_value(body: &[u8]) -> Result<Value, ProviderError> {
    serde_json::from_slice(body).map_err(|_| invalid_response())
}

fn bounded_output(output: Value) -> Result<Value, ProviderError> {
    let length = serde_json::to_vec(&output)
        .map_err(|_| invalid_response())?
        .len();
    if length > MAX_PROJECTED_OUTPUT_BYTES {
        return Err(invalid_response());
    }
    Ok(output)
}

fn status_error(status: u16) -> ProviderError {
    match status {
        401 => ProviderError::new(
            "reauth-required",
            "the broker credential must be replaced or re-enrolled",
        ),
        403 => ProviderError::new("forbidden", "Skylight refused this private API read"),
        404 => ProviderError::new("not-found", "the private API resource was not found"),
        429 => ProviderError::new("rate-limited", "the private API rate limit was reached"),
        _ => ProviderError::new(
            "unexpected-status",
            "the private API returned an unexpected status",
        ),
    }
}

fn unknown_capability() -> ProviderError {
    ProviderError::new(
        "unknown-capability",
        "unsupported Skylight private capability",
    )
}

fn invalid_input() -> ProviderError {
    ProviderError::new("invalid-input", "input must be exactly an empty object")
}

fn invalid_request() -> ProviderError {
    ProviderError::new(
        "invalid-request",
        "could not construct the fixed Skylight request",
    )
}

fn http_failed() -> ProviderError {
    ProviderError::new("http-failed", "broker HTTP request failed")
}

fn invalid_response() -> ProviderError {
    ProviderError::new(
        "invalid-response",
        "the private API returned an invalid response",
    )
}

dekopon_provider_sdk::export_provider_with_bindings!(SkylightPrivate, bindings);

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use dekopon_provider_http::{Header, HttpError, HttpErrorCode, Request, Response};
    use dekopon_provider_sdk::{EffectKind, Idempotency, Provider, RiskLevel};
    use serde_json::{Map, Value, json};

    use super::{
        ACCEPT_JSON, ACCOUNT_CAPABILITY, ACCOUNT_URI, FRAMES_CAPABILITY, FRAMES_URI,
        MAX_COMPONENT_OUTPUT_BYTES, MAX_FRAMES, MAX_ID_BYTES, MAX_NAME_BYTES,
        MAX_PROJECTED_OUTPUT_BYTES, MAX_RESPONSE_BODY_BYTES, NAME_TRUNCATION_MARKER,
        SkylightPrivate, USER_AGENT, invoke_with,
    };

    fn capability(value: &str) -> dekopon_provider_sdk::CapabilityId {
        value.parse().expect("valid capability fixture")
    }

    fn json_response(status: u16, body: Value) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: serde_json::to_vec(&body).expect("mock response serializes"),
        }
    }

    fn raw_response(status: u16, body: &[u8]) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    fn assert_fixed_request(request: &Request, expected_uri: &str) {
        assert_eq!(request.method, "GET");
        assert_eq!(request.uri, expected_uri);
        assert!(request.body.is_empty());
        assert_eq!(
            request.headers,
            vec![
                Header::text("accept", ACCEPT_JSON).expect("fixed accept header"),
                Header::text("user-agent", USER_AGENT).expect("fixed user-agent header"),
            ]
        );
        for denied in ["authorization", "cookie", "content-type"] {
            assert!(
                !request
                    .headers
                    .iter()
                    .any(|header| header.name.eq_ignore_ascii_case(denied)),
                "guest set denied header {denied}"
            );
        }
    }

    fn invoke_json(
        capability_id: &str,
        body: Value,
    ) -> Result<Value, dekopon_provider_sdk::ProviderError> {
        invoke_with(&capability(capability_id), json!({}), |_| {
            Ok(json_response(200, body))
        })
    }

    fn assert_invalid_response(body: Value) {
        let error = match invoke_json(FRAMES_CAPABILITY, body.clone()) {
            Ok(output) => panic!("fixture was unexpectedly accepted: {body}; output: {output}"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "invalid-response");
        assert_eq!(
            error.message(),
            "the private API returned an invalid response"
        );
    }

    #[test]
    fn manifest_is_exactly_the_two_medium_read_capabilities() {
        let manifest = SkylightPrivate::manifest();
        assert_eq!(manifest.id.as_str(), "skylight-private");
        assert!(manifest.command_words.is_empty());
        assert_eq!(manifest.capabilities.len(), 2);
        assert_eq!(
            manifest
                .capabilities
                .iter()
                .map(|capability| capability.id.as_str())
                .collect::<Vec<_>>(),
            vec![ACCOUNT_CAPABILITY, FRAMES_CAPABILITY]
        );
        let empty_schema = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });
        for capability in &manifest.capabilities {
            assert_eq!(capability.effect, EffectKind::ReadOnly);
            assert_eq!(capability.risk, RiskLevel::Medium);
            assert_eq!(capability.idempotency, Idempotency::Idempotent);
            assert_eq!(capability.input_schema, empty_schema);
            assert!(!capability.id.as_str().contains("write"));
            assert!(!capability.id.as_str().contains("request"));
            assert!(!capability.id.as_str().contains("api"));
        }
    }

    #[test]
    fn unknown_non_object_and_extra_field_inputs_never_send() {
        let error = invoke_with(
            &capability("skylight.private.unknown"),
            json!({"endpoint": "sentinel"}),
            |_| unreachable!("unknown capability must not call HTTP"),
        )
        .expect_err("unknown capability fails");
        assert_eq!(error.code(), "unknown-capability");

        for capability_id in [ACCOUNT_CAPABILITY, FRAMES_CAPABILITY] {
            for input in [
                Value::Null,
                json!([]),
                json!(true),
                json!(7),
                json!("not-an-object"),
            ] {
                let error = invoke_with(&capability(capability_id), input, |_| {
                    unreachable!("non-object input must not call HTTP")
                })
                .expect_err("non-object input fails");
                assert_eq!(error.code(), "invalid-input");
            }

            for field in [
                "endpoint",
                "url",
                "URL",
                "path",
                "method",
                "token",
                "accountId",
                "frameId",
                "query",
                "headers",
                "body",
            ] {
                let mut fields = Map::new();
                fields.insert(field.to_owned(), json!("caller-controlled-sentinel"));
                let error = invoke_with(&capability(capability_id), Value::Object(fields), |_| {
                    unreachable!("extra field must not call HTTP")
                })
                .expect_err("extra field fails");
                assert_eq!(error.code(), "invalid-input", "accepted {field}");
            }
        }
    }

    #[test]
    fn account_uses_one_exact_fixed_request_and_projects_only_the_id() {
        let calls = Cell::new(0);
        let output = invoke_with(&capability(ACCOUNT_CAPABILITY), json!({}), |request| {
            calls.set(calls.get() + 1);
            assert_fixed_request(&request, ACCOUNT_URI);
            Ok(json_response(
                200,
                json!({
                    "data": {
                        "id": "account-7",
                        "type": "users",
                        "attributes": {
                            "name": "private-name-sentinel",
                            "email": "email-sentinel@example.invalid",
                            "billing": "billing-sentinel",
                            "subscriptions": ["subscription-sentinel"],
                            "bearerToken": "bearer-token-sentinel",
                            "refreshToken": "refresh-token-sentinel"
                        },
                        "relationships": {"sessions": {"data": "session-sentinel"}},
                        "links": {"self": "absolute-link-sentinel"}
                    },
                    "included": [{"activationCode": "activation-code-sentinel"}]
                }),
            ))
        })
        .expect("valid account response succeeds");

        assert_eq!(calls.get(), 1, "the provider must not retry");
        assert_eq!(output, json!({"account": {"id": "account-7"}}));
        let encoded = output.to_string();
        for sentinel in [
            "private-name-sentinel",
            "email-sentinel",
            "billing-sentinel",
            "subscription-sentinel",
            "bearer-token-sentinel",
            "refresh-token-sentinel",
            "session-sentinel",
            "absolute-link-sentinel",
            "activation-code-sentinel",
        ] {
            assert!(!encoded.contains(sentinel), "output leaked {sentinel}");
        }
    }

    #[test]
    fn account_rejects_missing_empty_non_string_and_oversized_ids() {
        for body in [
            json!({"data": {}}),
            json!({"data": {"id": ""}}),
            json!({"data": {"id": 7}}),
            json!({"data": {"id": "x".repeat(MAX_ID_BYTES + 1)}}),
            json!({"data": []}),
            json!({"data": ["account-id"]}),
            json!([{"data": {"id": "account-id"}}]),
        ] {
            let error = invoke_json(ACCOUNT_CAPABILITY, body).expect_err("identity must fail");
            assert_eq!(error.code(), "invalid-response");
        }
    }

    #[test]
    fn frames_use_one_exact_fixed_request_and_project_sorted_names() {
        let calls = Cell::new(0);
        let output = invoke_with(&capability(FRAMES_CAPABILITY), json!({}), |request| {
            calls.set(calls.get() + 1);
            assert_fixed_request(&request, FRAMES_URI);
            Ok(json_response(
                200,
                json!({
                    "data": [
                        {"id": "frame-c", "attributes": {}},
                        {"id": "frame-a", "attributes": {"name": "Kitchen", "label": "ignored"}},
                        {"id": "frame-b", "attributes": {"name": "", "label": "Family"}}
                    ]
                }),
            ))
        })
        .expect("valid frame list succeeds");

        assert_eq!(calls.get(), 1, "the provider must not retry");
        assert_eq!(
            output,
            json!({
                "frames": [
                    {"id": "frame-a", "name": "Kitchen", "nameTruncated": false},
                    {"id": "frame-b", "name": "Family", "nameTruncated": false},
                    {"id": "frame-c", "nameTruncated": false}
                ],
                "truncated": false
            })
        );
    }

    #[test]
    fn frames_reject_duplicate_and_malformed_identities() {
        for body in [
            json!({"data": [{"id": "same"}, {"id": "same"}]}),
            json!({"data": [{}]}),
            json!({"data": [{"id": ""}]}),
            json!({"data": [{"id": 1}]}),
            json!({"data": [{"id": "x".repeat(MAX_ID_BYTES + 1)}]}),
            json!({"data": [null]}),
            json!({"data": [["frame-id", {}]]}),
        ] {
            assert_invalid_response(body);
        }
    }

    #[test]
    fn frames_reject_invalid_envelopes_and_wrong_known_field_types() {
        for body in [
            json!({}),
            json!({"data": {}}),
            json!({"data": null}),
            json!([[{"id": "1"}]]),
            json!({"data": [{"id": "1", "attributes": null}]}),
            json!({"data": [{"id": "1", "attributes": []}]}),
            json!({"data": [{"id": "1", "attributes": {"name": null}}]}),
            json!({"data": [{"id": "1", "attributes": {"name": 7}}]}),
            json!({"data": [{"id": "1", "attributes": {"label": false}}]}),
        ] {
            assert_invalid_response(body);
        }
    }

    #[test]
    fn an_empty_frame_array_is_a_valid_complete_projection() {
        assert_eq!(
            invoke_json(FRAMES_CAPABILITY, json!({"data": []})).expect("empty list succeeds"),
            json!({"frames": [], "truncated": false})
        );
    }

    #[test]
    fn frame_count_is_capped_after_stable_id_sorting() {
        let data = (0..(MAX_FRAMES + 3))
            .rev()
            .map(|index| json!({"id": format!("frame-{index:02}")}))
            .collect::<Vec<_>>();
        let output = invoke_json(FRAMES_CAPABILITY, json!({"data": data}))
            .expect("bounded frame list succeeds");
        let frames = output["frames"].as_array().expect("frames are an array");
        assert_eq!(frames.len(), MAX_FRAMES);
        assert_eq!(frames[0]["id"], "frame-00");
        assert_eq!(frames[MAX_FRAMES - 1]["id"], "frame-31");
        assert_eq!(output["truncated"], true);
    }

    #[test]
    fn frame_output_budget_omits_whole_records_and_marks_truncation() {
        let data = (0..MAX_FRAMES)
            .map(|index| {
                let mut id = format!("{index:02}");
                id.push_str(&"\0".repeat(MAX_ID_BYTES - id.len()));
                json!({
                    "id": id,
                    "attributes": {"name": "\u{0001}".repeat(MAX_NAME_BYTES)}
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({"data": data})).expect("fixture serializes");
        assert!(
            body.len() <= MAX_RESPONSE_BODY_BYTES,
            "fixture stays response-bounded"
        );
        let output = invoke_with(&capability(FRAMES_CAPABILITY), json!({}), |_| {
            Ok(raw_response(200, &body))
        })
        .expect("output-budget truncation succeeds");
        let frames = output["frames"].as_array().expect("frames are an array");
        assert!(!frames.is_empty());
        assert!(frames.len() < MAX_FRAMES);
        assert_eq!(output["truncated"], true);
        assert!(
            serde_json::to_vec(&output)
                .expect("output serializes")
                .len()
                <= MAX_PROJECTED_OUTPUT_BYTES
        );
        let component = json!({"outcome": "succeeded", "output": output});
        assert!(
            serde_json::to_vec(&component)
                .expect("component response serializes")
                .len()
                < MAX_COMPONENT_OUTPUT_BYTES
        );
    }

    #[test]
    fn frame_names_are_utf8_safe_and_include_the_truncation_marker() {
        let split_boundary = format!("{}😀tail", "a".repeat(252));
        let exact_boundary = "é".repeat(MAX_NAME_BYTES / 2);
        let output = invoke_json(
            FRAMES_CAPABILITY,
            json!({
                "data": [
                    {"id": "1", "attributes": {"name": split_boundary}},
                    {"id": "2", "attributes": {"name": exact_boundary}}
                ]
            }),
        )
        .expect("bounded names succeed");
        let truncated = output["frames"][0]["name"]
            .as_str()
            .expect("name is present");
        assert!(truncated.ends_with(NAME_TRUNCATION_MARKER));
        assert!(truncated.len() <= MAX_NAME_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert_eq!(output["frames"][0]["nameTruncated"], true);
        assert_eq!(output["frames"][1]["name"], exact_boundary);
        assert_eq!(output["frames"][1]["nameTruncated"], false);
    }

    #[test]
    fn private_and_secret_sentinel_fields_never_enter_frame_output() {
        let output = invoke_json(
            FRAMES_CAPABILITY,
            json!({
                "data": [{
                    "id": "frame-safe",
                    "type": "frame",
                    "attributes": {
                        "name": "Visible name",
                        "email": "email-value-sentinel",
                        "bearerToken": "bearer-value-sentinel",
                        "refreshToken": "refresh-value-sentinel",
                        "activationCode": "activation-value-sentinel",
                        "serial": "serial-value-sentinel",
                        "billing": "billing-value-sentinel",
                        "events": ["event-value-sentinel"],
                        "tasks": ["task-value-sentinel"],
                        "mediaUrl": "media-url-value-sentinel"
                    },
                    "links": {"self": "absolute-link-value-sentinel"},
                    "relationships": {"owner": {"data": "relationship-value-sentinel"}}
                }],
                "included": [{"attributes": "included-attributes-value-sentinel"}]
            }),
        )
        .expect("unknown private fields are ignored");
        assert_eq!(
            output,
            json!({
                "frames": [{
                    "id": "frame-safe",
                    "name": "Visible name",
                    "nameTruncated": false
                }],
                "truncated": false
            })
        );
        let sentinels = [
            "email-value-sentinel",
            "bearer-value-sentinel",
            "refresh-value-sentinel",
            "activation-value-sentinel",
            "serial-value-sentinel",
            "billing-value-sentinel",
            "event-value-sentinel",
            "task-value-sentinel",
            "media-url-value-sentinel",
            "absolute-link-value-sentinel",
            "relationship-value-sentinel",
            "included-attributes-value-sentinel",
        ];
        let encoded = output.to_string();
        for sentinel in sentinels {
            assert!(!encoded.contains(sentinel), "output leaked {sentinel}");
        }

        let error = invoke_json(
            FRAMES_CAPABILITY,
            json!({
                "data": [{
                    "id": "",
                    "attributes": {"email": sentinels[0], "bearerToken": sentinels[1]},
                    "relationships": {"owner": sentinels[10]}
                }]
            }),
        )
        .expect_err("malformed identity fails without reflecting its resource");
        let rendered = format!("{} {}", error.code(), error.message());
        for sentinel in sentinels {
            assert!(!rendered.contains(sentinel), "error leaked {sentinel}");
        }
    }

    #[test]
    fn statuses_and_invalid_bodies_map_to_stable_failures_without_retry() {
        for (status, expected) in [
            (401, "reauth-required"),
            (403, "forbidden"),
            (404, "not-found"),
            (429, "rate-limited"),
            (418, "unexpected-status"),
            (500, "unexpected-status"),
        ] {
            let calls = Cell::new(0);
            let error = invoke_with(&capability(ACCOUNT_CAPABILITY), json!({}), |_| {
                calls.set(calls.get() + 1);
                Ok(raw_response(status, b"response-body-secret-sentinel"))
            })
            .expect_err("non-200 must fail");
            assert_eq!(calls.get(), 1, "status {status} was retried");
            assert_eq!(error.code(), expected, "status {status}");
            assert!(!error.message().contains("response-body-secret-sentinel"));
        }

        for body in [b"not json".as_slice(), br#"{"data":null}"#.as_slice()] {
            let error = invoke_with(&capability(ACCOUNT_CAPABILITY), json!({}), |_| {
                Ok(raw_response(200, body))
            })
            .expect_err("invalid response must fail");
            assert_eq!(error.code(), "invalid-response");
        }

        let oversized = vec![b'x'; MAX_RESPONSE_BODY_BYTES + 1];
        let error = invoke_with(&capability(ACCOUNT_CAPABILITY), json!({}), |_| {
            Ok(raw_response(200, &oversized))
        })
        .expect_err("oversized response must fail before decoding");
        assert_eq!(error.code(), "invalid-response");
    }

    #[test]
    fn every_transport_failure_collapses_raw_detail_to_bounded_http_failed() {
        let detail = concat!(
            "raw-host-sentinel raw-path-sentinel raw-header-sentinel ",
            "raw-body-sentinel credential-value-sentinel raw-error-sentinel"
        );
        for code in [
            HttpErrorCode::InvalidMethod,
            HttpErrorCode::InvalidUri,
            HttpErrorCode::InvalidHeader,
            HttpErrorCode::RequestTooLarge,
            HttpErrorCode::Denied,
            HttpErrorCode::HostCallLimit,
            HttpErrorCode::Dns,
            HttpErrorCode::Connect,
            HttpErrorCode::Tls,
            HttpErrorCode::Timeout,
            HttpErrorCode::Protocol,
            HttpErrorCode::ResponseTooLarge,
            HttpErrorCode::Internal,
        ] {
            let calls = Cell::new(0);
            let error = invoke_with(&capability(FRAMES_CAPABILITY), json!({}), |_| {
                calls.set(calls.get() + 1);
                Err(HttpError {
                    code,
                    message: detail.repeat(64),
                })
            })
            .expect_err("transport failure must fail");
            assert_eq!(calls.get(), 1, "transport failure was retried");
            assert_eq!(error.code(), "http-failed");
            assert_eq!(error.message(), "broker HTTP request failed");
            assert!(error.message().len() < 64);
            for sentinel in [
                "raw-host-sentinel",
                "raw-path-sentinel",
                "raw-header-sentinel",
                "raw-body-sentinel",
                "credential-value-sentinel",
                "raw-error-sentinel",
            ] {
                assert!(!error.message().contains(sentinel));
            }
        }
    }
}
