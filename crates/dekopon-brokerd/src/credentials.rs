//! Owner-only provider credential storage, resolved at startup into a [`CredentialStore`].
//!
//! This file is the only place a provider secret exists at rest, and this module is the only code
//! that reads it. Values deserialize directly into [`Redacted`] and travel to the native HTTP
//! boundary as [`BoundCredential`]s — policy rules refer to entries by symbolic name, so the main
//! broker configuration stays shareable and the secret never enters policy, audit, evidence,
//! telemetry, or the wire protocol.
//!
//! Hygiene is stricter than the main configuration's: where `broker.yaml` rejects group/world
//! *writability*, a credentials file rejects group/world *anything* (`mode & 0o077`), because
//! readability is the whole threat.

use std::path::{Path, PathBuf};

use dekopon_broker::{BrokerBuildError, CredentialStore};
use dekopon_broker_host::BoundCredential;
use dekopon_core::Redacted;
use dekopon_http_host::ConfigurationError;
use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

/// Strict `apiVersion` accepted by the credentials file.
pub const CREDENTIALS_API_VERSION: &str = "dekopon.dev/broker-credentials/v1alpha1";
/// Hard ceiling on the credentials file size.
pub const HARD_MAX_CREDENTIALS_BYTES: usize = 1024 * 1024;
/// Hard ceiling on distinct credential entries.
pub const HARD_MAX_CREDENTIALS: usize = 64;
const MAX_CREDENTIAL_NAME_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum CredentialsApiVersion {
    #[serde(rename = "dekopon.dev/broker-credentials/v1alpha1")]
    V1Alpha1,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CredentialsFile {
    #[allow(
        dead_code,
        reason = "the version field exists to be strictly validated"
    )]
    api_version: CredentialsApiVersion,
    credentials: Vec<CredentialEntry>,
}

/// One named secret and its explicit destination binding.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CredentialEntry {
    /// Symbolic name policy rules bind with `credential:`.
    name: String,
    /// Credential mechanism; `bearerToken` renders `authorization: <scheme> <secret>`.
    kind: CredentialKind,
    /// Header scheme, typically `Bearer` or `token`.
    scheme: String,
    /// Authorities this secret may be presented to, in `allowedHosts` grammar.
    destinations: Vec<String>,
    /// The secret value. Deserializes straight into `Redacted`; no plain `String` field ever
    /// holds it, so `Debug` on this entry renders a marker.
    secret: Redacted<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
enum CredentialKind {
    BearerToken,
}

