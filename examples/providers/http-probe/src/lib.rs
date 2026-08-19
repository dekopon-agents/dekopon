use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_provider_http::{Header, Request, method};
use dekopon_provider_sdk::{
    CapabilityId, EffectKind, Idempotency, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel,
};
use serde_json::{Map, Value, json};

/// Maximum response body this provider returns to its caller.
///
/// The broker host already bounds a provider's total serialized output, so an unbounded body field
/// would simply fail the whole invocation on a large response instead of returning the useful
/// prefix. Bounding it here keeps a big response readable and keeps base64 expansion (4 bytes out
/// per 3 bytes in) comfortably inside that ceiling.
const MAX_RETURNED_BODY_BYTES: usize = 64 * 1024;

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct HttpProbe;

impl Provider for HttpProbe {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "http-probe".parse().expect("static provider ID is valid"),
            description: "Exercises the versioned broker HTTP import".to_owned(),
            command_words: Vec::new(),
            capabilities: vec![ProviderCapability {
                id: "http-probe.fetch"
                    .parse()
                    .expect("static capability ID is valid"),
                description: "Fetches one broker-authorized URI".to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "uri": {"type": "string"},
                        "method": {"type": "string"},
                        "headers": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "value": {"type": "string"}
                                },
                                "required": ["name", "value"],
                                "additionalProperties": false
                            }
                        },
                        "body": {"type": "string"},
                        "catchError": {"type": "boolean"}
                    },
                    "required": ["uri"],
                    "additionalProperties": false
                }),
            },
            ProviderCapability {
                id: "http-probe.conditional-write"
                    .parse()
                    .expect("static capability ID is valid"),
                description:
                    "Reads a resource, then writes only if its observed etag still matches"
                        .to_owned(),
                effect: EffectKind::ExternalWrite,
                risk: RiskLevel::High,
                idempotency: Idempotency::Conditional,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "uri": {"type": "string"},
                        "expectedEtag": {"type": "string"}
                    },
                    "required": ["uri"],
                    "additionalProperties": false
                }),
            },
            ProviderCapability {
                id: "http-probe.purge"
                    .parse()
                    .expect("static capability ID is valid"),
                description: "Deletes one broker-authorized resource".to_owned(),
                effect: EffectKind::ExternalWrite,
                risk: RiskLevel::High,
                idempotency: Idempotency::Idempotent,
                input_schema: json!({
                    "type": "object",
                    "properties": {"uri": {"type": "string"}},
                    "required": ["uri"],
                    "additionalProperties": false
                }),
            }],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() == "http-probe.conditional-write" {
            return conditional_write(&input);
        }
        if capability.as_str() == "http-probe.purge" {
            return purge(&input);
        }
        if capability.as_str() != "http-probe.fetch" {
            return Err(ProviderError::new(
                "unknown-capability",
                format!("unsupported capability {capability}"),
            ));
        }

        let uri = input
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::new("invalid-input", "uri must be a string"))?;
        let selected_method = input
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or(method::GET);
        let mut request = Request::new(selected_method, uri)
            .map_err(|error| ProviderError::new("invalid-request", error.to_string()))?;
        if let Some(headers) = input.get("headers") {
            let headers = headers
                .as_array()
                .ok_or_else(|| ProviderError::new("invalid-input", "headers must be an array"))?;
            for header in headers {
                let name = header.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::new("invalid-input", "header name must be a string")
                })?;
                let value = header.get("value").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::new("invalid-input", "header value must be a string")
                })?;
                request =
                    request.with_header(Header::text(name, value).map_err(|error| {
                        ProviderError::new("invalid-request", error.to_string())
                    })?);
            }
        }
        if let Some(body) = input.get("body") {
            let body = body
                .as_str()
                .ok_or_else(|| ProviderError::new("invalid-input", "body must be a string"))?;
            request = request.with_body(body.as_bytes().to_vec());
        }
        let catch_error = input
            .get("catchError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match dekopon_provider_http::send(request) {
            Ok(response) => Ok(describe_response(
                response.status,
                &response.body,
                response.headers.len(),
            )),
            Err(error) if catch_error => Ok(json!({
                "caughtError": format!("{:?}", error.code)
            })),
            Err(error) => Err(ProviderError::new("http-failed", error.to_string())),
        }
    }
}

