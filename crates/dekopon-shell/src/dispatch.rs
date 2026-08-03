//! Command-word resolution and the argv-to-JSON convention for capability calls.
//!
//! Resolution order is fixed:
//!
//! 1. words this shell refuses outright (`eval`, `exec`, `source`, job control, `declare`),
//! 2. shell functions declared earlier in the same script,
//! 3. the fixed builtin table,
//! 4. capability fallback, and
//! 5. `cap <id>`, which is itself a builtin and therefore already covered by step 3.
//!
//! Otherwise the word is "command not found", exit code 127.
//!
//! Steps 3 and 4 cannot collide. Every builtin name is separator-free, and capability fallback only
//! fires for words containing `.`, `-`, or `_`. The two sets are disjoint by construction, not by
//! luck, and [`tests::builtin_and_capability_namespaces_are_disjoint`] proves it.

use std::collections::BTreeSet;

use dekopon_core::CapabilityId;
use serde_json::Value;

use crate::{
    CapabilityInvoker,
    builtins::{self, BuiltinKind, CommandFailure},
    parser::REJECTED_COMMANDS,
    value::{object_from_pairs, scalar_from_token},
};

/// How one command word resolves.
pub(crate) enum Resolution {
    /// A shell function declared earlier in this script.
    Function,
    /// A builtin.
    Builtin(BuiltinKind),
    /// A granted capability, invoked through the fallback rule.
    Capability,
    /// A word this shell refuses, with the reason why.
    Rejected(&'static str),
    /// Nothing matched.
    NotFound,
}

/// Resolves one command word.
pub(crate) fn resolve(
    word: &str,
    functions: &BTreeSet<String>,
    invoker: &dyn CapabilityInvoker,
) -> Resolution {
    if let Some((_, reason)) = REJECTED_COMMANDS
        .iter()
        .find(|(rejected, _)| *rejected == word)
    {
        return Resolution::Rejected(reason);
    }
    if functions.contains(word) {
        return Resolution::Function;
    }
    if let Some(builtin) = builtins::lookup(word) {
        return Resolution::Builtin(builtin);
    }
    if looks_like_capability(word) && invoker.is_granted(word) {
        return Resolution::Capability;
    }
    Resolution::NotFound
}

/// Reports whether a word could be a capability identifier.
///
/// A capability identifier is not *required* to contain a separator by `dekopon-core`'s rules, but
/// this shell requires one before it will try capability fallback. That extra condition is what
/// keeps the builtin and capability namespaces provably disjoint; a separator-free capability
/// remains reachable through `cap <id>`.
#[must_use]
pub(crate) fn looks_like_capability(word: &str) -> bool {
    word.contains(['.', '-', '_']) && word.parse::<CapabilityId>().is_ok()
}

/// Converts argv into a capability input object.
///
/// `some.capability --post-id 7 --include-body` becomes `{"postId": 7, "includeBody": true}`, so
/// kebab-case flags land on the camelCase keys this workspace uses everywhere. A single bare
/// argument starting with `{` bypasses conversion and is parsed as the literal input object.
pub(crate) fn arguments_to_input(
    command: &str,
    arguments: &[String],
) -> Result<Value, CommandFailure> {
    if arguments.is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }

    if let [single] = arguments {
        if single.trim_start().starts_with('{') {
            let parsed = serde_json::from_str::<Value>(single).map_err(|error| {
                CommandFailure::usage(format!("{command}: input is not valid JSON: {error}"))
            })?;
            if !parsed.is_object() {
                return Err(CommandFailure::usage(format!(
                    "{command}: capability input must be a JSON object"
                )));
            }
            return Ok(parsed);
        }
    }

    let mut pairs = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        let Some(flag) = argument.strip_prefix("--") else {
            return Err(CommandFailure::usage(format!(
                "{command}: unexpected argument {argument:?}; pass capability input as --kebab-case flags or one JSON object"
            )));
        };
        if flag.is_empty() {
            return Err(CommandFailure::usage(format!(
                "{command}: `--` is not a capability input flag"
            )));
        }

        let key = to_camel_case(flag);
        match arguments.get(index + 1) {
            // A flag followed by another flag, or nothing, is a boolean present-flag.
            None => {
                pairs.push((key, Value::Bool(true)));
                index += 1;
            }
            Some(next) if next.starts_with("--") => {
                pairs.push((key, Value::Bool(true)));
                index += 1;
            }
            Some(next) => {
                pairs.push((key, scalar_from_token(next)));
                index += 2;
            }
        }
    }

    Ok(object_from_pairs(pairs))
}

