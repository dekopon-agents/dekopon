use dekopon_provider_sdk::{
    CapabilityId, EffectKind, Idempotency, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel, export_provider,
};
use serde_json::{Value, json};

struct EchoProvider;

impl Provider for EchoProvider {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "echo".parse().expect("static provider ID is valid"),
            description: "Echoes structured JSON input".to_owned(),
            capabilities: vec![ProviderCapability {
                id: "echo.echo"
                    .parse()
                    .expect("static capability ID is valid"),
                description: "Returns the supplied JSON object unchanged".to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": true
                }),
            }],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() != "echo.echo" {
            return Err(ProviderError::new(
                "unsupported-capability",
                format!("echo does not implement {capability}"),
            ));
        }
        Ok(input)
    }
}

export_provider!(EchoProvider);