/// Builds the probe's response summary, including a bounded copy of the body.
///
/// `body` is always base64, so any byte sequence round-trips. `bodyText` is added only when the
/// returned bytes are valid UTF-8; an invalid encoding omits the field rather than failing the
/// invocation, because a caller asking for a probe still wants the status and the raw bytes.
fn describe_response(status: u16, body: &[u8], header_count: usize) -> Value {
    let returned = bounded_prefix(body);
    let mut fields = Map::new();
    fields.insert("status".to_owned(), json!(status));
    fields.insert("bodyBytes".to_owned(), json!(body.len()));
    fields.insert("headerCount".to_owned(), json!(header_count));
    fields.insert("body".to_owned(), json!(STANDARD.encode(returned)));
    fields.insert(
        "bodyTruncated".to_owned(),
        json!(returned.len() < body.len()),
    );
    if let Ok(text) = core::str::from_utf8(returned) {
        fields.insert("bodyText".to_owned(), json!(text));
    }
    Value::Object(fields)
}

/// Returns the returnable prefix of a body, never cutting a character in half.
///
/// Slicing at a raw byte offset made `bodyText` vanish from bodies that were perfectly valid
/// UTF-8, purely because the 64 KiB mark landed mid-character — roughly three times in four for a
/// multibyte character straddling the boundary. The consumer path is `jq -r .bodyText`, so the
/// script saw a bare `null` and could not tell "this body was binary" from "I cut it badly".
fn bounded_prefix(body: &[u8]) -> &[u8] {
    if body.len() <= MAX_RETURNED_BODY_BYTES {
        return body;
    }
    let candidate = &body[..MAX_RETURNED_BODY_BYTES];
    match core::str::from_utf8(candidate) {
        Ok(_) => candidate,
        // An error with no length is an *incomplete* trailing sequence, meaning the cut split a
        // character; backing up to the last complete one keeps the body readable as text.
        // A genuinely invalid byte keeps the full prefix and omits `bodyText`, as before.
        Err(error) if error.error_len().is_none() => &candidate[..error.valid_up_to()],
        Err(_) => candidate,
    }
}

dekopon_provider_sdk::export_provider_with_bindings!(HttpProbe, bindings);

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::Provider;
    use serde_json::json;

    use super::{HttpProbe, MAX_RETURNED_BODY_BYTES, describe_response};

    #[test]
    fn manifest_declares_one_read_only_probe() {
        let manifest = HttpProbe::manifest();
        assert_eq!(manifest.id.as_str(), "http-probe");
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(
            manifest.capabilities[0].input_schema["required"],
            json!(["uri"])
        );
    }

    #[test]
    fn utf8_bodies_are_returned_as_both_base64_and_text() {
        let described = describe_response(200, b"hello probe", 4);
        assert_eq!(described["status"], json!(200));
        assert_eq!(described["bodyBytes"], json!(11));
        assert_eq!(described["headerCount"], json!(4));
        assert_eq!(described["body"], json!("aGVsbG8gcHJvYmU="));
        assert_eq!(described["bodyText"], json!("hello probe"));
        assert_eq!(described["bodyTruncated"], json!(false));
    }

    #[test]
    fn invalid_utf8_omits_body_text_without_failing() {
        let described = describe_response(200, &[0xff, 0xfe], 1);
        assert_eq!(described["body"], json!("//4="));
        assert!(described.get("bodyText").is_none());
        assert_eq!(described["bodyBytes"], json!(2));
    }

    #[test]
    fn oversized_bodies_are_truncated_and_flagged() {
        let body = vec![b'x'; MAX_RETURNED_BODY_BYTES + 100];
        let described = describe_response(200, &body, 0);
        assert_eq!(described["bodyBytes"], json!(body.len()));
        assert_eq!(described["bodyTruncated"], json!(true));
        assert_eq!(
            described["bodyText"].as_str().map(str::len),
            Some(MAX_RETURNED_BODY_BYTES)
        );
    }

    #[test]
    fn truncation_never_cuts_a_character_in_half() {
        // An all-ASCII body can never exercise this: the cut has to land inside a multibyte
        // character, which is where a valid UTF-8 body used to lose `bodyText` entirely.
        let mut body = vec![b'x'; MAX_RETURNED_BODY_BYTES - 1];
        body.extend_from_slice("€tail".as_bytes());
        assert!(
            core::str::from_utf8(&body).is_ok(),
            "the body is valid UTF-8"
        );

        let described = describe_response(200, &body, 0);
        assert_eq!(described["bodyTruncated"], json!(true));
        let text = described["bodyText"]
            .as_str()
            .expect("a valid UTF-8 body keeps its text");
        assert_eq!(text.len(), MAX_RETURNED_BODY_BYTES - 1);
        assert!(text.ends_with('x'), "the partial character was dropped");
    }

    #[test]
    fn a_body_that_is_binary_rather_than_badly_cut_still_omits_its_text() {
        let mut body = vec![0xff_u8; MAX_RETURNED_BODY_BYTES];
        body.extend_from_slice(b"tail");
        let described = describe_response(200, &body, 0);
        assert!(described.get("bodyText").is_none());
        assert_eq!(described["bodyTruncated"], json!(true));
    }

    #[test]
    fn an_empty_body_still_reports_every_field() {
        let described = describe_response(204, b"", 0);
        assert_eq!(described["body"], json!(""));
        assert_eq!(described["bodyText"], json!(""));
        assert_eq!(described["bodyBytes"], json!(0));
    }
}

