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
            description: "Echoes structured input and transforms messages".to_owned(),
            command_words: Vec::new(),
            capabilities: vec![
                ProviderCapability {
                    id: "echo.echo".parse().expect("static capability ID is valid"),
                    description: "Returns the supplied JSON object unchanged".to_owned(),
                    effect: EffectKind::ReadOnly,
                    risk: RiskLevel::Low,
                    idempotency: Idempotency::Idempotent,
                    input_schema: json!({
                        "type": "object",
                        "additionalProperties": true
                    }),
                },
                message_capability(
                    "echo.reverse",
                    "Reverses the Unicode scalar values in a message",
                ),
                message_capability("echo.upcase", "Converts a message to Unicode uppercase"),
                message_capability("echo.downcase", "Converts a message to Unicode lowercase"),
                message_capability(
                    "echo.ransom-case",
                    "Alternates message letters between lowercase and uppercase",
                ),
            ],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        match capability.as_str() {
            "echo.echo" => Ok(input),
            "echo.reverse" => transform_message(input, |message| message.chars().rev().collect()),
            "echo.upcase" => transform_message(input, str::to_uppercase),
            "echo.downcase" => transform_message(input, str::to_lowercase),
            "echo.ransom-case" => transform_message(input, ransom_case),
            _ => Err(ProviderError::new(
                "unsupported-capability",
                format!("echo does not implement {capability}"),
            )),
        }
    }
}

fn message_capability(id: &str, description: &str) -> ProviderCapability {
    ProviderCapability {
        id: id.parse().expect("static capability ID is valid"),
        description: description.to_owned(),
        effect: EffectKind::ReadOnly,
        risk: RiskLevel::Low,
        idempotency: Idempotency::Idempotent,
        input_schema: json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            },
            "required": ["message"],
            "additionalProperties": false
        }),
    }
}

fn transform_message(
    input: Value,
    transform: impl FnOnce(&str) -> String,
) -> Result<Value, ProviderError> {
    let object = input.as_object().ok_or_else(invalid_message_input)?;
    if object.len() != 1 {
        return Err(invalid_message_input());
    }
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(invalid_message_input)?;

    Ok(json!({"message": transform(message)}))
}

fn invalid_message_input() -> ProviderError {
    ProviderError::new(
        "invalid-input",
        "input must contain exactly one string field named \"message\"",
    )
}

fn ransom_case(message: &str) -> String {
    let mut uppercase = false;
    let mut output = String::with_capacity(message.len());
    for character in message.chars() {
        if character.is_alphabetic() {
            if uppercase {
                output.extend(character.to_uppercase());
            } else {
                output.extend(character.to_lowercase());
            }
            uppercase = !uppercase;
        } else {
            output.push(character);
        }
    }
    output
}

export_provider!(EchoProvider);

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke(capability: &str, message: &str) -> Value {
        EchoProvider::invoke(
            &capability.parse().expect("valid capability fixture"),
            json!({"message": message}),
        )
        .expect("transformation succeeds")
    }

    #[test]
    fn manifest_describes_all_echo_capabilities() {
        let ids = EchoProvider::manifest()
            .capabilities
            .into_iter()
            .map(|capability| capability.id.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            [
                "echo.echo",
                "echo.reverse",
                "echo.upcase",
                "echo.downcase",
                "echo.ransom-case",
            ]
        );
    }

    #[test]
    fn transforms_messages() {
        assert_eq!(
            invoke("echo.reverse", "Hello 🦀"),
            json!({"message": "🦀 olleH"})
        );
        assert_eq!(
            invoke("echo.upcase", "Hello, Straße!"),
            json!({"message": "HELLO, STRASSE!"})
        );
        assert_eq!(
            invoke("echo.downcase", "Hello, WORLD!"),
            json!({"message": "hello, world!"})
        );
        assert_eq!(
            invoke("echo.ransom-case", "Hello, World!"),
            json!({"message": "hElLo, WoRlD!"})
        );
    }

    #[test]
    fn rejects_malformed_transformation_inputs() {
        let capability = "echo.upcase".parse().expect("valid capability fixture");
        for input in [
            json!({}),
            json!({"message": 42}),
            json!({"message": "hello", "extra": true}),
        ] {
            let error = EchoProvider::invoke(&capability, input)
                .expect_err("malformed transformation input must fail");
            assert_eq!(error.code(), "invalid-input");
        }
    }

    #[test]
    fn plain_echo_still_returns_arbitrary_objects_unchanged() {
        let capability = "echo.echo".parse().expect("valid capability fixture");
        let input = json!({"nested": {"answer": 42}});

        assert_eq!(
            EchoProvider::invoke(&capability, input.clone()).expect("echo succeeds"),
            input
        );
    }
}
