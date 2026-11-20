use std::{path::Path, sync::Arc};

use dekopon_broker::SecretMaterial;
use dekopon_core::{FileTier, SecretDrn, error_chain, read_trusted_file};
use dekopon_model::chatgpt::{
    ChatGptAuthStatus, ChatGptError, CredentialFile, RefreshClient, RefreshOutcome,
};
use sha2::{Digest as _, Sha256};
use tracing::Instrument as _;

use super::{REAUTHORIZATION_CODES, REFRESH_TIMEOUT, RefreshError};
use crate::socket;

const HARD_MAX_CHATGPT_AUTH_BYTES: usize = 64 * 1024;

pub(crate) struct ChatGptSubscription {
    drn: SecretDrn,
    credential: Arc<CredentialFile>,
    auth_digest: [u8; 32],
    status: ChatGptAuthStatus,
    refresh: RefreshClient,
}

impl std::fmt::Debug for ChatGptSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatGptSubscription")
            .field("drn", &self.drn)
            .finish_non_exhaustive()
    }
}

impl ChatGptSubscription {
    pub(crate) fn open(
        drn: &SecretDrn,
        auth_file: &Path,
        expected_uid: u32,
    ) -> Result<Self, String> {
        let bytes = read_trusted_file(
            auth_file,
            expected_uid,
            FileTier::Private,
            HARD_MAX_CHATGPT_AUTH_BYTES,
        );
        #[cfg(test)]
        crate::private_read_witness::record(auth_file, &bytes);
        let bytes = bytes.map_err(|source| {
            format!(
                "{drn} authFile {} is not trusted input ({}): {}",
                auth_file.display(),
                source.category(),
                error_chain(&source)
            )
        })?;
        let auth_digest = Sha256::digest(&bytes).into();
        // The auth-file parent must be writable, since a read-only parent would leave a
        // rotated-but-unsaved retired token on disk for reuse.
        socket::validate_private_parent(auth_file, expected_uid).map_err(|source| {
            format!(
                "{drn} authFile {} needs an owner-only parent directory: {}",
                auth_file.display(),
                error_chain(&source)
            )
        })?;
        let parent = auth_file.parent().ok_or_else(|| {
            format!(
                "{drn} authFile {} has no parent directory",
                auth_file.display()
            )
        })?;
        if !parent_is_owner_writable(parent) {
            return Err(format!(
                "{drn} authFile {} has a parent directory the broker cannot write; a rotated \
                 credential is persisted by renaming a sibling temporary file over the target",
                auth_file.display()
            ));
        }
        let credential = CredentialFile::from_document(auth_file, &bytes).map_err(|source| {
            format!(
                "{drn} authFile {} is not a Dekopon ChatGPT credential: {}",
                auth_file.display(),
                error_chain(&source)
            )
        })?;
        let status = credential.status().map_err(|source| {
            format!(
                "{drn} authFile {} could not be inspected: {}",
                auth_file.display(),
                error_chain(&source)
            )
        })?;
        let refresh = RefreshClient::new(REFRESH_TIMEOUT, &[]).map_err(|source| {
            format!("{drn} could not build the ChatGPT refresh client: {source}")
        })?;
        Ok(Self {
            drn: drn.clone(),
            credential: Arc::new(credential),
            auth_digest,
            status,
            refresh,
        })
    }

    pub(super) fn auth_digest(&self) -> &[u8; 32] {
        &self.auth_digest
    }

    pub(super) fn same_generation(&self, previous: &Self) -> bool {
        self.auth_digest == previous.auth_digest && self.status.path == previous.status.path
    }

    pub(super) fn log_loaded(&self) {
        tracing::info!(
            event = "broker_chatgpt_credential_loaded",
            credential = %self.drn,
            path = %self.status.path.display(),
            expires_at = self.status.expires_at,
            expired = self.status.expired,
            "broker.chatgpt.credential.loaded"
        );
    }

    pub(crate) fn file(&self) -> Arc<CredentialFile> {
        Arc::clone(&self.credential)
    }

    pub(crate) async fn resolve(&self) -> Result<SecretMaterial, RefreshError> {
        let span = tracing::info_span!(
            target: "credential",
            "broker.credential.refresh",
            credential = %self.drn,
            trigger = "use",
            secret.source.kind = "chatgpt-subscription",
            outcome = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let resolved = match self
            .credential
            .current(&self.refresh)
            .instrument(span.clone())
            .await
        {
            Ok(resolved) => resolved,
            Err(source) => {
                span.record("outcome", "failed");
                span.record("otel.status_code", "error");
                return Err(self.classify(&source));
            }
        };
        span.record(
            "outcome",
            resolved.refresh.map_or("current", RefreshOutcome::label),
        );
        Ok(
            SecretMaterial::new(resolved.access.expose().as_bytes().to_vec())
                .with_chatgpt_account(dekopon_core::Redacted::new(resolved.account_id)),
        )
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "ChatGptError is foreign; every variant not named is a permanent local fault"
    )]
    fn classify(&self, error: &ChatGptError) -> RefreshError {
        let (permanent, category) = match error {
            ChatGptError::TokenRefused { status, code, .. } => {
                if code
                    .as_deref()
                    .is_some_and(|code| REAUTHORIZATION_CODES.contains(&code))
                {
                    (true, "reauthorization-required")
                } else if (500..600).contains(status) {
                    (false, "token-endpoint-unavailable")
                } else {
                    (false, "token-endpoint-rejected")
                }
            }
            ChatGptError::RefreshTask(_) => (false, "refresh-task"),
            ChatGptError::Request(_) => (false, "transport"),
            ChatGptError::Protocol(_) => (false, "token-endpoint-protocol"),
            _ => (true, "credential-unusable"),
        };
        if permanent {
            tracing::error!(
                event = "broker_chatgpt_credential_reauth_required",
                credential = %self.drn,
                path = %self.credential.path().display(),
                category = category,
                error = %error_chain(error),
                remedy = "dekopon-gatewayd auth chatgpt login --auth-file",
                "broker.chatgpt.credential.reauth.required"
            );
            RefreshError::ReauthorizationRequired
        } else {
            tracing::warn!(
                event = "broker_chatgpt_credential_refresh_failed",
                credential = %self.drn,
                category = category,
                error = %error_chain(error),
                "broker.chatgpt.credential.refresh.failed"
            );
            RefreshError::Unavailable { category }
        }
    }
}

fn parent_is_owner_writable(parent: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::symlink_metadata(parent)
        .is_ok_and(|metadata| metadata.permissions().mode() & 0o200 != 0)
}
