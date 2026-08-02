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
            }],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
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
    let returned = &body[..body.len().min(MAX_RETURNED_BODY_BYTES)];
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
    fn an_empty_body_still_reports_every_field() {
        let described = describe_response(204, b"", 0);
        assert_eq!(described["body"], json!(""));
        assert_eq!(described["bodyText"], json!(""));
        assert_eq!(described["bodyBytes"], json!(0));
    }
}