/// Reads a resource and writes only if what it observed is still current.
///
/// The pre-read is the point. It exists so the broker host has an in-tree capability that makes
/// *two* authorized calls in one invocation and refuses between them, which is what exercises
/// `maxRequests`, per-call evidence, and the host-call limit. Until the GitHub provider moved to
/// its own repository, `gh.pull-request.approve` was the only capability shaped like this, and host
/// coverage of that shape should not depend on a provider that is no longer in this tree.
fn conditional_write(input: &Value) -> Result<Value, ProviderError> {
    let uri = input
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::new("invalid-input", "uri is required"))?;

    let read = dekopon_provider_http::send(Request {
        method: method::GET.to_owned(),
        uri: uri.to_owned(),
        headers: Vec::new(),
        body: Vec::new(),
    })
    .map_err(|error| ProviderError::new("http", error.message))?;

    let observed = read
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("etag"))
        .map(|header| String::from_utf8_lossy(&header.value).into_owned())
        .unwrap_or_default();

    // Refusing here is what makes the capability `conditional` rather than idempotent, and it must
    // happen before the write rather than being reported after it.
    if let Some(expected) = input.get("expectedEtag").and_then(Value::as_str)
        && expected != observed
    {
        return Err(ProviderError::new(
            "precondition-failed",
            format!("resource moved: expected {expected:?}, observed {observed:?}"),
        ));
    }

    let write = dekopon_provider_http::send(Request {
        method: method::POST.to_owned(),
        uri: uri.to_owned(),
        headers: vec![Header {
            name: "if-match".to_owned(),
            value: observed.clone().into_bytes(),
        }],
        body: b"{}".to_vec(),
    })
    .map_err(|error| ProviderError::new("http", error.message))?;

    Ok(json!({
        "observedEtag": observed,
        "readStatus": read.status,
        "writeStatus": write.status,
    }))
}

/// Deletes one resource.
///
/// Exists so the manifest exposes more than any one deployment grants, which is the realistic
/// shape: `examples/conditional-write/` deliberately leaves this out of both its policy and its
/// constraint sets, and the example tests assert that an ungranted capability is refused twice
/// over — by Cedar, and by the missing constraint set before Cedar is consulted.
fn purge(input: &Value) -> Result<Value, ProviderError> {
    let uri = input
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::new("invalid-input", "uri is required"))?;
    let response = dekopon_provider_http::send(Request {
        method: "DELETE".to_owned(),
        uri: uri.to_owned(),
        headers: Vec::new(),
        body: Vec::new(),
    })
    .map_err(|error| ProviderError::new("http", error.message))?;
    Ok(json!({"status": response.status}))
}
