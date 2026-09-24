use serde_json::Value;

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

#[must_use]
#[cfg(test)]
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

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

#[must_use]
pub fn from_lines(lines: Vec<String>) -> Value {
    match lines.len() {
        0 => Value::Null,
        1 => Value::String(lines.into_iter().next().unwrap_or_default()),
        _ => Value::Array(lines.into_iter().map(Value::String).collect()),
    }
}

#[must_use]
pub fn to_text(value: &Value) -> String {
    match value {
        Value::Array(_) => to_lines(value).join("\n"),
        other => display(other),
    }
}

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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Value, display, from_lines, index, scalar_from_token, to_lines, truthy};

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
        assert_eq!(from_lines(Vec::new()), Value::Null);
    }

    #[test]
    fn indexing_is_backed_by_real_json() {
        assert_eq!(index(&json!([10, 20]), "1"), json!(20));
        assert_eq!(index(&json!([10, 20]), "9"), Value::Null);
        assert_eq!(index(&json!({"key": "v"}), "key"), json!("v"));
        assert_eq!(index(&json!("scalar"), "0"), Value::Null);
    }

    #[test]
    fn local_tokens_promote_only_unambiguous_scalars() {
        assert_eq!(scalar_from_token("7"), json!(7));
        assert_eq!(scalar_from_token("-1.5"), json!(-1.5));
        assert_eq!(scalar_from_token("true"), json!(true));
        assert_eq!(scalar_from_token("null"), Value::Null);
        assert_eq!(scalar_from_token("hello"), json!("hello"));
        assert_eq!(scalar_from_token(r#"{"a":1}"#), json!(r#"{"a":1}"#));
    }
}
