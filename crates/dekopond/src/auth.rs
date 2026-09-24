use std::{
    collections::BTreeMap,
    io::{self, IsTerminal as _},
    path::Path,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_core::Redacted;
use dekopon_model::chatgpt::{self, ChatGptCredentialExport, ChatGptError};
use serde::Serialize;
use thiserror::Error;

use crate::{
    auth_result::{CommandResult, ModelAuthStatus},
    cli::{AuthCommand, ChatGptAuthCommand, ExportFormat},
};

const SECRET_KEY: &str = "chatgpt-auth.json";

const MANIFEST_HEADER: &str = "\
# Exported by `dekopond auth chatgpt export`. This manifest carries a live ChatGPT access token and
# a rotating refresh token; base64 here is Kubernetes' encoding for `data`, not encryption.
#
# The refresh token rotates: whichever process refreshes next invalidates this copy. Seed it once
# into a writable directory, never overwrite a newer credential file with it, and re-export after
# a deliberate rotation.
";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(transparent)]
    ChatGpt(#[from] ChatGptError),
    #[error("refusing to print ChatGPT credentials without --expose-credential")]
    ExposeNotAcknowledged,
    /// Standard output is a terminal, which keeps the credential after the command exits.
    #[error(
        "refusing to print ChatGPT credentials to a terminal; redirect or pipe the output (for example `| kubectl apply -f -`), or pass --allow-terminal to accept a live refresh token in your scrollback"
    )]
    TerminalDestination,
    #[error("could not render the ChatGPT credential Secret manifest")]
    Manifest(#[source] serde_yaml::Error),
}

pub(crate) fn execute(account: &AuthCommand) -> Result<CommandResult, AuthError> {
    match account {
        AuthCommand::ChatGpt { command } => execute_chatgpt(command),
    }
}

fn execute_chatgpt(command: &ChatGptAuthCommand) -> Result<CommandResult, AuthError> {
    let status = match command {
        ChatGptAuthCommand::Login { auth_file } => {
            let stderr = io::stderr();
            let mut output = stderr.lock();
            chatgpt::login_with_output(auth_file.as_deref(), &mut output)?;
            chatgpt::status(auth_file.as_deref())?
        }
        ChatGptAuthCommand::Status { auth_file } => chatgpt::status(auth_file.as_deref())?,
        ChatGptAuthCommand::Logout { auth_file } => {
            chatgpt::logout(auth_file.as_deref())?;
            chatgpt::status(auth_file.as_deref())?
        }
        ChatGptAuthCommand::Export {
            auth_file,
            format,
            secret_name,
            namespace,
            expose_credential,
            allow_terminal,
        } => {
            return export(
                auth_file.as_deref(),
                *format,
                secret_name,
                namespace.as_deref(),
                *expose_credential,
                *allow_terminal,
            );
        }
    };

    Ok(CommandResult::Auth(ModelAuthStatus::chatgpt(status)))
}

/// Both gates are checked before the credential file is opened, so a refused export never even
/// reads the secret.
fn export(
    auth_file: Option<&Path>,
    format: ExportFormat,
    secret_name: &str,
    namespace: Option<&str>,
    expose_credential: bool,
    allow_terminal: bool,
) -> Result<CommandResult, AuthError> {
    if !expose_credential {
        return Err(AuthError::ExposeNotAcknowledged);
    }
    guard_destination(io::stdout().is_terminal(), allow_terminal)?;

    let export = chatgpt::export_credentials(auth_file)?;
    warn_about_the_exported_copy(export.path());

    let document = match format {
        ExportFormat::Raw => export.expose_document().to_owned(),
        ExportFormat::Secret => secret_manifest(&export, secret_name, namespace)?,
    };

    Ok(CommandResult::CredentialExport(Redacted::new(document)))
}

/// The required flag covers intent; this gate covers destination, since even a genuine export
/// should not leave a live token in terminal scrollback, a tmux capture, or a screen share.
fn guard_destination(is_terminal: bool, allow_terminal: bool) -> Result<(), AuthError> {
    if is_terminal && !allow_terminal {
        return Err(AuthError::TerminalDestination);
    }
    Ok(())
}

fn warn_about_the_exported_copy(path: &Path) {
    tracing::warn!(
        credential_file = %path.display(),
        "exported ChatGPT credential material in the clear: a live access token and a rotating refresh token"
    );
    tracing::warn!(
        "the refresh token rotates, so this copy is stale the moment the live credential refreshes; seed it once into a writable directory, never overwrite a newer credential file with it, and re-export after a deliberate rotation"
    );
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SecretManifest<'a> {
    api_version: &'static str,
    kind: &'static str,
    metadata: SecretMetadata<'a>,
    #[serde(rename = "type")]
    secret_type: &'static str,
    data: BTreeMap<&'static str, String>,
}

#[derive(Serialize)]
struct SecretMetadata<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<&'a str>,
    labels: BTreeMap<&'static str, &'static str>,
}

fn secret_manifest(
    export: &ChatGptCredentialExport,
    name: &str,
    namespace: Option<&str>,
) -> Result<String, AuthError> {
    let manifest = SecretManifest {
        api_version: "v1",
        kind: "Secret",
        metadata: SecretMetadata {
            name,
            namespace,
            labels: BTreeMap::from([
                ("app.kubernetes.io/component", "chatgpt-credential"),
                ("app.kubernetes.io/managed-by", "dekopon-auth-export"),
                ("app.kubernetes.io/name", "dekopon"),
            ]),
        },
        secret_type: "Opaque",
        data: BTreeMap::from([(SECRET_KEY, STANDARD.encode(export.expose_document()))]),
    };

    serde_yaml::to_string(&manifest)
        .map(|document| format!("{MANIFEST_HEADER}{document}"))
        .map_err(AuthError::Manifest)
}

#[cfg(test)]
mod tests {
    use super::{AuthError, guard_destination};

    #[test]
    fn a_redirected_destination_is_accepted() {
        assert!(guard_destination(false, false).is_ok());
    }

    #[test]
    fn a_terminal_destination_is_refused_by_default() {
        let error = guard_destination(true, false).expect_err("a terminal must be refused");

        assert!(matches!(error, AuthError::TerminalDestination));
        assert!(error.to_string().contains("--allow-terminal"));
    }

    #[test]
    fn a_terminal_destination_is_allowed_explicitly() {
        assert!(guard_destination(true, true).is_ok());
    }
    #[test]
    fn constructed_export_requires_acknowledgement_before_reading() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("missing.json");
        let error = super::export(
            Some(&missing),
            crate::cli::ExportFormat::Raw,
            "dekopon-chatgpt-auth",
            None,
            false,
            true,
        )
        .expect_err("acknowledgement required");
        assert!(matches!(error, AuthError::ExposeNotAcknowledged));
        assert!(!missing.exists());
    }
}
