use std::{borrow::Cow, fmt};

use serde::{Deserialize, Deserializer, Serialize};

use crate::attribute::bounded_text;

pub const MAX_FAILURE_CODE_BYTES: usize = 128;

pub const MAX_FAILURE_MESSAGE_BYTES: usize = 1024;

/// Provider-authored and untrusted; bounded on both construction and decode so no peer can inflate
/// it, and it carries no authority over the broker's classification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderFailureDetail {
    #[serde(deserialize_with = "deserialize_code")]
    pub code: String,
    #[serde(deserialize_with = "deserialize_message")]
    pub message: String,
}

impl ProviderFailureDetail {
    #[must_use]
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: bounded_text(code, MAX_FAILURE_CODE_BYTES).into_owned(),
            message: bounded_text(message, MAX_FAILURE_MESSAGE_BYTES).into_owned(),
        }
    }
}

impl fmt::Display for ProviderFailureDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

fn deserialize_code<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    bound_on_decode(deserializer, MAX_FAILURE_CODE_BYTES)
}

fn deserialize_message<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    bound_on_decode(deserializer, MAX_FAILURE_MESSAGE_BYTES)
}

/// Cuts rather than refuses: rejecting a message one byte over the bound would cost the caller the
/// only record that the invocation failed at all.
fn bound_on_decode<'de, D>(deserializer: D, maximum: usize) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Ok(match bounded_text(&value, maximum) {
        Cow::Borrowed(_) => value,
        Cow::Owned(cut) => cut,
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_FAILURE_CODE_BYTES, MAX_FAILURE_MESSAGE_BYTES, ProviderFailureDetail};

    #[test]
    fn a_code_and_message_within_their_bounds_are_carried_verbatim() {
        let detail = ProviderFailureDetail::new(
            "upstream-rejected",
            "the image route refused the request with HTTP 400 (moderation_blocked)",
        );

        assert_eq!(detail.code, "upstream-rejected");
        assert_eq!(
            detail.message,
            "the image route refused the request with HTTP 400 (moderation_blocked)"
        );
        assert_eq!(
            detail.to_string(),
            "upstream-rejected: the image route refused the request with HTTP 400 \
             (moderation_blocked)"
        );
    }

    #[test]
    fn an_oversized_code_and_message_are_each_cut_at_their_own_bound() {
        let detail = ProviderFailureDetail::new(
            &"c".repeat(MAX_FAILURE_CODE_BYTES + 1),
            &"m".repeat(MAX_FAILURE_MESSAGE_BYTES + 1),
        );

        assert_eq!(
            detail.code,
            format!("{}\u{2026}[truncated]", "c".repeat(MAX_FAILURE_CODE_BYTES))
        );
        assert_eq!(
            detail.message,
            format!(
                "{}\u{2026}[truncated]",
                "m".repeat(MAX_FAILURE_MESSAGE_BYTES)
            )
        );
    }

    #[test]
    fn a_peer_supplied_message_over_the_bound_is_cut_on_decode() {
        let document = serde_json::json!({
            "code": "upstream-rejected",
            "message": "m".repeat(MAX_FAILURE_MESSAGE_BYTES + 64),
        })
        .to_string();

        let detail = serde_json::from_str::<ProviderFailureDetail>(&document)
            .expect("an oversized message is cut rather than refused");

        assert_eq!(
            detail.message,
            format!(
                "{}\u{2026}[truncated]",
                "m".repeat(MAX_FAILURE_MESSAGE_BYTES)
            )
        );
    }

    #[test]
    fn the_wire_names_are_code_and_message_and_nothing_else_decodes() {
        let detail = ProviderFailureDetail::new("upstream-rejected", "refused");

        let document = serde_json::to_string(&detail).expect("the pair serializes");

        assert_eq!(
            document,
            r#"{"code":"upstream-rejected","message":"refused"}"#
        );
        assert!(
            serde_json::from_str::<ProviderFailureDetail>(
                r#"{"code":"c","message":"m","detail":"x"}"#
            )
            .is_err(),
            "an unknown field is refused rather than ignored"
        );
    }
}
