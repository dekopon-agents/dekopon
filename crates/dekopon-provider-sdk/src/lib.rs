#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
pub use dekopon_capability::EffectKind;
pub use dekopon_core::{
    CapabilityId, IdentifierError, ProviderId, RiskLevel, SecretDrn, SecretUseProposal,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use clap;
pub use schemars;

pub mod asset;
#[cfg(target_arch = "wasm32")]
mod clock;
mod http;
pub mod provider;
mod storage;

pub use provider::{
    Capability, Code, Failure, Needs, Proposal, Provider, SdkFailure, Stdin, Stdout, Usage,
    command, manifest, stdin,
};

#[doc(hidden)]
pub mod export_bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider-cli",
        pub_export_macro: true,
        generate_all,
    });
}

#[macro_export]
macro_rules! export {
    ($provider:ty) => {
        struct __DekoponTypedProviderComponent;

        impl $crate::export_bindings::Guest for __DekoponTypedProviderComponent {
            fn describe() -> ::std::string::String {
                $crate::__typed_describe::<$provider>()
            }

            fn invoke(capability: ::std::string::String, input_json: ::std::string::String) -> ::std::result::Result<(), u8> {
                $crate::__typed_invoke::<$provider>(&capability, &input_json)
            }

            fn run_command(argv: ::std::vec::Vec<::std::string::String>, stdin_piped: bool) -> ::std::string::String {
                $crate::__typed_run_command::<$provider>(&argv, stdin_piped)
            }
        }

        $crate::export_bindings::export!(__DekoponTypedProviderComponent with_types_in $crate::export_bindings);
    };
}

#[doc(hidden)]
pub fn __typed_describe<P: provider::Provider>() -> String {
    match provider::manifest::<P>() {
        Ok(manifest) => {
            serde_json::to_string(&manifest).unwrap_or_else(|error| describe_fallback(&error))
        }
        Err(error) => serde_json::json!({
            "apiVersion": "dekopon.dev/provider/v1alpha1",
            "id": "invalid",
            "description": format!("manifest derivation failed: {error}"),
            "capabilities": []
        })
        .to_string(),
    }
}

#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
pub fn __typed_invoke<P: provider::Provider>(capability: &str, input: &str) -> Result<(), u8> {
    provider::invoke::<P>(capability, input).map_err(std::num::NonZeroU8::get)
}

#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
pub fn __typed_invoke<P: provider::Provider>(capability: &str, input: &str) -> Result<(), u8> {
    let stdio = provider::NativeStdio {
        stdin: None,
        stdout: Box::new(std::io::stdout()),
    };
    match provider::invoke_native::<P>(capability, input, stdio) {
        provider::NativeExit { status: 0, .. } => Ok(()),
        provider::NativeExit { status, .. } => Err(status),
    }
}

#[doc(hidden)]
pub fn __typed_run_command<P: provider::Provider>(argv: &[String], stdin_piped: bool) -> String {
    serde_json::to_string(&provider::command::<P>(argv, stdin_piped))
        .unwrap_or_else(|_| RUN_SERIALIZATION_FALLBACK.to_owned())
}

pub const PROVIDER_WIT: &str = include_str!("../wit/provider.wit");

pub const ASSET_WIT: &str = include_str!("../wit/deps/asset.wit");

/// Version of the component manifest returned by a provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProviderApiVersion {
    /// Initial experimental provider contract.
    #[serde(rename = "dekopon.dev/provider/v1alpha1")]
    V1Alpha1,
}

/// Manifest returned by a provider component.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderManifest {
    /// Provider manifest API version.
    pub api_version: ProviderApiVersion,
    /// Stable provider identity.
    pub id: ProviderId,
    /// Concise operator-facing description.
    pub description: String,
    /// Capabilities implemented by this component.
    pub capabilities: Vec<ProviderCapability>,
    /// The only way a model reaches this provider's capabilities; defaulted so an omitted list
    /// still decodes and is named in the startup conflict report rather than failing to decode.
    #[serde(default)]
    pub command_words: Vec<String>,
}

