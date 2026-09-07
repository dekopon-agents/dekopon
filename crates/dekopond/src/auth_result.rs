//! Auth-only typed results; exported material stays redacted until rendering for the writer.
use dekopon_core::Redacted;
use dekopon_model::chatgpt::ChatGptAuthStatus;
use serde::Serialize;
#[derive(Clone, Debug)]
pub(crate) enum CommandResult {
    Auth(ModelAuthStatus),
    CredentialExport(Redacted<String>),
}
/// Machine-readable model-account authentication state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuthStatus {
    /// Model account namespace.
    pub account: &'static str,
    /// Credential file owned by Dekopon.
    pub credential_file: String,
    /// Whether credentials are present.
    pub signed_in: bool,
    /// Whether the current access token is expired.
    pub expired: bool,
}

impl ModelAuthStatus {
    pub(crate) fn chatgpt(status: ChatGptAuthStatus) -> Self {
        Self {
            account: "chatgpt",
            credential_file: status.path.display().to_string(),
            signed_in: status.signed_in,
            expired: status.expired,
        }
    }
}
