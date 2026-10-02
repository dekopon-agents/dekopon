use std::{collections::BTreeSet, path::Path};

use dekopon_broker_host::{BrokerHostLimits, CommandRunOutcome};
use dekopon_provider_sdk::{
    ProviderManifest,
    provider::{self, Capabilities, ImportSet, Provider},
};
use serde_json::Value;
use wasmparser::{Parser, Payload};

/// A mismatch between a typed declaration and its checked component.
#[derive(Debug, thiserror::Error)]
pub enum ConformanceError {
    #[error("fixture I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("component decode: {0}")]
    Decode(#[from] wasmparser::BinaryReaderError),
    #[error("component host: {0}")]
    Host(#[from] dekopon_broker_host::BrokerHostError),
    #[error("native manifest: {0}")]
    Manifest(#[from] provider::ManifestError),
    #[error("manifest capability IDs differ: declared {declared:?}, component {component:?}")]
    CapabilityIds {
        declared: BTreeSet<String>,
        component: BTreeSet<String>,
    },
    #[error("help or usage differs for command word {word}")]
    HelpUsage { word: String },
    #[error("decoded imports differ: declared {declared:?}, component {component:?}")]
    Imports {
        declared: ImportSet,
        component: BTreeSet<String>,
    },
    #[error("input object schema is open for capability {capability}")]
    OpenSchema { capability: String },
}

fn ids(manifest: &ProviderManifest) -> BTreeSet<String> {
    manifest
        .capabilities
        .iter()
        .map(|c| c.id.to_string())
        .collect()
}

fn closed(schema: &Value) -> bool {
    match schema {
        Value::Object(map) => {
            let object = match map.get("type") {
                Some(Value::String(kind)) => kind == "object",
                Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
                _ => false,
            };
            if object && map.get("additionalProperties") != Some(&Value::Bool(false)) {
                return false;
            }
            map.iter().all(|(key, value)| match key.as_str() {
                "properties" | "patternProperties" | "dependentSchemas" | "$defs" => value
                    .as_object()
                    .is_some_and(|properties| properties.values().all(closed)),
                "items" | "additionalProperties" | "not" | "if" | "then" | "else" => closed(value),
                "prefixItems" | "anyOf" | "oneOf" | "allOf" => value
                    .as_array()
                    .is_some_and(|items| items.iter().all(closed)),
                _ => true,
            })
        }
        Value::Bool(_) => true,
        Value::Null | Value::Number(_) | Value::String(_) | Value::Array(_) => false,
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "external wasmparser payload variants may grow"
)]
fn decoded_imports(bytes: &[u8]) -> Result<BTreeSet<String>, ConformanceError> {
    let mut depth = 0;
    let mut imports = BTreeSet::new();
    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) if depth > 0 => depth -= 1,
            Payload::ComponentImportSection(section) if depth == 0 => {
                for import in section {
                    imports.insert(import?.name.name.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(imports)
}

fn declared_imports(set: ImportSet) -> BTreeSet<String> {
    [
        (ImportSet::HTTP, "dekopon:http/client@1.1.0"),
        (ImportSet::CLOCK, "dekopon:clock/wall@1.0.0"),
        (ImportSet::SETTINGS, "dekopon:settings/config@0.1.0"),
        (ImportSet::JSONL, "dekopon:storage/jsonl@0.1.1"),
        (
            ImportSet::DURABLE_FILES,
            "dekopon:storage/durable-files@0.1.1",
        ),
        (ImportSet::ASSETS, "dekopon:asset/asset@0.1.0"),
    ]
    .into_iter()
    .filter_map(|(bit, name)| set.contains(bit).then_some(name.to_owned()))
    .collect()
}

fn rendered(outcome: &CommandRunOutcome, help: bool) -> bool {
    match outcome {
        CommandRunOutcome::Rendered {
            stdout,
            stderr,
            status,
        } => {
            if help {
                *status == 0
                    && !stdout.is_empty()
                    && stderr.is_empty()
                    && !stdout.contains('\u{1b}')
            } else {
                *status != 0
                    && stdout.is_empty()
                    && !stderr.is_empty()
                    && !stderr.contains('\u{1b}')
            }
        }
        CommandRunOutcome::Proposed { .. } | CommandRunOutcome::Failed { .. } => false,
    }
}

/// Checks the four declaration/real-component contract items for `P`.
///
/// # Errors
/// Returns the first mismatch, decode, manifest or host failure.
pub fn conformance<P: Provider>(component: impl AsRef<Path>) -> Result<(), ConformanceError> {
    let path = component.as_ref();
    let bytes = std::fs::read(path)?;
    let imports = decoded_imports(&bytes)?;
    let declared = <P::Capabilities as Capabilities<P>>::IMPORTS;
    if imports != declared_imports(declared) {
        return Err(ConformanceError::Imports {
            declared,
            component: imports,
        });
    }
    let native = provider::manifest::<P>()?;
    let registry =
        super::typed::cached_registry::<P>(path.canonicalize()?, BrokerHostLimits::default())?;
    let real = registry
        .manifests()
        .next()
        .ok_or(ConformanceError::HelpUsage {
            word: String::new(),
        })?;
    if ids(&native) != ids(real)
        || native.capabilities.len() != real.capabilities.len()
        || ids(&native).len() != native.capabilities.len()
    {
        return Err(ConformanceError::CapabilityIds {
            declared: ids(&native),
            component: ids(real),
        });
    }
    for capability in &real.capabilities {
        if !closed(&capability.input_schema) {
            return Err(ConformanceError::OpenSchema {
                capability: capability.id.to_string(),
            });
        }
    }
    for &word in P::COMMAND_WORDS {
        if !real.command_words.iter().any(|declared| declared == word) {
            return Err(ConformanceError::HelpUsage {
                word: word.to_owned(),
            });
        }
    }
    for word in &real.command_words {
        for (argv, help) in [
            (&["--help".to_owned()][..], true),
            (&["--definitely-invalid-option".to_owned()][..], false),
        ] {
            let native_outcome = provider::command::<P>(argv, None);
            let real_outcome =
                super::typed::runtime().block_on(registry.run_command(word, argv, false))?;
            if !rendered(&native_outcome, help) || !rendered(&real_outcome, help) {
                return Err(ConformanceError::HelpUsage {
                    word: word.to_owned(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_object_in_a_schema_branch_must_be_closed() {
        assert!(!closed(
            &json!({"type":"object","additionalProperties":false,"properties":{"child":{"anyOf":[{"type":"null"},{"type":"object"}]}}})
        ));
        assert!(closed(
            &json!({"type":"object","additionalProperties":false,"properties":{"child":{"type":"object","additionalProperties":false}}})
        ));
    }

    #[test]
    fn dependent_and_definition_object_schemas_must_be_closed() {
        for keyword in ["dependentSchemas", "$defs"] {
            let mut schema = json!({"type":"object","additionalProperties":false});
            schema[keyword] = json!({"nested":{"type":"object"}});
            assert!(!closed(&schema), "open nested object under {keyword}");
        }
    }

    #[test]
    fn checked_cli_bytes_have_no_imports_but_a_declared_clock_would_mismatch() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/providers/cli-probe-provider.wasm");
        let bytes = std::fs::read(path).unwrap();
        let decoded = decoded_imports(&bytes).unwrap();
        assert!(
            decoded.is_empty(),
            "decoded imports: {decoded:?}; bytes: {}",
            bytes.len()
        );
        assert_ne!(decoded, declared_imports(ImportSet::CLOCK));
    }
}