/// One prompt-visible capability exported by a provider component.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderCapability {
    /// Stable capability identity.
    pub id: CapabilityId,
    /// Concise model- and operator-facing description.
    pub description: String,
    /// Effect classification. Immediate mode accepts only [`EffectKind::ReadOnly`].
    pub effect: EffectKind,
    /// Coarse risk classification available to callers.
    pub risk: RiskLevel,
    /// Object-shaped JSON Schema supplied to a model as function parameters.
    pub input_schema: Value,
}

/// A provider's refusal of an argv, carried by [`CommandRunOutcome::Failed`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ComponentFailure {
    /// Stable provider-specific code.
    pub code: String,
    /// Bounded human-readable detail.
    pub message: String,
}

/// JSON result of a command-word run, returned across the `run-command` WIT boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "camelCase", deny_unknown_fields)]
pub enum CommandRunOutcome {
    /// The argv maps to one capability proposal.
    Proposed {
        /// Capability the command word named.
        capability: CapabilityId,
        /// Input object assembled from the arguments.
        input: Value,
        /// Secret use the proposal names, under the wire key `secretUse`; absent when it names
        /// none, so a proposal without one keeps its earlier shape.
        #[serde(rename = "secretUse", default, skip_serializing_if = "Option::is_none")]
        secret_use: Option<SecretUseProposal>,
    },
    /// The guest answered by itself, as a command-line program prints and exits.
    Rendered {
        /// Text for the shell's standard output.
        stdout: String,
        /// Text for the shell's standard error.
        stderr: String,
        /// Exit status the shell reports for the word.
        status: u8,
    },
    /// The provider declined this argv.
    Failed {
        /// Stable failure detail, reported to the model as a usage error.
        error: ComponentFailure,
    },
}

const RUN_SERIALIZATION_FALLBACK: &str = r#"{"outcome":"failed","error":{"code":"serialization-failed","message":"command run could not be serialized"}}"#;

