use dekopon_provider_http::{Request, method};
use dekopon_provider_sdk::{
    CapabilityId, EffectKind, Idempotency, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel,
};
use serde_json::{Value, json};

const PROBE_URI: &str = "https://example.invalid/";

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
                description: "Fetches a fixed non-routable documentation endpoint".to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            }],
        }
    }

    fn invoke(capability: &CapabilityId, _input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() != "http-probe.fetch" {
            return Err(ProviderError::new(
                "unknown-capability",
                format!("unsupported capability {capability}"),
            ));
        }

        let request = Request::new(method::GET, PROBE_URI)
            .map_err(|error| ProviderError::new("invalid-request", error.to_string()))?;
        let response = dekopon_provider_http::send(request)
            .map_err(|error| ProviderError::new("http-failed", error.to_string()))?;
        Ok(json!({
            "status": response.status,
            "bodyBytes": response.body.len()
        }))
    }
}

dekopon_provider_sdk::export_provider_with_bindings!(HttpProbe, bindings);

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::Provider;

    use super::{HttpProbe, PROBE_URI};

    #[test]
    fn manifest_declares_one_read_only_probe() {
        let manifest = HttpProbe::manifest();
        assert_eq!(manifest.id.as_str(), "http-probe");
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(PROBE_URI, "https://example.invalid/");
    }
}
