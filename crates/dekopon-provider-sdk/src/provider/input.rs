mod decode;

use std::fmt;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_path_to_error::Segment;

use super::{Code, Failure, SdkFailure};

pub(super) struct InvalidInput {
    path: String,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    MissingField,
    UnknownField,
    WrongType,
    InvalidValue,
    InvalidLength,
    DuplicateField,
    Unlocated,
}

impl fmt::Display for InvalidInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            Kind::MissingField => "missing field",
            Kind::UnknownField => "unknown field",
            Kind::WrongType => "wrong type",
            Kind::InvalidValue => "invalid value",
            Kind::InvalidLength => "wrong length",
            Kind::DuplicateField => "duplicate field",
            Kind::Unlocated => return SdkFailure::InvalidInput.fmt(formatter),
        };
        let path = if self.path.is_empty() {
            "input"
        } else {
            &self.path
        };
        write!(formatter, "invalid input: {path}: {kind}")
    }
}

impl Failure for InvalidInput {
    fn code(&self) -> Code {
        Code::INVALID_INPUT
    }
}

fn field(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

fn resolved<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
        .unwrap_or(schema)
}

fn selected<'a>(root: &'a Value, schema: &'a Value, input: &Value) -> &'a Value {
    let schema = resolved(root, schema);
    let Some(variants) = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
    else {
        return schema;
    };
    let Some(properties) = variants
        .first()
        .and_then(|variant| variant.get("properties"))
        .and_then(Value::as_object)
    else {
        return schema;
    };
    let mut selected = None;
    for name in properties.keys() {
        let Some(value) = input.get(name) else {
            continue;
        };
        let mut candidate = None;
        let mut discriminates = true;
        for (index, variant) in variants.iter().enumerate() {
            let expected = variant
                .get("properties")
                .and_then(|properties| properties.get(name));
            let expected = expected.and_then(|property| {
                property.get("const").or_else(|| {
                    let choices = property.get("enum")?.as_array()?;
                    (choices.len() == 1).then(|| &choices[0])
                })
            });
            let Some(expected) = expected else {
                discriminates = false;
                break;
            };
            if expected == value {
                if candidate.is_some() {
                    discriminates = false;
                    break;
                }
                candidate = Some(index);
            }
        }
        if discriminates && let Some(index) = candidate {
            if selected.is_some_and(|previous| previous != index) {
                return schema;
            }
            selected = Some(index);
        }
    }
    selected.map_or(schema, |index| resolved(root, &variants[index]))
}

fn location<'a>(
    root: &'a Value,
    input: &'a Value,
    segments: &serde_path_to_error::Path,
) -> (String, &'a Value, &'a Value) {
    let mut path = String::new();
    let (mut schema, mut value) = (root, input);
    for segment in segments {
        schema = selected(root, schema, value);
        match segment {
            Segment::Map { key } => {
                let Some(child) = value.as_object().and_then(|object| object.get(key)) else {
                    break;
                };
                path = field(&path, key);
                value = child;
                schema = schema
                    .get("properties")
                    .and_then(|properties| properties.get(key))
                    .unwrap_or(&Value::Null);
            }
            Segment::Seq { index } => {
                let Some(child) = value.as_array().and_then(|array| array.get(*index)) else {
                    break;
                };
                path.push_str(&format!("[{index}]"));
                value = child;
                schema = schema
                    .get("prefixItems")
                    .and_then(|items| items.get(index))
                    .or_else(|| schema.get("items"))
                    .unwrap_or(&Value::Null);
            }
            Segment::Enum { variant } => {
                if let Some(child) = value.as_object().and_then(|object| object.get(variant)) {
                    value = child;
                    schema = schema
                        .get("properties")
                        .and_then(|properties| properties.get(variant))
                        .unwrap_or(&Value::Null);
                }
            }
            Segment::Unknown => break,
        }
    }
    (path, selected(root, schema, value), value)
}

fn accepts_type(schema: &Value, input: &Value) -> bool {
    fn accepts(kind: &str, input: &Value) -> bool {
        match kind {
            "object" => input.is_object(),
            "array" => input.is_array(),
            "string" => input.is_string(),
            "boolean" => input.is_boolean(),
            "null" => input.is_null(),
            "number" => input.is_number(),
            "integer" => input
                .as_number()
                .is_some_and(|number| number.is_i64() || number.is_u64()),
            _ => true,
        }
    }
    match schema.get("type") {
        Some(Value::String(kind)) => accepts(kind, input),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| accepts(kind, input)),
        _ => true,
    }
}