/// Loads and validates the credentials file into a resolved store.
pub(crate) async fn load(
    path: &Path,
    expected_uid: u32,
) -> Result<CredentialStore, CredentialsError> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .await
        .map_err(|source| CredentialsError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let metadata = file
        .metadata()
        .await
        .map_err(|source| CredentialsError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(CredentialsError::NotRegular {
            path: path.to_path_buf(),
        });
    }
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(CredentialsError::InsecureFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > HARD_MAX_CREDENTIALS_BYTES as u64 {
        return Err(CredentialsError::TooLarge {
            length: metadata.len(),
            maximum: HARD_MAX_CREDENTIALS_BYTES,
        });
    }
    let mut bytes = Vec::new();
    file.take((HARD_MAX_CREDENTIALS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| CredentialsError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > HARD_MAX_CREDENTIALS_BYTES {
        return Err(CredentialsError::TooLarge {
            length: bytes.len() as u64,
            maximum: HARD_MAX_CREDENTIALS_BYTES,
        });
    }
    let parsed = serde_yaml::from_slice::<CredentialsFile>(&bytes)
        .map_err(|source| CredentialsError::Decode { source })?;
    resolve(parsed)
}

fn resolve(file: CredentialsFile) -> Result<CredentialStore, CredentialsError> {
    if file.credentials.len() > HARD_MAX_CREDENTIALS {
        return Err(CredentialsError::TooMany {
            maximum: HARD_MAX_CREDENTIALS,
        });
    }
    let mut entries = Vec::with_capacity(file.credentials.len());
    for entry in file.credentials {
        if entry.name.is_empty()
            || entry.name.len() > MAX_CREDENTIAL_NAME_BYTES
            || entry.name.trim() != entry.name
            || entry
                .name
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(CredentialsError::InvalidName { name: entry.name });
        }
        let CredentialKind::BearerToken = entry.kind;
        let credential = BoundCredential::bearer(&entry.scheme, entry.secret, entry.destinations)
            .map_err(|source| CredentialsError::InvalidCredential {
            name: entry.name.clone(),
            source,
        })?;
        entries.push((entry.name, credential));
    }
    CredentialStore::new(entries).map_err(|source| CredentialsError::Store { source })
}

/// Credential storage that could not be trusted or decoded.
#[derive(Debug, Error)]
pub enum CredentialsError {
    #[error("could not read broker credentials at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("broker credentials path is not a regular non-symlink file: {path}")]
    NotRegular { path: PathBuf },
    #[error(
        "broker credentials must be single-link, owned by the server UID, and unreadable by group and world: {path}"
    )]
    InsecureFile { path: PathBuf },
    #[error("broker credentials are {length} bytes; maximum is {maximum}")]
    TooLarge { length: u64, maximum: usize },
    #[error("broker credentials are not strict valid YAML/JSON")]
    Decode {
        #[source]
        source: serde_yaml::Error,
    },
    #[error("broker credentials name too many entries; maximum is {maximum}")]
    TooMany { maximum: usize },
    #[error("broker credential name {name:?} is empty, oversized, or contains whitespace")]
    InvalidName { name: String },
    #[error("broker credential {name:?} is structurally invalid")]
    InvalidCredential {
        name: String,
        #[source]
        source: ConfigurationError,
    },
    #[error("broker credential store could not be built")]
    Store {
        #[source]
        source: BrokerBuildError,
    },
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{CredentialsError, load};

    const VALID: &str = "\
apiVersion: dekopon.dev/broker-credentials/v1alpha1
credentials:
  - name: github-pat-fixture
    kind: bearerToken
    scheme: Bearer
    destinations: [api.github.com]
    secret: fixture-secret-value
";

    async fn write_credentials(directory: &std::path::Path, contents: &str, mode: u32) {
        let path = directory.join("credentials.yaml");
        tokio::fs::write(&path, contents).await.expect("write file");
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .await
            .expect("set mode");
    }

    fn uid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    #[tokio::test]
    async fn loads_a_strict_owner_only_credentials_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        write_credentials(directory.path(), VALID, 0o600).await;

        load(&directory.path().join("credentials.yaml"), uid())
            .await
            .expect("valid credentials load");
    }

    #[tokio::test]
    async fn rejects_group_or_world_readable_files() {
        // Stricter than broker.yaml on purpose: for a secret, readability is the threat.
        let directory = tempfile::tempdir().expect("temporary directory");
        write_credentials(directory.path(), VALID, 0o640).await;

        let error = load(&directory.path().join("credentials.yaml"), uid())
            .await
            .expect_err("readable credentials must be refused");
        assert!(matches!(error, CredentialsError::InsecureFile { .. }));
    }

    #[tokio::test]
    async fn rejects_unknown_fields_and_versions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for contents in [
            VALID.replace("v1alpha1", "v2"),
            VALID.replace("scheme: Bearer", "scheme: Bearer\n    extra: field"),
            VALID.replace("kind: bearerToken", "kind: password"),
        ] {
            write_credentials(directory.path(), &contents, 0o600).await;
            let error = load(&directory.path().join("credentials.yaml"), uid())
                .await
                .expect_err("strict decoding must refuse");
            assert!(matches!(error, CredentialsError::Decode { .. }));
        }
    }

    #[tokio::test]
    async fn rejects_structural_credential_problems_without_echoing_secrets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        write_credentials(
            directory.path(),
            &VALID.replace("destinations: [api.github.com]", "destinations: []"),
            0o600,
        )
        .await;

        let error = load(&directory.path().join("credentials.yaml"), uid())
            .await
            .expect_err("empty destinations must be refused");
        let rendered = format!("{error:?} {error}");
        assert!(matches!(error, CredentialsError::InvalidCredential { .. }));
        assert!(!rendered.contains("fixture-secret-value"), "{rendered}");
    }

    #[tokio::test]
    async fn rejects_duplicate_names() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let duplicated = format!(
            "{VALID}  - name: github-pat-fixture\n    kind: bearerToken\n    scheme: Bearer\n    destinations: [api.github.com]\n    secret: another\n"
        );
        write_credentials(directory.path(), &duplicated, 0o600).await;

        let error = load(&directory.path().join("credentials.yaml"), uid())
            .await
            .expect_err("duplicate names must be refused");
        assert!(matches!(error, CredentialsError::Store { .. }));
    }
}
