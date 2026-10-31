use dekopon_core::redaction_marker;
use serde_json::{Map, Value};

fn itoa(index: usize) -> String {
    index.to_string()
}

const SECRET_KEYS: &[&str] = &[
    "apikey",
    "authorization",
    "cookie",
    "credential",
    "credentials",
    "password",
    "privatekey",
    "secret",
    "session",
    "sessiontoken",
    "token",
];

const MIN_SECRET_LENGTH: usize = 20;

fn key_is_secret(key: &str) -> bool {
    let folded: String = key
        .chars()
        .filter(|character| *character != '-' && *character != '_')
        .flat_map(char::to_lowercase)
        .collect();
    SECRET_KEYS.contains(&folded.as_str())
}

fn is_secret_body(value: &str) -> bool {
    value.len() >= MIN_SECRET_LENGTH
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn is_token_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '+' | '/' | '=')
}

fn run_is_secret(run: &str) -> bool {
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "sk-"] {
        if let Some(body) = run.strip_prefix(prefix)
            && is_secret_body(body)
        {
            return true;
        }
    }

    // A three-segment JWT, checked structurally rather than by its conventional `eyJ` header, with
    // every segment long enough that a dotted identifier cannot be mistaken for one.
    let segments: Vec<&str> = run.split('.').collect();
    segments.len() == 3
        && segments
            .iter()
            .all(|segment| segment.len() >= 16 && is_secret_body(segment))
}

fn scheme_introduces_secret(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "bearer" | "basic" | "token"
    )
}

fn redact_text(text: &str) -> Option<String> {
    let mut output = String::with_capacity(text.len());
    let mut previous_run: Option<String> = None;
    let mut found = false;
    let mut rest = text;

    while !rest.is_empty() {
        let run_length = rest.find(|character| !is_token_char(character));
        match run_length {
            Some(0) => {
                let separator = rest.chars().next().unwrap_or_default();
                // Only whitespace separates a scheme from its credential; a comma or a quote ends
                // the pairing, so `Bearer, abc` is not a token introduction.
                if !separator.is_whitespace() {
                    previous_run = None;
                }
                output.push(separator);
                rest = &rest[separator.len_utf8()..];
            }
            _ => {
                let end = run_length.unwrap_or(rest.len());
                let run = &rest[..end];
                let introduced = previous_run
                    .as_deref()
                    .is_some_and(scheme_introduces_secret)
                    && run.len() >= MIN_SECRET_LENGTH;
                if introduced || run_is_secret(run) {
                    output.push_str(&redaction_marker(run.chars().count()));
                    found = true;
                } else {
                    output.push_str(run);
                }
                previous_run = Some(run.to_owned());
                rest = &rest[end..];
            }
        }
    }

    found.then_some(output)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redaction {
    pub path: String,
    pub reason: RedactionReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedactionReason {
    Key,
    Shape,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Redacted {
    pub value: Value,
    pub redactions: Vec<Redaction>,
}

impl Redacted {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.redactions.is_empty()
    }
}

#[must_use]
pub fn redact(value: &Value) -> Redacted {
    let mut redacted = Redacted::default();
    redacted.value = walk(value, false, &mut String::new(), &mut redacted.redactions);
    redacted
}

fn walk(
    value: &Value,
    key_says_secret: bool,
    path: &mut String,
    found: &mut Vec<Redaction>,
) -> Value {
    match value {
        Value::String(text) => {
            // A secret key hides the whole value; a shape match hides only the run that matched, so
            // the surrounding text an operator was reading survives.
            if key_says_secret {
                found.push(Redaction {
                    path: path.clone(),
                    reason: RedactionReason::Key,
                });
                return Value::String(redaction_marker(text.chars().count()));
            }
            match redact_text(text) {
                Some(redacted) => {
                    found.push(Redaction {
                        path: path.clone(),
                        reason: RedactionReason::Shape,
                    });
                    Value::String(redacted)
                }
                None => Value::String(text.clone()),
            }
        }
        // A secret key holding a non-string is still a secret: `{"token": 12345678901234567890}`
        // is a credential wearing a number's clothes.
        other @ (Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_))
            if key_says_secret =>
        {
            found.push(Redaction {
                path: path.clone(),
                reason: RedactionReason::Key,
            });
            Value::String(redaction_marker(other.to_string().chars().count()))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let restore = path.len();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(itoa(index).as_str());
                    let mapped = walk(item, false, path, found);
                    path.truncate(restore);
                    mapped
                })
                .collect(),
        ),
        Value::Object(fields) => {
            let mut mapped = Map::with_capacity(fields.len());
            for (key, field) in fields {
                let restore = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
                mapped.insert(key.clone(), walk(field, key_is_secret(key), path, found));
                path.truncate(restore);
            }
            Value::Object(mapped)
        }
        scalar @ (Value::Null | Value::Bool(_) | Value::Number(_)) => scalar.clone(),
    }
}

const CONTROL_REPLACEMENT: char = '\u{fffd}';

#[must_use]
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\t' | '\n' => character,
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                CONTROL_REPLACEMENT
            }
            _ if character.is_control() => CONTROL_REPLACEMENT,
            _ => character,
        })
        .collect()
}

#[must_use]
pub fn sanitize_line(text: &str) -> String {
    sanitize(text).replace(['\n', '\t'], " ")
}

#[cfg(test)]
mod tests;