fn recover(
    root: &Value,
    schema: &Value,
    input: &Value,
    error: &decode::Error,
    path: &str,
) -> Option<InvalidInput> {
    let schema = selected(root, schema, input);
    let properties = schema.get("properties").and_then(Value::as_object);
    let name = error.field.as_deref();
    let at_field = match error.kind {
        Kind::MissingField => name.filter(|name| {
            input.is_object()
                && input.get(name).is_none()
                && properties.is_some_and(|properties| properties.contains_key(*name))
        }),
        Kind::UnknownField => name.filter(|name| {
            input
                .as_object()
                .is_some_and(|object| object.contains_key(*name))
                && properties.is_some_and(|properties| !properties.contains_key(*name))
        }),
        Kind::WrongType
        | Kind::InvalidValue
        | Kind::InvalidLength
        | Kind::DuplicateField
        | Kind::Unlocated => None,
    };
    if let Some(name) = at_field {
        return Some(InvalidInput {
            path: field(path, name),
            kind: error.kind,
        });
    }
    let wrong_type = match error.kind {
        Kind::WrongType => {
            error.unexpected.is_some_and(|shape| shape.matches(input))
                && !accepts_type(schema, input)
        }
        Kind::MissingField
        | Kind::UnknownField
        | Kind::InvalidValue
        | Kind::InvalidLength
        | Kind::DuplicateField
        | Kind::Unlocated => false,
    };
    if wrong_type {
        return Some(InvalidInput {
            path: path.to_owned(),
            kind: Kind::WrongType,
        });
    }
    match input {
        Value::Object(object) => properties.and_then(|properties| {
            object.iter().find_map(|(name, value)| {
                recover(
                    root,
                    properties.get(name)?,
                    value,
                    error,
                    &field(path, name),
                )
            })
        }),
        Value::Array(items) => items.iter().enumerate().find_map(|(index, value)| {
            let schema = schema
                .get("prefixItems")
                .and_then(|items| items.get(index))
                .or_else(|| schema.get("items"))?;
            recover(root, schema, value, error, &format!("{path}[{index}]"))
        }),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

pub(super) fn refusal<I: DeserializeOwned + JsonSchema>(input: &str) -> InvalidInput {
    let fallback = InvalidInput {
        path: String::new(),
        kind: Kind::Unlocated,
    };
    let Ok(value) = serde_json::from_str::<Value>(input) else {
        return fallback;
    };
    let Some(error) = decode::failure::<I>(input) else {
        return fallback;
    };
    let failure = error.inner();
    match failure.kind {
        Kind::DuplicateField | Kind::Unlocated => return fallback,
        Kind::MissingField
        | Kind::UnknownField
        | Kind::WrongType
        | Kind::InvalidValue
        | Kind::InvalidLength => {}
    }
    let schema = super::input_schema::<I>();
    let (path, at_schema, at_value) = location(&schema, &value, error.path());
    recover(&schema, at_schema, at_value, failure, &path).unwrap_or(InvalidInput {
        path,
        kind: failure.kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Deserializer};

    #[test]
    fn a_diagnostic_refusal_keeps_the_invalid_input_code() {
        assert_eq!(refusal::<String>("null").code(), Code::INVALID_INPUT);
    }

    #[test]
    fn enum_and_custom_errors_never_echo_input_values() {
        #[derive(Deserialize, schemars::JsonSchema)]
        enum Choice {
            Known,
        }

        #[derive(schemars::JsonSchema)]
        #[schemars(with = "String")]
        struct Custom;

        impl<'de> Deserialize<'de> for Custom {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                Err(serde::de::Error::custom(text))
            }
        }

        for (failure, kind, message) in [
            (
                refusal::<Choice>(r#""SENTINEL-INPUT-VALUE""#),
                Kind::InvalidValue,
                "invalid input: input: invalid value",
            ),
            (
                refusal::<Choice>(r#"{"SENTINEL-INPUT-VALUE":null}"#),
                Kind::InvalidValue,
                "invalid input: input: invalid value",
            ),
            (
                refusal::<Choice>(r#"{"Known":"SENTINEL-INPUT-VALUE"}"#),
                Kind::WrongType,
                "invalid input: input: wrong type",
            ),
            (
                refusal::<Custom>(r#""SENTINEL-INPUT-VALUE""#),
                Kind::InvalidValue,
                "invalid input: input: invalid value",
            ),
        ] {
            assert_eq!(failure.kind, kind);
            assert_eq!(failure.to_string(), message);
        }
    }

    #[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
    struct IdentifierInput {
        #[serde(deserialize_with = "identifier")]
        field: String,
    }

    fn identifier<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(field_identifier)]
        enum Identifier {
            Allowed,
        }
        match Identifier::deserialize(deserializer)? {
            Identifier::Allowed => Ok("Allowed".to_owned()),
        }
    }

    #[test]
    fn a_field_identifier_value_never_enters_the_diagnostic_path() {
        let failure = refusal::<IdentifierInput>(r#"{"field":"SENTINEL-INPUT-VALUE"}"#);
        assert_eq!(failure.to_string(), "invalid input: field: unknown field");
    }

    #[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
    struct WideInput {
        limit: u128,
        path: String,
    }

    #[test]
    fn a_valid_u128_field_is_not_blamed_for_a_later_type_error() {
        let failure = refusal::<WideInput>(r#"{"limit":1,"path":null}"#);
        assert_eq!(failure.to_string(), "invalid input: path: wrong type");
    }

    #[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
    struct RawInput {
        #[schemars(with = "serde_json::Value")]
        data: Box<serde_json::value::RawValue>,
        path: String,
    }

    #[test]
    fn a_valid_raw_json_field_is_not_blamed_for_a_later_type_error() {
        let failure = refusal::<RawInput>(r#"{"data":{"nested":true},"path":null}"#);
        assert_eq!(failure.to_string(), "invalid input: path: wrong type");
    }

    #[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
    #[serde(tag = "kind")]
    enum TaggedInput {
        Read { path: String },
        Count { limit: u128 },
    }

    #[test]
    fn an_internally_tagged_enum_refusal_names_the_failing_field() {
        let failure = refusal::<TaggedInput>(r#"{"kind":"Read","path":null}"#);
        assert_eq!(failure.to_string(), "invalid input: path: wrong type");
    }

    #[derive(serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
    struct NestedInput {
        items: Vec<TaggedInput>,
    }

    #[test]
    fn an_internally_tagged_enum_refusal_keeps_enclosing_fields_and_indices() {
        let failure = refusal::<NestedInput>(
            r#"{"items":[{"kind":"Read","path":"valid"},{"kind":"Read","path":null}]}"#,
        );
        assert_eq!(
            failure.to_string(),
            "invalid input: items[1].path: wrong type"
        );
    }
}
