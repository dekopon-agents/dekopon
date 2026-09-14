//! The failure a provider reported for itself, carried beside the broker's classification.
//!
//! The broker classifies every host failure into one stable, low-cardinality string an operator and
//! a model can act on — `provider-failure`, `provider-timeout`, `storage-quota`. That string says
//! which *class* of thing went wrong and deliberately says nothing about the particular refusal, so
//! on its own it cannot tell an upstream moderation refusal from a bad argument. The provider
//! already wrote both: `ComponentResponse::Failed` carries the component's own `code` and
//! `message`. This pair is that answer, bounded, travelling with the classification rather than
//! instead of it.

use std::{borrow::Cow, fmt};

use serde::{Deserialize, Deserializer, Serialize};

use crate::attribute::bounded_text;

/// Maximum bytes of a provider-reported failure code carried past the broker boundary.
pub const MAX_FAILURE_CODE_BYTES: usize = 128;

/// Maximum bytes of a provider-reported failure message carried past the broker boundary.
///
/// Tighter than [`MAX_ATTRIBUTE_BYTES`](crate::MAX_ATTRIBUTE_BYTES): a span attribute is read by an
/// operator, while this rides every failed invocation result, every terminal audit record, and the
/// script output a model reads back.
pub const MAX_FAILURE_MESSAGE_BYTES: usize = 1024;

/// One provider's own failure code and message.
///
/// Provider-authored and therefore untrusted text. It is bounded on construction and again on
/// decode, so no peer can make it larger by writing it itself, and it carries no authority: the
/// broker's classification stays the field a caller branches on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderFailureDetail {
    /// The code the provider chose, cut at [`MAX_FAILURE_CODE_BYTES`].
    #[serde(deserialize_with = "deserialize_code")]
    pub code: String,
    /// The message the provider wrote, cut at [`MAX_FAILURE_MESSAGE_BYTES`].
    #[serde(deserialize_with = "deserialize_message")]
    pub message: String,
}

impl ProviderFailureDetail {
    /// Bounds one provider-reported code and message into the pair that travels.
    #[must_use]
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: bounded_text(code, MAX_FAILURE_CODE_BYTES).into_owned(),
            message: bounded_text(message, MAX_FAILURE_MESSAGE_BYTES).into_owned(),
        }
    }
}

/// Renders `code: message`, the one spelling every surface that shows the pair uses.
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

/// Cuts rather than refuses: a message one byte over its bound must not cost a caller the whole
/// terminal result, which is the only record saying the invocation failed at all.
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

    /// The bound is a property of the type, not of the one process that happens to construct it:
    /// a peer that writes its own oversized pair gets it cut on the way in rather than accepted.
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
