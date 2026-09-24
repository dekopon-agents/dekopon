use dekopon_core::Redacted;
use dekopon_model::chatgpt::ChatGptAuthStatus;
use serde::Serialize;
#[derive(Clone, Debug)]
pub(crate) enum CommandResult {
    Auth(ModelAuthStatus),
    CredentialExport(Redacted<String>),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuthStatus {
    pub account: &'static str,
    pub credential_file: String,
    pub signed_in: bool,
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
