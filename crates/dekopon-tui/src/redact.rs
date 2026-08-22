//! Rendering-time redaction and terminal-control sanitisation.
//!
//! Two different jobs, deliberately in one module because every string this console draws needs
//! both and forgetting either is the same class of mistake.
//!
//! **Redaction.** Provider credentials never reach this process: the broker resolves a symbolic
//! `credential:` name from its owner-only credentials file and injects the value inside its native
//! HTTP engine, after guest-header validation. The model, the script, the component, and the
//! invocation input all see nothing. So this is not credential redaction — it is a guard against
//! the secrets a *model* writes or a *provider* returns: an `Authorization` header a script
//! assembled by hand, a token pasted into a turn, a row in a query result.
//!
//! **Sanitisation.** A pull-request title and an issue body are attacker-controlled text arriving
//! through a read-only capability. Drawn raw into a terminal they can move the cursor, repaint
//! earlier lines, or set a scroll region — so every borrowed string is stripped before it reaches a
//! buffer, exactly as `dekopon`'s table renderer already strips its cells.

use dekopon_core::redaction_marker;
use serde_json::{Map, Value};

/// One array index, rendered without a fallible formatter.
fn itoa(index: usize) -> String {
    index.to_string()
}

/// Object keys whose value is a secret whatever it looks like.
///
/// Matched case-insensitively against the whole key, after `-`/`_` are folded out, so `api_key`,
/// `apiKey`, and `API-KEY` are one entry rather than three that can drift apart.
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

/// Shortest run this module will call a credential.
///
/// Below it the shapes stop being distinctive — `sk-` followed by four characters is as likely to
/// be a SKU as a key — and a redactor that fires on ordinary text teaches an operator to stop
/// reading its markers.
const MIN_SECRET_LENGTH: usize = 20;

/// Whether one object key names a secret.
fn key_is_secret(key: &str) -> bool {
    let folded: String = key
        .chars()
        .filter(|character| *character != '-' && *character != '_')
        .flat_map(char::to_lowercase)
        .collect();
    SECRET_KEYS.contains(&folded.as_str())
}

/// Whether a run of characters could be the body of a credential.
fn is_secret_body(value: &str) -> bool {
    value.len() >= MIN_SECRET_LENGTH
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

/// Characters a credential is made of, and therefore what bounds one inside a longer string.
///
/// `.`, `+`, `/`, and `=` are in the set because a JWT and a base64 blob need them, which is also
/// why an identifier like `gh.pull-request.read` arrives here as one run rather than three — and
/// why the JWT rule below insists on three *long* segments, so ordinary dotted names do not match.
fn is_token_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '+' | '/' | '=')
}

/// Whether one run of token characters is a credential on its own.
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

/// Whether a scheme word means the run after it is the credential.
fn scheme_introduces_secret(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "bearer" | "basic" | "token"
    )
}

/// Replaces every credential inside one string, or answers `None` when there was none.
///
/// Run-level rather than whole-value: a provider result is very often a sentence with a token in
/// it, and replacing the sentence would hide the part an operator needed to read. The marker lands
/// exactly where the secret was, so the shape of the text survives its removal.
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

/// One redaction applied while rendering, retained so a reveal can undo exactly that one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redaction {
    /// Dotted path to the redacted scalar, such as `headers.authorization` or `rows.0.token`.
    pub path: String,
    /// Why it was redacted, so the pane can say which rule fired.
    pub reason: RedactionReason,
}

/// Which rule redacted a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedactionReason {
    /// The key named a secret.
    Key,
    /// The value had the shape of one.
    Shape,
}

/// A redacted rendering of one JSON value, plus what was hidden to produce it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Redacted {
    /// The value with every secret scalar replaced by its marker.
    pub value: Value,
    /// Every redaction applied, in document order.
    pub redactions: Vec<Redaction>,
}

impl Redacted {
    /// Whether anything was hidden.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.redactions.is_empty()
    }
}

/// Redacts every secret scalar in one JSON value.
///
/// Every redaction is reported rather than only the first: an operator reading a tool call needs to
/// know the payload carried three secrets, not that it carried at least one.
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
        other if key_says_secret && !other.is_null() => {
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
        scalar => scalar.clone(),
    }
}

/// Replacement drawn where a control character was removed.
///
/// Visible rather than silent: text that was tampered with should look tampered with, and a line
/// that quietly loses characters is one an operator compares against the real thing and misreads.
const CONTROL_REPLACEMENT: char = '\u{fffd}';

/// Strips terminal control sequences from borrowed text before it reaches a buffer.
///
/// Tabs and newlines survive — a renderer decides what to do with those — while every other
/// control character, the whole C1 range, and the bidirectional overrides become one replacement
/// character each. The C1 range matters because a lone `0x9b` is a control sequence introducer on
/// terminals that accept eight-bit controls, and the overrides matter because they can reorder a
/// rendered line without changing a byte of its content.
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

/// Sanitises and collapses text into one displayable line.
#[must_use]
pub fn sanitize_line(text: &str) -> String {
    sanitize(text).replace(['\n', '\t'], " ")
}

#[cfg(test)]
mod tests;
