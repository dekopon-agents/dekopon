//! `cap`: what this session was granted.
//!
//! `cap --list` (`-l`) prints the granted capability identifiers, and `cap --describe <id>` (`-d`)
//! prints one identifier with its description. Neither invokes anything. A capability is used
//! through the provider command word that proposes it, and that word's `--help` is where its
//! arguments are documented. Every other argument form is a usage error at exit `2`.

use serde_json::{Value, json};

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult};

/// The usage text every malformed `cap` reports.
const USAGE: &str = "usage: cap --list | cap --describe <capability> (what this session was \
                     granted — run `<word> --help` to use it)";

/// Lists and describes the session's granted capabilities.
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
            return Err(CommandFailure::usage(format!("cap: {USAGE}")));
        };

        match first.as_str() {
            "--list" | "-l" => {
                if !rest.is_empty() {
                    return Err(CommandFailure::usage(format!(
                        "cap: --list takes no arguments; {USAGE}"
                    )));
                }
                let mut granted = context.invoker.granted();
                granted.sort();
                Ok(CommandResult::value(Value::Array(
                    granted.into_iter().map(Value::String).collect(),
                )))
            }
            "--describe" | "-d" => {
                let [capability] = rest else {
                    return Err(CommandFailure::usage(format!(
                        "cap: --describe takes exactly one capability identifier; {USAGE}"
                    )));
                };
                let Some(description) = context.invoker.describe(capability) else {
                    return Err(CommandFailure::failed(format!(
                        "cap: {capability}: no description is available; try `cap --list`"
                    )));
                };
                Ok(CommandResult::value(json!({
                    "capability": description.capability,
                    "description": description.description,
                })))
            }
            other => Err(CommandFailure::usage(format!(
                "cap: unexpected argument {other:?}; {USAGE}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::{Value, json};

    use crate::{
        CapabilityCallResult, CapabilityDescription, CapabilityInvoker, ExitCode, Interpreter,
        Limits,
        builtins::{CommandFailure, test_support::run_builtin_with_invoker},
    };

    use super::Cap;

    /// A session granted two capabilities, one of them capability-shaped enough to tempt an
    /// invocation, that records every call `cap` might wrongly make.
    #[derive(Default)]
    struct Fixture {
        invoked: RefCell<Vec<String>>,
    }

    impl CapabilityInvoker for Fixture {
        fn granted(&self) -> Vec<String> {
            vec!["wikipedia_page".to_owned(), "alpha.write".to_owned()]
        }

        fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
            (capability == "alpha.write").then(|| CapabilityDescription {
                capability: capability.to_owned(),
                description: "Writes alpha".to_owned(),
            })
        }

        fn invoke(
            &self,
            capability: &str,
            _input: Value,
            secret_use: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            if secret_use.is_some() {
                return crate::secret_use_unsupported();
            }
            self.invoked.borrow_mut().push(capability.to_owned());
            CapabilityCallResult::NotFound
        }
    }

    fn usage_message(failure: CommandFailure) -> String {
        let CommandFailure::Status { message, status } = failure else {
            panic!("a usage error must stay recoverable");
        };
        assert_eq!(status, ExitCode::SYNTAX, "{message}");
        message
    }

    #[test]
    fn list_enumerates_granted_capabilities_in_order() {
        for flag in ["--list", "-l"] {
            let result = run_builtin_with_invoker(&Cap, &[flag], &Fixture::default())
                .expect("cap --list runs");
            assert_eq!(
                result.value,
                json!(["alpha.write", "wikipedia_page"]),
                "{flag}"
            );
        }
    }

    #[test]
    fn describe_prints_the_identifier_and_description_only() {
        for flag in ["--describe", "-d"] {
            let result =
                run_builtin_with_invoker(&Cap, &[flag, "alpha.write"], &Fixture::default())
                    .expect("cap --describe runs");
            assert_eq!(
                result.value,
                json!({"capability": "alpha.write", "description": "Writes alpha"}),
                "{flag}"
            );
        }
    }

    #[test]
    fn describing_a_capability_with_no_description_names_it() {
        let failure =
            run_builtin_with_invoker(&Cap, &["--describe", "missing.one"], &Fixture::default())
                .expect_err("nothing describes it");
        let CommandFailure::Status { message, status } = failure else {
            panic!("a missing description must stay recoverable");
        };
        assert_eq!(status, ExitCode::FAILURE);
        assert!(message.contains("missing.one"), "{message}");
    }

    #[test]
    fn a_capability_identifier_is_a_usage_error_rather_than_an_invocation() {
        let fixture = Fixture::default();
        let outcome = Interpreter::new(Limits::default()).run("cap wikipedia_page '{}'", &fixture);
        assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{}", outcome.output);
        assert!(
            outcome
                .output
                .contains("cap: unexpected argument \"wikipedia_page\"; usage: cap --list"),
            "{}",
            outcome.output
        );
        assert!(
            outcome.output.contains("run `<word> --help` to use it"),
            "{}",
            outcome.output
        );
        assert_eq!(outcome.capability_calls, 0);
        assert!(
            fixture.invoked.borrow().is_empty(),
            "cap invoked a capability"
        );
    }

    #[test]
    fn every_other_argument_form_is_a_usage_error() {
        let fixture = Fixture::default();
        for (arguments, cause) in [
            (&[][..], "cap: usage: cap --list"),
            (&["--list", "extra"][..], "--list takes no arguments"),
            (&["--describe"][..], "--describe takes exactly one"),
            (
                &["--describe", "a.b", "c.d"][..],
                "--describe takes exactly one",
            ),
            (&["--nope"][..], "unexpected argument \"--nope\""),
            (
                &["alpha.write", "--post-id", "7"][..],
                "unexpected argument \"alpha.write\"",
            ),
        ] {
            let message = usage_message(
                run_builtin_with_invoker(&Cap, arguments, &fixture)
                    .expect_err("malformed usage is refused"),
            );
            assert!(message.contains(cause), "{arguments:?}: {message}");
        }
        assert!(
            fixture.invoked.borrow().is_empty(),
            "cap invoked a capability"
        );
    }
}
