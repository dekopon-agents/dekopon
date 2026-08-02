//! The `jq` builtin, backed by the real jaq interpreter.
//!
//! This wraps `jaq-core`, `jaq-std`, and `jaq-json` as a library rather than hand-rolling a jq
//! subset. A model that knows jq gets jq, not an approximation of it that quietly differs.

use jaq_core::{
    Compiler, Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::Val;
use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};

/// `jq [-r|--raw-output] FILTER`.
pub(crate) struct Jq;

impl Builtin for Jq {
    fn name(&self) -> &'static str {
        "jq"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut filter = None;
        for argument in arguments {
            match argument.as_str() {
                // `-r` and `-c` are accepted and documented as no-ops rather than rejected: the
                // value model already emits string results verbatim and renders structures
                // compactly, so raw compact output is this shell's only output mode. Nothing is
                // silently different from what these flags request.
                "-r" | "--raw-output" | "-c" | "--compact-output" => {}
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("jq", flag));
                }
                _ => {
                    if filter.is_some() {
                        return Err(CommandFailure::usage(
                            "jq: exactly one filter argument is supported",
                        ));
                    }
                    filter = Some(argument.clone());
                }
            }
        }
        let Some(filter) = filter else {
            return Err(CommandFailure::usage("jq: a filter argument is required"));
        };

        let input = input.unwrap_or(Value::Null);
        evaluate(&filter, &input)
            .map(CommandResult::value)
            .map_err(CommandFailure::failed)
    }
}

/// Compiles and runs one jq filter over one value.
pub(crate) fn evaluate(filter: &str, input: &Value) -> Result<Value, String> {
    let definitions = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let functions = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(definitions);
    let arena = Arena::default();
    let modules = loader
        .load(
            &arena,
            File {
                code: filter,
                path: (),
            },
        )
        .map_err(|errors| format!("jq: invalid filter: {}", describe_load_errors(&errors)))?;
    let compiled = Compiler::default()
        .with_funs(functions)
        .compile(modules)
        .map_err(|errors| format!("jq: invalid filter: {}", describe_compile_errors(&errors)))?;

    let text =
        serde_json::to_string(input).map_err(|error| format!("jq: invalid input: {error}"))?;
    let value = jaq_json::read::parse_single(text.as_bytes())
        .map_err(|error| format!("jq: invalid input: {error}"))?;

    let context = Ctx::<data::JustLut<Val>>::new(&compiled.lut, Vars::new([]));
    let mut outputs = Vec::new();
    for result in compiled.id.run((context, value)).map(unwrap_valr) {
        let produced = result.map_err(|error| format!("jq: {error}"))?;
        outputs.push(
            serde_json::from_str::<Value>(&produced.to_string())
                .map_err(|error| format!("jq: could not read filter output: {error}"))?,
        );
    }

    // A jq filter is a stream. One output stays scalar so `| jq .field | grep x` reads naturally;
    // several outputs become a JSON array so nothing is silently discarded.
    Ok(match outputs.len() {
        0 => Value::Null,
        1 => outputs.into_iter().next().unwrap_or(Value::Null),
        _ => Value::Array(outputs),
    })
}

fn describe_load_errors<P>(errors: &[(File<&str, P>, jaq_core::load::Error<&str>)]) -> String {
    errors
        .iter()
        .map(|(_, error)| match error {
            jaq_core::load::Error::Io(entries) => entries
                .iter()
                .map(|(name, message)| format!("{name}: {message}"))
                .collect::<Vec<_>>()
                .join("; "),
            // `Expect::as_str` panics for non-standard delimiters, so lex errors are described
            // structurally instead. An untrusted filter must never be able to abort the process.
            jaq_core::load::Error::Lex(entries) => entries
                .iter()
                .map(|(expected, found)| format!("expected {expected:?} near {found:?}"))
                .collect::<Vec<_>>()
                .join("; "),
            jaq_core::load::Error::Parse(entries) => entries
                .iter()
                .map(|(expected, found)| format!("expected {} near {found:?}", expected.as_str()))
                .collect::<Vec<_>>()
                .join("; "),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// One file's compilation errors, as `jaq-core` reports them.
type CompileErrors<'a, P> = (File<&'a str, P>, Vec<jaq_core::compile::Error<&'a str>>);

fn describe_compile_errors<P>(errors: &[CompileErrors<'_, P>]) -> String {
    errors
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .map(|(symbol, undefined)| format!("undefined {} {symbol:?}", undefined.as_str()))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::evaluate;

    #[test]
    fn evaluates_real_jq_filters() {
        assert_eq!(
            evaluate(".a", &json!({"a": 1})).expect("filter runs"),
            json!(1)
        );
        assert_eq!(
            evaluate("map(. * 2)", &json!([1, 2, 3])).expect("filter runs"),
            json!([2, 4, 6])
        );
        assert_eq!(
            evaluate(
                "{name: .id, total: (.items | length)}",
                &json!({"id": "x", "items": [1, 2]})
            )
            .expect("filter runs"),
            json!({"name": "x", "total": 2})
        );
    }

    #[test]
    fn standard_library_functions_are_available() {
        assert_eq!(
            evaluate("[.[] | select(. > 1)] | sort | reverse", &json!([3, 1, 2]))
                .expect("filter runs"),
            json!([3, 2])
        );
        assert_eq!(
            evaluate("to_entries | map(.key)", &json!({"b": 2, "a": 1})).expect("filter runs"),
            json!(["a", "b"])
        );
    }

    #[test]
    fn a_multi_output_filter_becomes_an_array() {
        assert_eq!(
            evaluate(".[]", &json!([1, 2, 3])).expect("filter runs"),
            json!([1, 2, 3])
        );
    }

    #[test]
    fn an_empty_stream_becomes_null() {
        assert_eq!(
            evaluate("empty", &json!(1)).expect("filter runs"),
            Value::Null
        );
    }

    #[test]
    fn raw_and_compact_flags_are_accepted_because_they_match_the_only_output_mode() {
        use crate::builtins::test_support::run_builtin;

        for flags in [
            vec!["-r", ".a"],
            vec!["-c", ".a"],
            vec!["--raw-output", ".a"],
            vec!["--compact-output", ".a"],
        ] {
            let result = run_builtin(&super::Jq, &flags, Some(json!({"a": "x"})))
                .expect("documented output flags are accepted");
            assert_eq!(result.value, json!("x"), "{flags:?}");
        }
        assert!(run_builtin(&super::Jq, &["--slurp", "."], Some(json!(1))).is_err());
    }

    #[test]
    fn invalid_filters_report_an_error_instead_of_panicking() {
        let error = evaluate(".[", &json!({})).expect_err("unbalanced filter");
        assert!(error.starts_with("jq: invalid filter"), "{error}");
        let error = evaluate("no_such_function", &json!({})).expect_err("undefined filter");
        assert!(error.contains("undefined"), "{error}");
    }

    #[test]
    fn runtime_errors_are_reported_not_fatal() {
        let error = evaluate(".a", &json!([1, 2])).expect_err("indexing an array by name fails");
        assert!(error.starts_with("jq:"), "{error}");
    }
}
