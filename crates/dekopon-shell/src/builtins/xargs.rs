//! This module only builds the invocation plan; the interpreter re-enters itself to actually run
//! each one, and elements come from JSON array items or text lines rather than POSIX word
//! splitting.

use serde_json::Value;

use super::CommandFailure;
use crate::value::{display, to_lines};

pub(crate) const NAME: &str = "xargs";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Plan {
    pub invocations: Vec<Vec<String>>,
}

pub(crate) fn plan(arguments: &[String], input: Option<&Value>) -> Result<Plan, CommandFailure> {
    let mut placeholder: Option<String> = None;
    let mut index = 0;

    while index < arguments.len() {
        match arguments[index].as_str() {
            "-I" | "--replace" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err(CommandFailure::usage(
                        "xargs: -I requires a placeholder token",
                    ));
                };
                if value.is_empty() {
                    return Err(CommandFailure::usage(
                        "xargs: the -I placeholder must not be empty",
                    ));
                }
                placeholder = Some(value.clone());
                index += 2;
            }
            "-n" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err(CommandFailure::usage("xargs: -n requires a count"));
                };
                if value != "1" {
                    return Err(CommandFailure::usage(
                        "xargs: only -n 1 is supported; each element becomes one invocation",
                    ));
                }
                index += 2;
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(super::unsupported_flag("xargs", flag));
            }
            _ => break,
        }
    }

    let template = &arguments[index..];
    let Some(command) = template.first() else {
        return Err(CommandFailure::usage(
            "xargs: a command is required, as in `xargs gh issue view`",
        ));
    };
    if command.starts_with('-') {
        return Err(CommandFailure::usage(format!(
            "xargs: expected a command, found flag {command}"
        )));
    }

    let elements = match input {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().map(display).collect(),
        Some(other) => to_lines(other),
    };

    let invocations = elements
        .into_iter()
        .map(|element| match &placeholder {
            Some(placeholder) => template
                .iter()
                .map(|word| word.replace(placeholder.as_str(), &element))
                .collect(),
            None => {
                let mut argv = template.to_vec();
                argv.push(element);
                argv
            }
        })
        .collect();

    Ok(Plan { invocations })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::plan;

    fn arguments(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn appends_each_array_element_as_a_trailing_argument() {
        let planned = plan(&arguments(&["probe", "--id"]), Some(&json!([1, 2]))).expect("plans");
        assert_eq!(
            planned.invocations,
            vec![
                arguments(&["probe", "--id", "1"]),
                arguments(&["probe", "--id", "2"]),
            ]
        );
    }

    #[test]
    fn a_placeholder_substitutes_anywhere_in_the_template() {
        let planned = plan(
            &arguments(&["-I", "{}", "probe", "--id", "{}", "--tag", "x{}y"]),
            Some(&json!(["7"])),
        )
        .expect("plans");
        assert_eq!(
            planned.invocations,
            vec![arguments(&["probe", "--id", "7", "--tag", "x7y"])]
        );
    }

    #[test]
    fn line_oriented_text_yields_one_invocation_per_line() {
        let planned = plan(&arguments(&["echo"]), Some(&json!("a\nb"))).expect("plans");
        assert_eq!(
            planned.invocations,
            vec![arguments(&["echo", "a"]), arguments(&["echo", "b"])]
        );
    }

    #[test]
    fn no_input_plans_no_invocations() {
        assert!(
            plan(&arguments(&["echo"]), None)
                .expect("plans")
                .invocations
                .is_empty()
        );
        assert!(
            plan(&arguments(&["echo"]), Some(&Value::Null))
                .expect("plans")
                .invocations
                .is_empty()
        );
    }

    #[test]
    fn object_elements_are_passed_as_compact_json() {
        let planned = plan(&arguments(&["cap", "x.y"]), Some(&json!([{"a": 1}]))).expect("plans");
        assert_eq!(
            planned.invocations,
            vec![arguments(&["cap", "x.y", r#"{"a":1}"#])]
        );
    }

    #[test]
    fn malformed_usage_is_rejected() {
        assert!(plan(&arguments(&[]), Some(&json!([1]))).is_err());
        assert!(plan(&arguments(&["-I"]), Some(&json!([1]))).is_err());
        assert!(plan(&arguments(&["-n", "5", "echo"]), Some(&json!([1]))).is_err());
        assert!(plan(&arguments(&["-P", "4", "echo"]), Some(&json!([1]))).is_err());
    }
}
