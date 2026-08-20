use dekopon_provider_sdk::{
    CapabilityId, EffectKind, Idempotency, Provider, ProviderApiVersion, ProviderCapability,
    ProviderError, ProviderManifest, RiskLevel,
};
use serde_json::{Value, json};

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        pub_export_macro: true,
    });
}

struct HistoricalProvider;

impl Provider for HistoricalProvider {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "provider-v0-1-compat".parse().expect("static provider ID"),
            description: "Historical provider ABI compatibility fixture".to_owned(),
            command_words: Vec::new(),
            capabilities: vec![ProviderCapability {
                id: "provider-v0-1-compat.echo"
                    .parse()
                    .expect("static capability ID"),
                description: "Returns its bounded object unchanged".to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                input_schema: json!({"type":"object","additionalProperties":true}),
            }],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() != "provider-v0-1-compat.echo" || !input.is_object() {
            return Err(ProviderError::new(
                "invalid-input",
                "historical fixture rejected input",
            ));
        }
        Ok(input)
    }
}

dekopon_provider_sdk::export_provider_with_bindings!(HistoricalProvider, bindings);
