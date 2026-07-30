use dekopon_provider_http::{Header, Request, method};
use dekopon_provider_sdk::{
    CapabilityId, EffectKind, Idempotency, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel,
};
use serde_json::{Value, json};

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
            Ok(response) => Ok(json!({
                "status": response.status,
                "bodyBytes": response.body.len(),
                "headerCount": response.headers.len()
            })),
            Err(error) if catch_error => Ok(json!({
                "caughtError": format!("{:?}", error.code)
            })),
            Err(error) => Err(ProviderError::new("http-failed", error.to_string())),
        }
    }
}

dekopon_provider_sdk::export_provider_with_bindings!(HttpProbe, bindings);

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::Provider;

    use super::HttpProbe;

    #[test]
    fn manifest_declares_one_read_only_probe() {
        let manifest = HttpProbe::manifest();
        assert_eq!(manifest.id.as_str(), "http-probe");
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(
            manifest.capabilities[0].input_schema["required"],
            serde_json::json!(["uri"])
        );
    }
}
