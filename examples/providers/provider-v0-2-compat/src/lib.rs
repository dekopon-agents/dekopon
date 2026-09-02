use dekopon_provider_sdk::{
    CapabilityId, CommandInvocation, EffectKind, Idempotency, Provider, ProviderApiVersion,
    ProviderCapability, ProviderError, ProviderManifest, RiskLevel,
};
use serde_json::{Value, json};

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        pub_export_macro: true,
    });
}

/// Frozen `dekopon:provider@0.2.0` guest: it exports the legacy `resolve-command` and nothing else
/// beyond the base world, so hosts prove the old command export still loads and runs.
struct HistoricalCommandProvider;

const ECHO_CAPABILITY: &str = "provider-v0-2-compat.echo";

impl Provider for HistoricalCommandProvider {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "provider-v0-2-compat".parse().expect("static provider ID"),
            description: "Historical provider-commands ABI compatibility fixture".to_owned(),
            command_words: vec!["compat".to_owned()],
            capabilities: vec![ProviderCapability {
                id: ECHO_CAPABILITY.parse().expect("static capability ID"),
                description: "Returns its bounded object unchanged".to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                input_schema: json!({"type":"object","additionalProperties":true}),
            }],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() != ECHO_CAPABILITY || !input.is_object() {
            return Err(ProviderError::new(
                "invalid-input",
                "historical fixture rejected input",
            ));
        }
        Ok(input)
    }

    fn resolve_command(argv: &[String]) -> Result<CommandInvocation, ProviderError> {
        match argv {
            [subcommand] if subcommand == "echo" => Ok(CommandInvocation {
                capability: ECHO_CAPABILITY.parse().expect("static capability ID"),
                input: json!({}),
            }),
            _ => Err(ProviderError::new(
                "usage",
                "historical fixture accepts exactly `echo`",
            )),
        }
    }
}

dekopon_provider_sdk::export_provider_with_commands!(HistoricalCommandProvider, bindings);

#[cfg(test)]
mod tests {
    use super::{ECHO_CAPABILITY, HistoricalCommandProvider, Provider};
    use serde_json::json;

    #[test]
    fn manifest_declares_the_compat_word_and_the_echo_capability() {
        let manifest = HistoricalCommandProvider::manifest();
        assert_eq!(manifest.id.as_str(), "provider-v0-2-compat");
        assert_eq!(manifest.command_words, ["compat"]);
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(manifest.capabilities[0].id.as_str(), ECHO_CAPABILITY);
    }

    #[test]
    fn echo_argv_resolves_to_the_echo_capability_with_empty_input() {
        let invocation = HistoricalCommandProvider::resolve_command(&["echo".to_owned()])
            .expect("echo resolves");
        assert_eq!(invocation.capability.as_str(), ECHO_CAPABILITY);
        assert_eq!(invocation.input, json!({}));
    }

    #[test]
    fn any_other_argv_is_declined_with_a_usage_error() {
        for argv in [
            Vec::new(),
            vec!["--help".to_owned()],
            vec!["echo".to_owned(), "x".to_owned()],
        ] {
            let error = HistoricalCommandProvider::resolve_command(&argv)
                .expect_err("non-echo argv is declined");
            assert_eq!(error.code(), "usage", "argv {argv:?}");
        }
    }
}