/// The description carries the serialization error instead of being empty, so the refusal is
/// traceable rather than sending an operator hunting for a blank description nowhere in their
/// source.
fn describe_fallback(error: &serde_json::Error) -> String {
    serde_json::json!({
        "apiVersion": "dekopon.dev/provider/v1alpha1",
        "id": "invalid",
        "description": format!("manifest serialization failed: {error}"),
        "capabilities": [],
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        CommandRunOutcome, ComponentFailure, ProviderCapability, ProviderManifest,
        RUN_SERIALIZATION_FALLBACK, SecretUseProposal, describe_fallback,
    };
    use serde_json::json;
    const UPPER: &str = "cli-probe.upper";
    const DRN: &str = "drn:com.example:secret:prod:api/token";
    fn upper() -> super::CapabilityId {
        UPPER.parse().expect("valid capability fixture")
    }
    #[test]
    fn a_manifest_carrying_any_other_unknown_field_is_still_refused() {
        for unknown in ["idempotency", "retries", "idempotencyKey"] {
            let error = serde_json::from_value::<ProviderCapability>(json!({
                "id": UPPER,
                "description": "Upper-cases text",
                "effect": "read-only",
                "risk": "Low",
                unknown: "whatever",
                "inputSchema": {"type": "object"},
            }))
            .expect_err("an unknown manifest field must be refused");

            assert!(
                error.to_string().contains(unknown),
                "the refusal must name {unknown}, got {error}"
            );
        }
    }

    #[test]
    fn a_proposal_carries_its_secret_use_under_secret_use_and_omits_it_when_absent() {
        let with = CommandRunOutcome::Proposed {
            capability: upper(),
            input: json!({"text": "hello"}),
            secret_use: Some(SecretUseProposal::HttpBasic {
                secret: DRN.parse().expect("canonical DRN fixture"),
                username: "user-a".to_owned(),
            }),
        };
        let encoded = serde_json::to_string(&with).expect("a proposal serializes");
        assert_eq!(
            encoded,
            r#"{"outcome":"proposed","capability":"cli-probe.upper","input":{"text":"hello"},"secretUse":{"kind":"httpBasic","secret":"drn:com.example:secret:prod:api/token","username":"user-a"}}"#
        );
        assert_eq!(
            serde_json::from_str::<CommandRunOutcome>(&encoded).expect("the proposal decodes"),
            with
        );

        let without = CommandRunOutcome::Proposed {
            capability: upper(),
            input: json!({"text": "hello"}),
            secret_use: None,
        };
        let encoded = serde_json::to_string(&without).expect("a proposal serializes");
        assert_eq!(
            encoded,
            r#"{"outcome":"proposed","capability":"cli-probe.upper","input":{"text":"hello"}}"#
        );
        assert_eq!(
            serde_json::from_str::<CommandRunOutcome>(&encoded).expect("the proposal decodes"),
            without
        );
    }

    #[test]
    fn a_run_outcome_decodes_only_its_own_wire_shape() {
        let outcome = serde_json::from_str::<CommandRunOutcome>(
            r#"{"outcome":"rendered","stdout":"Usage: fixture\n","stderr":"","status":0}"#,
        )
        .expect("a rendered page parses");
        assert_eq!(
            outcome,
            CommandRunOutcome::Rendered {
                stdout: "Usage: fixture\n".to_owned(),
                stderr: String::new(),
                status: 0,
            }
        );

        let error = serde_json::from_str::<CommandRunOutcome>(
            r#"{"outcome":"resolved","capability":"fixture.run","input":{}}"#,
        )
        .expect_err("the retired resolution tag is not a run outcome");
        assert!(error.to_string().contains("resolved"), "{error}");

        let error = serde_json::from_str::<CommandRunOutcome>(
            r#"{"outcome":"rendered","stdout":"","stderr":"","status":0,"extra":1}"#,
        )
        .expect_err("unknown fields are refused");
        assert!(error.to_string().contains("extra"), "{error}");

        let error = serde_json::from_str::<CommandRunOutcome>(&format!(
            r#"{{"outcome":"proposed","capability":"fixture.run","input":{{}},"secretUse":{{"kind":"httpBasic","secret":"{DRN}","username":"user:password"}}}}"#
        ))
        .expect_err("a Basic username carrying a colon is refused");
        assert!(error.to_string().contains("colon-free"), "{error}");
    }

    #[test]
    fn describe_fallback_reports_the_serialization_error() {
        let error = serde_json::from_str::<ProviderManifest>("{").expect_err("malformed manifest");
        let encoded = describe_fallback(&error);

        let manifest =
            serde_json::from_str::<ProviderManifest>(&encoded).expect("fallback manifest decodes");
        assert_eq!(manifest.id.as_str(), "invalid");
        assert!(
            manifest
                .description
                .starts_with("manifest serialization failed: "),
            "{}",
            manifest.description
        );
        assert!(manifest.description.contains(&error.to_string()));
        assert!(manifest.capabilities.is_empty());
    }

    #[test]
    fn fallback_literals_decode_into_their_wire_types() {
        let error = serde_json::from_str::<ProviderManifest>("{").expect_err("malformed manifest");
        serde_json::from_str::<ProviderManifest>(&describe_fallback(&error))
            .expect("describe fallback decodes as a manifest");

        let run = serde_json::from_str::<CommandRunOutcome>(RUN_SERIALIZATION_FALLBACK)
            .expect("run fallback decodes as a command run outcome");
        assert_eq!(
            run,
            CommandRunOutcome::Failed {
                error: ComponentFailure {
                    code: "serialization-failed".to_owned(),
                    message: "command run could not be serialized".to_owned(),
                },
            }
        );
    }
}
