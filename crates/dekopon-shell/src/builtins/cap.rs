//! The `cap` escape hatch: `cap --list`, `cap --describe <id>`, and `cap <id> [input]`.
//!
//! Capability fallback in [`crate::dispatch`] only fires for words that look like capability
//! identifiers. `cap` is always available regardless of naming, so a capability whose identifier
//! collides with a builtin name, or whose shape the fallback rule declines, is still reachable.

use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult};
use crate::dispatch::arguments_to_input;

/// The capability escape hatch.
pub(crate) struct Cap;

impl Builtin for Cap {
    fn name(&self) -> &'static str {
        "cap"
    }

    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let Some((first, rest)) = arguments.split_first() else {
            return Err(CommandFailure::usage(
                "cap: usage: cap --list | cap --describe <capability> | cap <capability> [input]",
            ));
        };

        match first.as_str() {
            "--list" | "-l" => {
                if !rest.is_empty() {
                    return Err(CommandFailure::usage("cap --list takes no arguments"));
                }
                let mut granted = context.invoker.granted();
                granted.sort();
                Ok(CommandResult::value(Value::Array(
                    granted.into_iter().map(Value::String).collect(),
                )))
            }
            "--describe" | "-d" => {
                let [capability] = rest else {
                    return Err(CommandFailure::usage(
                        "cap --describe requires exactly one capability identifier",
                    ));
                };
                let Some(description) = context.invoker.describe(capability) else {
                    return Err(CommandFailure::failed(format!(
                        "cap: {capability}: no description is available; try `cap --list`"
                    )));
                };
                Ok(CommandResult::value(serde_json::json!({
                    "capability": description.capability,
                    "description": description.description,
                    "inputSchema": description.input_schema,
                })))
            }
            flag if flag.starts_with("--") => Err(CommandFailure::usage(format!(
                "cap: unknown option {flag}; usage: cap --list | cap --describe <capability> | cap <capability> [input]"
            ))),
            capability => {
                let input = arguments_to_input("cap", rest)?;
                let capability = capability.to_owned();
                context.invoke_capability(&capability, input)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        CapabilityCallResult, CapabilityDescription, CapabilityInvoker, ExitCode,
        builtins::{CommandFailure, test_support::run_builtin_with_invoker},
    };

    use super::Cap;

    struct Fixture;

    impl CapabilityInvoker for Fixture {
        fn granted(&self) -> Vec<String> {
            vec!["zulu.read".to_owned(), "alpha.write".to_owned()]
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            (capability == "alpha.write").then(|| CapabilityDescription {
                capability: capability.to_owned(),
                description: "Writes alpha".to_owned(),
                input_schema: json!({"type": "object"}),
            })
        }

        fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
            match capability {
                "alpha.write" => CapabilityCallResult::Succeeded(input),
                "zulu.read" => CapabilityCallResult::Denied {
                    reason: "policy".to_owned(),
                },
                _ => CapabilityCallResult::NotFound,
            }
        }
    }

    #[test]
    fn list_enumerates_granted_capabilities_in_order() {
        let result =
            run_builtin_with_invoker(&Cap, &["--list"], &Fixture).expect("cap --list runs");
        assert_eq!(result.value, json!(["alpha.write", "zulu.read"]));
    }

    #[test]
    fn describe_returns_model_facing_metadata() {
        let result = run_builtin_with_invoker(&Cap, &["--describe", "alpha.write"], &Fixture)
            .expect("cap --describe runs");
        assert_eq!(result.value["capability"], json!("alpha.write"));
        assert_eq!(result.value["inputSchema"], json!({"type": "object"}));
        assert!(run_builtin_with_invoker(&Cap, &["--describe", "missing.one"], &Fixture).is_err());
    }

    #[test]
    fn invokes_a_capability_with_flag_converted_input() {
        let result = run_builtin_with_invoker(
            &Cap,
            &["alpha.write", "--post-id", "7", "--include-body"],
            &Fixture,
        )
        .expect("cap invokes");
        assert_eq!(result.value, json!({"postId": 7, "includeBody": true}));
    }

    #[test]
    fn accepts_a_literal_json_object() {
        let result = run_builtin_with_invoker(&Cap, &["alpha.write", r#"{"raw":true}"#], &Fixture)
            .expect("cap invokes");
        assert_eq!(result.value, json!({"raw": true}));
    }

    #[test]
    fn a_denied_capability_reports_exit_code_126() {
        let failure =
            run_builtin_with_invoker(&Cap, &["zulu.read"], &Fixture).expect_err("policy refuses");
        let CommandFailure::Status { message, status } = failure else {
            panic!("a denial must stay recoverable");
        };
        assert_eq!(status, ExitCode::DENIED);
        assert!(message.contains("denied"), "{message}");
    }

    #[test]
    fn an_unknown_capability_reports_exit_code_127() {
        let failure = run_builtin_with_invoker(&Cap, &["nope.missing"], &Fixture)
            .expect_err("unknown capabilities fail");
        let CommandFailure::Status { status, .. } = failure else {
            panic!("an unknown capability must stay recoverable");
        };
        assert_eq!(status, ExitCode::NOT_FOUND);
    }

    #[test]
    fn malformed_usage_is_rejected() {
        assert!(run_builtin_with_invoker(&Cap, &[], &Fixture).is_err());
        assert!(run_builtin_with_invoker(&Cap, &["--list", "extra"], &Fixture).is_err());
        assert!(run_builtin_with_invoker(&Cap, &["--describe"], &Fixture).is_err());
        assert!(run_builtin_with_invoker(&Cap, &["--nope"], &Fixture).is_err());
    }
}