/// Converts one kebab-case flag name to camelCase.
fn to_camel_case(flag: &str) -> String {
    let mut output = String::with_capacity(flag.len());
    let mut capitalize = false;
    for character in flag.chars() {
        if character == '-' || character == '_' {
            capitalize = true;
            continue;
        }
        if capitalize {
            output.extend(character.to_uppercase());
            capitalize = false;
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    use crate::{CapabilityCallResult, CapabilityInvoker, builtins};

    use super::{Resolution, arguments_to_input, looks_like_capability, resolve, to_camel_case};

    struct Granted;

    impl CapabilityInvoker for Granted {
        fn granted(&self) -> Vec<String> {
            vec![
                "echo.echo".to_owned(),
                "http-probe.fetch".to_owned(),
                "with_underscore".to_owned(),
            ]
        }

        fn invoke(&self, _capability: &str, input: Value) -> CapabilityCallResult {
            CapabilityCallResult::Succeeded(input)
        }
    }

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn resolution_follows_the_documented_priority_order() {
        let mut functions = BTreeSet::new();
        functions.insert("greet".to_owned());

        assert!(matches!(
            resolve("greet", &functions, &Granted),
            Resolution::Function
        ));
        assert!(matches!(
            resolve("jq", &functions, &Granted),
            Resolution::Builtin(_)
        ));
        assert!(matches!(
            resolve("echo.echo", &functions, &Granted),
            Resolution::Capability
        ));
        assert!(matches!(
            resolve("eval", &functions, &Granted),
            Resolution::Rejected(_)
        ));
        assert!(matches!(
            resolve("nothing.here", &functions, &Granted),
            Resolution::NotFound
        ));
        assert!(matches!(
            resolve("unknown", &functions, &Granted),
            Resolution::NotFound
        ));
    }

    #[test]
    fn a_function_takes_priority_over_a_granted_capability() {
        let mut functions = BTreeSet::new();
        functions.insert("echo.echo".to_owned());
        assert!(matches!(
            resolve("echo.echo", &functions, &Granted),
            Resolution::Function
        ));
    }

    #[test]
    fn a_function_shadows_a_builtin_only_by_being_declared() {
        let mut functions = BTreeSet::new();
        assert!(matches!(
            resolve("jq", &functions, &Granted),
            Resolution::Builtin(_)
        ));
        functions.insert("jq".to_owned());
        assert!(matches!(
            resolve("jq", &functions, &Granted),
            Resolution::Function
        ));
    }

    #[test]
    fn builtin_and_capability_namespaces_are_disjoint() {
        // Capability fallback demands a separator; builtin names have none. No word can satisfy
        // both rules, so no builtin can ever be shadowed by a granted capability.
        for name in builtins::names() {
            assert!(
                !looks_like_capability(name),
                "builtin {name:?} is reachable through capability fallback"
            );
        }
        assert!(looks_like_capability("echo.echo"));
        assert!(looks_like_capability("http-probe.fetch"));
        assert!(looks_like_capability("with_underscore"));
        // Separator-free identifiers are valid capability IDs but stay out of fallback on purpose.
        assert!(!looks_like_capability("echo"));
        assert!(!looks_like_capability("Echo.Echo"));
        assert!(!looks_like_capability("bad..id"));
    }

    #[test]
    fn kebab_flags_become_camel_case_keys() {
        assert_eq!(to_camel_case("post-id"), "postId");
        assert_eq!(to_camel_case("include-body"), "includeBody");
        assert_eq!(to_camel_case("id"), "id");
        assert_eq!(to_camel_case("a-b-c"), "aBC");
    }

    #[test]
    fn argv_converts_to_the_camel_case_input_object() {
        assert_eq!(
            arguments_to_input("cap", &arguments(&["--post-id", "7", "--include-body"]))
                .expect("valid argv"),
            json!({"postId": 7, "includeBody": true})
        );
        assert_eq!(
            arguments_to_input("cap", &arguments(&["--message", "hello"])).expect("valid argv"),
            json!({"message": "hello"})
        );
        assert_eq!(
            arguments_to_input("cap", &arguments(&[])).expect("valid argv"),
            json!({})
        );
    }

    #[test]
    fn a_single_json_argument_bypasses_flag_conversion() {
        assert_eq!(
            arguments_to_input("cap", &arguments(&[r#"{"postId": 7}"#])).expect("valid argv"),
            json!({"postId": 7})
        );
        assert!(arguments_to_input("cap", &arguments(&["{not json}"])).is_err());
        assert!(arguments_to_input("cap", &arguments(&["{\"a\": 1}", "extra"])).is_err());
    }

    #[test]
    fn repeated_flags_fold_into_arrays() {
        assert_eq!(
            arguments_to_input("cap", &arguments(&["--tag", "a", "--tag", "b"]))
                .expect("valid argv"),
            json!({"tag": ["a", "b"]})
        );
    }

    #[test]
    fn positional_arguments_are_rejected_with_guidance() {
        let failure =
            arguments_to_input("cap", &arguments(&["bare"])).expect_err("bare words are rejected");
        assert!(format!("{failure:?}").contains("kebab-case"), "{failure:?}");
    }
}
