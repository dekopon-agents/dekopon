//! The script value type and its coercion rules.
//!
//! Every shell variable, command result, and pipeline element is a [`serde_json::Value`]. Nothing
//! in this interpreter is stringly typed, so capability inputs and outputs never need marshaling:
//! the rest of the workspace already speaks `serde_json::Value` everywhere.

use serde_json::{Map, Value};

/// Coerces one value to its display form.
///
/// This is the form used by bare-word arguments, double-quoted interpolation, and emitted output:
///
/// - strings are reproduced verbatim, without quotes,
/// - numbers use their JSON literal,
/// - booleans become `true` or `false`,
/// - null becomes the empty string,
/// - arrays and objects become compact JSON text.
#[must_use]
pub fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

/// Reports whether a value is "true" for `if`, `while`, and `test`.
///
/// This is a value-model predicate, not bash's exit-status rule: exit status drives control flow
/// in the evaluator, while this helper is only used by builtins that inspect a value directly.
#[must_use]
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// Converts a value into the line list consumed by text-shaped builtins.
///
/// A JSON array is treated as an array of lines (each element display-coerced). Every other value
/// is display-coerced and split on newlines. A trailing empty line is dropped so that
/// `"a\nb\n"` and `"a\nb"` behave identically.
#[must_use]
pub fn to_lines(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().map(display).collect(),
        Value::Null => Vec::new(),
        other => {
            let text = display(other);
            if text.is_empty() {
                return Vec::new();
            }
            let mut lines = text.split('\n').map(str::to_owned).collect::<Vec<_>>();
            if lines.last().is_some_and(String::is_empty) {
                lines.pop();
            }
            lines
        }
    }
}

/// Converts a line list back into a value.
///
/// A single line becomes a string so that `echo hi | grep hi` stays scalar; anything else becomes a
/// JSON array of lines so that later `jq` or index expressions see real structure.
#[must_use]
pub fn from_lines(lines: Vec<String>) -> Value {
    match lines.len() {
        0 => Value::String(String::new()),
        1 => Value::String(lines.into_iter().next().unwrap_or_default()),
        _ => Value::Array(lines.into_iter().map(Value::String).collect()),
    }
}

/// Converts a value into the text a text-shaped builtin operates on.
#[must_use]
pub fn to_text(value: &Value) -> String {
    match value {
        Value::Array(_) => to_lines(value).join("\n"),
        other => display(other),
    }
}

/// Indexes a value with one display-coerced key.
///
/// Arrays accept non-negative decimal indices; objects accept field names. Anything else yields
/// `null`, matching how a missing JSON field reads.
#[must_use]
pub fn index(value: &Value, key: &str) -> Value {
    match value {
        Value::Array(items) => key
            .parse::<usize>()
            .ok()
            .and_then(|offset| items.get(offset))
            .cloned()
            .unwrap_or(Value::Null),
        Value::Object(fields) => fields.get(key).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Parses one argv token into a value, keeping ambiguous text as a string.
///
/// Only JSON numbers, `true`, `false`, and `null` are promoted. Objects and arrays are deliberately
/// left as strings here so a flag value such as `--message '{"a":1}'` is not silently restructured;
/// the single-bare-argument JSON form used by `cap` is the explicit way to pass an object.
#[must_use]
pub fn scalar_from_token(token: &str) -> Value {
    match token {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" => return Value::Null,
        _ => {}
    }
    if let Ok(Value::Number(number)) = serde_json::from_str::<Value>(token) {
        return Value::Number(number);
    }
    Value::String(token.to_owned())
}

/// Builds an object from ordered key/value pairs, folding repeated keys into arrays.
#[must_use]
pub fn object_from_pairs(pairs: Vec<(String, Value)>) -> Value {
    let mut fields = Map::new();
    for (key, value) in pairs {
        match fields.remove(&key) {
            None => {
                fields.insert(key, value);
            }
            Some(Value::Array(mut existing)) => {
                existing.push(value);
                fields.insert(key, Value::Array(existing));
            }
            Some(existing) => {
                fields.insert(key, Value::Array(vec![existing, value]));
            }
        }
    }
    Value::Object(fields)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Value, display, from_lines, index, object_from_pairs, scalar_from_token, to_lines, truthy,
    };

    #[test]
    fn display_uses_documented_coercions() {
        assert_eq!(display(&json!("hi")), "hi");
        assert_eq!(display(&json!(7)), "7");
        assert_eq!(display(&json!(1.5)), "1.5");
        assert_eq!(display(&json!(true)), "true");
        assert_eq!(display(&Value::Null), "");
        assert_eq!(display(&json!([1, 2])), "[1,2]");
        assert_eq!(display(&json!({"a": 1})), r#"{"a":1}"#);
    }

    #[test]
    fn truthiness_follows_the_value_model() {
        assert!(!truthy(&Value::Null));
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("x")));
        assert!(!truthy(&json!(0)));
        assert!(truthy(&json!(3)));
        assert!(!truthy(&json!([])));
        assert!(truthy(&json!([1])));
    }

    #[test]
    fn text_shaped_conversions_round_trip() {
        assert_eq!(to_lines(&json!("a\nb")), vec!["a", "b"]);
        assert_eq!(to_lines(&json!("a\nb\n")), vec!["a", "b"]);
        assert_eq!(to_lines(&json!(["a", "b"])), vec!["a", "b"]);
        assert_eq!(to_lines(&Value::Null), Vec::<String>::new());
        assert_eq!(from_lines(vec!["only".to_owned()]), json!("only"));
        assert_eq!(
            from_lines(vec!["a".to_owned(), "b".to_owned()]),
            json!(["a", "b"])
        );
    }

    #[test]
    fn indexing_is_backed_by_real_json() {
        assert_eq!(index(&json!([10, 20]), "1"), json!(20));
        assert_eq!(index(&json!([10, 20]), "9"), Value::Null);
        assert_eq!(index(&json!({"key": "v"}), "key"), json!("v"));
        assert_eq!(index(&json!("scalar"), "0"), Value::Null);
    }

    #[test]
    fn argv_tokens_promote_only_unambiguous_scalars() {
        assert_eq!(scalar_from_token("7"), json!(7));
        assert_eq!(scalar_from_token("-1.5"), json!(-1.5));
        assert_eq!(scalar_from_token("true"), json!(true));
        assert_eq!(scalar_from_token("null"), Value::Null);
        assert_eq!(scalar_from_token("hello"), json!("hello"));
        assert_eq!(scalar_from_token(r#"{"a":1}"#), json!(r#"{"a":1}"#));
    }

    #[test]
    fn repeated_object_keys_fold_into_arrays() {
        let object = object_from_pairs(vec![
            ("headerName".to_owned(), json!("a")),
            ("headerName".to_owned(), json!("b")),
            ("other".to_owned(), json!(1)),
        ]);
        assert_eq!(object, json!({"headerName": ["a", "b"], "other": 1}));
    }
}
