use dekopon_core::Redacted;

use crate::{
    error::ProtocolFailure,
    model::{strip_controls, truncate_diagnostic},
};

/// Borrow both credentials across Codex's one resend; never copy or format the secrets.
#[derive(Clone, Copy, Default)]
pub(crate) struct DiagnosticSecrets<'a> {
    current: Option<&'a Redacted<String>>,
    previous: Option<&'a Redacted<String>>,
}

impl<'a> DiagnosticSecrets<'a> {
    pub(crate) fn new(current: Option<&'a Redacted<String>>) -> Self {
        Self {
            current,
            previous: None,
        }
    }

    pub(crate) fn with_previous(mut self, previous: &'a Redacted<String>) -> Self {
        self.previous = Some(previous);
        self
    }

    /// In-scope credentials, longest first: a rotated token can extend the old one.
    fn tokens(self) -> impl Iterator<Item = &'a str> {
        let mut tokens = [self.current, self.previous].map(|token| {
            token
                .map(|token| token.expose().as_str())
                .filter(|token| !token.is_empty())
        });
        tokens.sort_by_key(|token| std::cmp::Reverse(token.map_or(0, str::len)));
        tokens.into_iter().flatten()
    }

    pub(crate) fn sanitize(self, value: &str) -> String {
        let mut text = strip_controls(value);
        for token in self.tokens() {
            text = text.replace(token, "[REDACTED]");
        }
        truncate_diagnostic(text)
    }

    /// Sanitizes a body the reader cut short, which may end inside a credential.
    pub(crate) fn sanitize_truncated(self, bytes: &[u8]) -> String {
        let stripped: Vec<u8> = bytes
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_control() || matches!(byte, b'\n' | b'\t'))
            .collect();
        let bytes = stripped.as_slice();
        let partial = self
            .tokens()
            .flat_map(|token| {
                (1..token.len()).filter(move |&end| bytes.ends_with(&token.as_bytes()[..end]))
            })
            .max()
            .unwrap_or(0);
        self.sanitize(&String::from_utf8_lossy(&bytes[..bytes.len() - partial]))
    }

    pub(crate) fn decode_failure(self, source: serde_json::Error) -> ProtocolFailure {
        ProtocolFailure::Decode(self.json_error(source))
    }

    fn json_error(self, source: serde_json::Error) -> serde_json::Error {
        let shown = source.to_string();
        let sanitized = self.sanitize(&shown);
        if sanitized == shown {
            return source;
        }
        // Serde error sources may quote upstream values; keeping the source would re-expose the
        // token through Debug or source() even if the outer message is redacted.
        <serde_json::Error as serde::de::Error>::custom(sanitized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilding_a_serde_source_does_not_reconstruct_the_other_credential() {
        let current = Redacted::new("synthetic-current".to_owned());
        let previous = Redacted::new("synthetic-previous".to_owned());
        let source = <serde_json::Error as serde::de::Error>::custom(
            "synthetic-previous synthetic-\u{0000}current",
        );
        let error = DiagnosticSecrets::new(Some(&current))
            .with_previous(&previous)
            .decode_failure(source);
        assert!(matches!(error, ProtocolFailure::Decode(_)));
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains(current.expose()));
        assert!(!shown.contains(previous.expose()));
    }

    #[test]
    fn stripping_controls_precedes_redaction() {
        let token = Redacted::new("synthetic-secret".to_owned());
        assert_eq!(
            DiagnosticSecrets::new(Some(&token)).sanitize("a synthetic-\u{001b}secret\r\n\tb"),
            "a [REDACTED]\n\tb"
        );
    }

    #[test]
    fn a_pretty_printed_provider_error_keeps_its_message() {
        let token = Redacted::new("synthetic-secret".to_owned());
        let body =
            "{\r\n  \"error\": {\r\n    \"message\": \"bad key synthetic-secret\"\r\n  }\r\n}";
        assert_eq!(
            DiagnosticSecrets::new(Some(&token)).sanitize(body),
            "{\n  \"error\": {\n    \"message\": \"bad key [REDACTED]\"\n  }\n}"
        );
    }

    #[test]
    fn a_truncated_body_drops_a_trailing_partial_credential() {
        let token = Redacted::new("synthetic-été".to_owned());
        let secrets = DiagnosticSecrets::new(Some(&token));
        let bytes = format!("refused: {}", token.expose()).into_bytes();
        for end in 9..bytes.len() {
            assert_eq!(secrets.sanitize_truncated(&bytes[..end]), "refused: ");
        }
        assert_eq!(secrets.sanitize_truncated(&bytes), "refused: [REDACTED]");
        assert_eq!(
            secrets.sanitize_truncated(b"refused: synth\x00etic-"),
            "refused: "
        );
    }

    #[test]
    fn diagnostics_are_capped_at_a_character_boundary() {
        let max = crate::model::MAX_ERROR_BODY_BYTES as usize;
        let unicode = format!("{}é", "x".repeat(max - 1));
        assert_eq!(
            DiagnosticSecrets::default().sanitize(&unicode).len(),
            max - 1
        );
    }

    #[test]
    fn overlapping_current_and_previous_credentials_are_removed_before_truncation() {
        let previous = Redacted::new("synthetic-secret".to_owned());
        let current = Redacted::new("synthetic-secret-rotated".to_owned());
        let secrets = DiagnosticSecrets::new(Some(&current)).with_previous(&previous);
        assert_eq!(
            secrets.sanitize("synthetic-secret-rotated / synthetic-secret"),
            "[REDACTED] / [REDACTED]"
        );
        let padding = "x".repeat(crate::model::MAX_ERROR_BODY_BYTES as usize - 3);
        let text = secrets.sanitize(&format!("{padding}synthetic-secret-rotated"));
        assert!(!text.contains("syn"));
        assert_eq!(text.len(), crate::model::MAX_ERROR_BODY_BYTES as usize);
    }
}
