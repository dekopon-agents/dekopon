//! `base64 [-d|--decode]`.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult, unsupported_flag};
use crate::value::to_text;

/// Encodes or decodes standard base64.
///
/// Text-shaped: a JSON array of lines is newline-joined before encoding, so
/// `curl ... | base64` behaves the way a script expects.
pub(crate) struct Base64;

impl Builtin for Base64 {
    fn name(&self) -> &'static str {
        "base64"
    }

    fn run(
        &self,
        _context: &mut BuiltinContext<'_>,
        arguments: &[String],
        input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let mut decode = false;
        let mut literal = None;

        for argument in arguments {
            match argument.as_str() {
                "-d" | "-D" | "--decode" => decode = true,
                flag if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(unsupported_flag("base64", flag));
                }
                other => {
                    if literal.is_some() {
                        return Err(CommandFailure::usage(
                            "base64: at most one literal argument is supported",
                        ));
                    }
                    literal = Some(other.to_owned());
                }
            }
        }

        let text = match (literal, input) {
            (Some(literal), _) => literal,
            (None, Some(input)) => to_text(&input),
            (None, None) => String::new(),
        };

        if !decode {
            return Ok(CommandResult::value(Value::String(STANDARD.encode(text))));
        }

        // Real base64 tolerates embedded newlines in encoded input.
        let compact = text
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        let bytes = STANDARD
            .decode(compact.as_bytes())
            .map_err(|error| CommandFailure::failed(format!("base64: invalid input: {error}")))?;
        #[allow(
            clippy::map_err_ignore,
            reason = "FromUtf8Error adds only the byte offset of the first invalid sequence, and \
                      the message already names the whole diagnosis: the decode succeeded and the \
                      shell has no value type for the bytes it produced"
        )]
        let decoded = String::from_utf8(bytes).map_err(|_| {
            CommandFailure::failed(
                "base64: decoded bytes are not valid UTF-8, and this shell has no binary value type",
            )
        })?;
        Ok(CommandResult::value(Value::String(decoded)))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::builtins::test_support::run_builtin;

    use super::Base64;

    #[test]
    fn round_trips_through_encode_and_decode() {
        let encoded = run_builtin(&Base64, &[], Some(json!("hello world")))
            .expect("encodes")
            .value;
        assert_eq!(encoded, json!("aGVsbG8gd29ybGQ="));
        let decoded = run_builtin(&Base64, &["-d"], Some(encoded))
            .expect("decodes")
            .value;
        assert_eq!(decoded, json!("hello world"));
    }

    #[test]
    fn accepts_a_literal_argument() {
        assert_eq!(
            run_builtin(&Base64, &["hi"], None).expect("encodes").value,
            json!("aGk=")
        );
    }

    #[test]
    fn newline_joins_arrays_before_encoding() {
        let encoded = run_builtin(&Base64, &[], Some(json!(["a", "b"])))
            .expect("encodes")
            .value;
        let decoded = run_builtin(&Base64, &["--decode"], Some(encoded))
            .expect("decodes")
            .value;
        assert_eq!(decoded, json!("a\nb"));
    }

    #[test]
    fn tolerates_wrapped_encoded_input() {
        let decoded = run_builtin(&Base64, &["-d"], Some(json!("aGVsbG8g\nd29ybGQ=")))
            .expect("decodes")
            .value;
        assert_eq!(decoded, json!("hello world"));
    }

    #[test]
    fn invalid_input_fails_without_panicking() {
        assert!(run_builtin(&Base64, &["-d"], Some(json!("!!!!"))).is_err());
        assert!(run_builtin(&Base64, &["-d"], Some(json!("/w=="))).is_err());
        assert!(run_builtin(&Base64, &["-w", "0"], Some(Value::Null)).is_err());
    }
}
