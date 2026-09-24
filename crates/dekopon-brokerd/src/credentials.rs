//! A credentials file rejects any group/world permission bit, not just writability, since
//! readability alone is the threat; a failed rotation because of a read-only parent directory would
//! leave the retired token on disk for reuse.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use dekopon_broker::{
    BrokerBuildError, CredentialRefreshError, CredentialStore, RefreshingCredential,
    StoredCredential,
};
use dekopon_broker_host::BoundCredential;
use dekopon_core::{FileHygieneError, FileTier, Redacted, error_chain, read_trusted_file};
use dekopon_http_host::ConfigurationError;
use dekopon_model::chatgpt::{ChatGptError, CredentialFile, RefreshOutcome};
use serde::Deserialize;
use thiserror::Error;

use crate::socket;

pub const CREDENTIALS_API_VERSION: &str = "dekopon.dev/broker-credentials/v1alpha1";
pub const HARD_MAX_CREDENTIALS_BYTES: usize = 1024 * 1024;
pub const HARD_MAX_CREDENTIALS: usize = 64;
/// 64 KiB is far larger than the five-field JWT document needs, but it still bounds what a
/// misconfigured path could make the broker read at startup.
pub const HARD_MAX_CHATGPT_AUTH_BYTES: usize = 64 * 1024;
const MAX_CREDENTIAL_NAME_BYTES: usize = 128;

const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Anything not explicitly listed here is treated as transient by default, since classifying an
/// outage as permanent would take a capability out of service until an operator noticed.
const REAUTHORIZATION_CODES: [&str; 4] = [
    "invalid_grant",
    "refresh_token_reused",
    "refresh_token_invalidated",
    "refresh_token_expired",
];

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CredentialEntry {
    name: String,
    kind: CredentialKind,
    #[serde(default)]
    scheme: Option<String>,
    destinations: Vec<String>,
    /// The secret value deserializes directly into Redacted, never through a plain String field, so
    /// a derived Debug on this entry can't accidentally print it.
    #[serde(default)]
    secret: Option<Redacted<String>>,
    #[serde(default)]
    auth_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
enum CredentialKind {
    BearerToken,
    ChatgptSubscription,
}

impl CredentialKind {
    const fn label(self) -> &'static str {
        match self {
            Self::BearerToken => "bearerToken",
            Self::ChatgptSubscription => "chatgptSubscription",
        }
    }
}

pub(crate) async fn load(
    path: &Path,
    expected_uid: u32,
) -> Result<CredentialStore, CredentialsError> {
    let owned = path.to_path_buf();
    let bytes = tokio::task::spawn_blocking(move || {
        read_trusted_file(
            &owned,
            expected_uid,
            FileTier::Private,
            HARD_MAX_CREDENTIALS_BYTES,
        )
    })
    .await
    .map_err(|join| CredentialsError::Read {
        path: path.to_path_buf(),
        source: std::io::Error::other(join),
    })?
    .map_err(|error| match error {
        FileHygieneError::NotRegular { path, .. } => CredentialsError::NotRegular { path },
        FileHygieneError::TooLarge {
            length, maximum, ..
        } => CredentialsError::TooLarge { length, maximum },
        FileHygieneError::Io { path, source } => CredentialsError::Read { path, source },
        insecure => CredentialsError::InsecureFile {
            path: path.to_path_buf(),
            source: insecure,
        },
    })?;
    let parsed = serde_yaml::from_slice::<CredentialsFile>(&bytes)
        .map_err(|source| CredentialsError::Decode { source })?;
    let resolved = tokio::task::spawn_blocking(move || resolve(parsed, expected_uid))
        .await
        .map_err(|join| CredentialsError::Read {
            path: path.to_path_buf(),
            source: std::io::Error::other(join),
        })??;
    Ok(resolved)
}

fn resolve(file: CredentialsFile, expected_uid: u32) -> Result<CredentialStore, CredentialsError> {
    if file.credentials.len() > HARD_MAX_CREDENTIALS {
        return Err(CredentialsError::TooMany {
            maximum: HARD_MAX_CREDENTIALS,
        });
    }
    let mut problems = Vec::new();
    let mut entries = Vec::with_capacity(file.credentials.len());
    let mut seen = std::collections::BTreeSet::new();
    for entry in file.credentials {
        let name = entry.name.clone();
        if name.is_empty()
            || name.len() > MAX_CREDENTIAL_NAME_BYTES
            || name.trim() != name
            || name
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            problems.push(format!(
                "credential name {name:?} is empty, oversized, or contains whitespace"
            ));
            continue;
        }
        if !seen.insert(name.clone()) {
            problems.push(format!("credential {name:?} is declared more than once"));
            continue;
        }
        match resolve_entry(entry, expected_uid) {
            Ok(credential) => entries.push((name, credential)),
            Err(entry_problems) => problems.extend(entry_problems),
        }
    }
    if !problems.is_empty() {
        return Err(CredentialsError::Invalid { problems });
    }
    CredentialStore::new(entries).map_err(|source| CredentialsError::Store { source })
}

fn resolve_entry(
    entry: CredentialEntry,
    expected_uid: u32,
) -> Result<StoredCredential, Vec<String>> {
    let name = entry.name;
    let kind = entry.kind;
    let mut problems = Vec::new();
    let absent = |field: &str, present: bool, problems: &mut Vec<String>| {
        if present {
            problems.push(format!(
                "credential {name:?} is kind {} and must not set {field}",
                kind.label()
            ));
        }
    };
    match kind {
        CredentialKind::BearerToken => {
            absent("authFile", entry.auth_file.is_some(), &mut problems);
            if entry.scheme.is_none() {
                problems.push(format!(
                    "credential {name:?} is kind bearerToken and needs scheme"
                ));
            }
            if entry.secret.is_none() {
                problems.push(format!(
                    "credential {name:?} is kind bearerToken and needs secret"
                ));
            }
            let (Some(scheme), Some(secret)) = (entry.scheme.as_deref(), entry.secret) else {
                return Err(problems);
            };
            match BoundCredential::bearer(scheme, secret, entry.destinations) {
                Ok(credential) if problems.is_empty() => Ok(credential.into()),
                Ok(_) => Err(problems),
                Err(source) => {
                    problems.push(describe_structural(&name, &source));
                    Err(problems)
                }
            }
        }
        CredentialKind::ChatgptSubscription => {
            absent("secret", entry.secret.is_some(), &mut problems);
            absent("scheme", entry.scheme.is_some(), &mut problems);
            let Some(auth_file) = entry.auth_file else {
                problems.push(format!(
                    "credential {name:?} is kind chatgptSubscription and needs authFile"
                ));
                return Err(problems);
            };
            if auth_file.is_relative() {
                problems.push(format!(
                    "credential {name:?} authFile {} is relative; the broker resolves nothing \
                     against its working directory",
                    auth_file.display()
                ));
                return Err(problems);
            }
            if let Err(source) = BoundCredential::chatgpt_subscription(
                Redacted::new("startup-destination-probe".to_owned()),
                "startup-destination-probe",
                entry.destinations.clone(),
            ) {
                problems.push(describe_structural(&name, &source));
            }
            match open_auth_file(&name, &auth_file, expected_uid) {
                Ok(credential) if problems.is_empty() => Ok(StoredCredential::Refreshing(
                    Arc::new(ChatGptSubscriptionCredential {
                        name,
                        credential: Arc::new(credential),
                        destinations: entry.destinations,
                    }),
                )),
                Ok(_) => Err(problems),
                Err(problem) => {
                    problems.push(problem);
                    Err(problems)
                }
            }
        }
    }
}

fn describe_structural(name: &str, source: &ConfigurationError) -> String {
    format!("credential {name:?} is structurally invalid: {source}")
}

fn open_auth_file(
    name: &str,
    auth_file: &Path,
    expected_uid: u32,
) -> Result<CredentialFile, String> {
    drop(
        read_trusted_file(
            auth_file,
            expected_uid,
            FileTier::Private,
            HARD_MAX_CHATGPT_AUTH_BYTES,
        )
        .map_err(|source| {
            format!(
                "credential {name:?} authFile {} is not trusted input ({}): {}",
                auth_file.display(),
                source.category(),
                error_chain(&source)
            )
        })?,
    );
    // The auth-file parent must be writable, since a read-only parent would leave a
    // rotated-but-unsaved retired token on disk for reuse.
    socket::validate_private_parent(auth_file, expected_uid).map_err(|source| {
        format!(
            "credential {name:?} authFile {} needs an owner-only parent directory: {}",
            auth_file.display(),
            error_chain(&source)
        )
    })?;
    let parent = auth_file.parent().ok_or_else(|| {
        format!(
            "credential {name:?} authFile {} has no parent directory",
            auth_file.display()
        )
    })?;
    if !parent_is_owner_writable(parent) {
        return Err(format!(
            "credential {name:?} authFile {} has a parent directory the broker cannot write; a \
             rotated credential is persisted by renaming a sibling temporary file over the target",
            auth_file.display()
        ));
    }
    let credential = CredentialFile::open(auth_file, REFRESH_TIMEOUT).map_err(|source| {
        format!(
            "credential {name:?} authFile {} is not a Dekopon ChatGPT credential: {}",
            auth_file.display(),
            error_chain(&source)
        )
    })?;
    let status = credential.status().map_err(|source| {
        format!(
            "credential {name:?} authFile {} could not be inspected: {}",
            auth_file.display(),
            error_chain(&source)
        )
    })?;
    tracing::info!(
        event = "broker_chatgpt_credential_loaded",
        credential = name,
        path = %auth_file.display(),
        expires_at = status.expires_at,
        expired = status.expired,
        "loaded a ChatGPT subscription credential"
    );
    Ok(credential)
}

#[cfg(unix)]
fn parent_is_owner_writable(parent: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::symlink_metadata(parent)
        .is_ok_and(|metadata| metadata.permissions().mode() & 0o200 != 0)
}

/// Sharing one CredentialFile per entry across invocations is what keeps two concurrent invocations
/// of the same capability from spending the same single-use refresh token twice.
struct ChatGptSubscriptionCredential {
    name: String,
    credential: Arc<CredentialFile>,
    destinations: Vec<String>,
}

impl std::fmt::Debug for ChatGptSubscriptionCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChatGptSubscriptionCredential")
            .field("name", &self.name)
            .field("destinations", &self.destinations)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RefreshingCredential for ChatGptSubscriptionCredential {
    fn destinations(&self) -> &[String] {
        &self.destinations
    }

    async fn resolve(&self) -> Result<BoundCredential, CredentialRefreshError> {
        let span = tracing::info_span!(
            "broker.credential.refresh",
            credential = %self.name,
            outcome = tracing::field::Empty,
        );
        let credential = Arc::clone(&self.credential);
        let blocking_span = span.clone();
        // This refresh runs on the blocking pool because dekopon-model's client blocks on a file
        // lock and would stall the runtime worker inline.
        let joined =
            tokio::task::spawn_blocking(move || blocking_span.in_scope(|| credential.current()))
                .await;
        let resolved = match joined {
            Ok(resolved) => resolved,
            Err(source) => {
                span.record("outcome", "failed");
                tracing::warn!(
                    event = "broker_chatgpt_credential_refresh_failed",
                    credential = %self.name,
                    category = "refresh-task",
                    error = %source,
                    "the credential refresh task did not complete"
                );
                return Err(CredentialRefreshError::Unavailable {
                    category: "refresh-task",
                });
            }
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(source) => {
                span.record("outcome", "failed");
                return Err(self.classify(&source));
            }
        };
        span.record(
            "outcome",
            resolved.refresh.map_or("current", RefreshOutcome::label),
        );
        BoundCredential::chatgpt_subscription(
            resolved.access,
            &resolved.account_id,
            self.destinations.clone(),
        )
        .map_err(|source| {
            tracing::warn!(
                event = "broker_chatgpt_credential_refresh_failed",
                credential = %self.name,
                category = "invalid-material",
                reason = %source,
                "a renewed ChatGPT credential is not presentable"
            );
            CredentialRefreshError::Unavailable {
                category: "invalid-material",
            }
        })
    }
}

impl ChatGptSubscriptionCredential {
    fn classify(&self, error: &ChatGptError) -> CredentialRefreshError {
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
            ChatGptError::Request(_) => (false, "transport"),
            ChatGptError::Protocol(_) => (false, "token-endpoint-protocol"),
            _ => (true, "credential-unusable"),
        };
        if permanent {
            tracing::error!(
                event = "broker_chatgpt_credential_reauth_required",
                credential = %self.name,
                path = %self.credential.path().display(),
                category = category,
                error = %error_chain(error),
                "the ChatGPT subscription credential must be renewed with `dekopond auth chatgpt \
                 login --auth-file`"
            );
            CredentialRefreshError::ReauthorizationRequired
        } else {
            tracing::warn!(
                event = "broker_chatgpt_credential_refresh_failed",
                credential = %self.name,
                category = category,
                error = %error_chain(error),
                "a ChatGPT subscription credential could not be renewed"
            );
            CredentialRefreshError::Unavailable { category }
        }
    }
}

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
    InsecureFile {
        path: PathBuf,
        #[source]
        source: FileHygieneError,
    },
    #[error("broker credentials are {length} bytes; maximum is {maximum}")]
    TooLarge { length: u64, maximum: usize },
    #[error("broker credentials are not strict valid YAML/JSON")]
    Decode {
        #[source]
        source: serde_yaml::Error,
    },
    #[error("broker credentials name too many entries; maximum is {maximum}")]
    TooMany { maximum: usize },
    #[error("broker credentials are invalid: {}", problems.join("; "))]
    Invalid { problems: Vec<String> },
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

    fn chatgpt_document(expires_at: u64) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-fixture"}
            }))
            .expect("serialize JWT fixture"),
        );
        serde_json::json!({
            "version": 1,
            "access": format!("header.{payload}.signature"),
            "refresh": "fixture-refresh-token",
            "expiresAt": expires_at,
            "accountId": "acct-fixture",
        })
        .to_string()
    }

    async fn write_credentials(directory: &std::path::Path, contents: &str, mode: u32) {
        let path = directory.join("credentials.yaml");
        tokio::fs::write(&path, contents).await.expect("write file");
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .await
            .expect("set mode");
    }

    async fn write_auth_file(
        root: &std::path::Path,
        subdir: &str,
        expires_at: u64,
        mode: u32,
    ) -> std::path::PathBuf {
        let directory = root.join(subdir);
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("create auth directory");
        tokio::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .await
            .expect("set directory mode");
        let path = directory.join("chatgpt-auth.json");
        tokio::fs::write(&path, chatgpt_document(expires_at))
            .await
            .expect("write auth file");
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .await
            .expect("set auth file mode");
        path
    }

    fn chatgpt_credentials(auth_file: &std::path::Path) -> String {
        format!(
            "apiVersion: dekopon.dev/broker-credentials/v1alpha1\ncredentials:\n  - name: \
             chatgpt-gpt-image\n    kind: chatgptSubscription\n    authFile: {}\n    \
             destinations: [chatgpt.com]\n",
            auth_file.display()
        )
    }

    fn uid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn problems(error: &CredentialsError) -> String {
        let CredentialsError::Invalid { problems } = error else {
            panic!("expected an aggregated entry refusal, got {error:?}");
        };
        problems.join(" | ")
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
        assert!(problems(&error).contains("structurally invalid"), "{error}");
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
        assert!(
            problems(&error).contains("declared more than once"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn loads_a_chatgpt_subscription_credential_and_its_auth_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let auth = write_auth_file(directory.path(), "broker-chatgpt", u64::MAX, 0o600).await;
        write_credentials(directory.path(), &chatgpt_credentials(&auth), 0o600).await;

        load(&directory.path().join("credentials.yaml"), uid())
            .await
            .expect("a trusted auth file on an owner-only writable parent loads");
    }

    #[tokio::test]
    async fn both_kinds_coexist_in_one_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let auth = write_auth_file(directory.path(), "broker-chatgpt", u64::MAX, 0o600).await;
        let mixed = format!(
            "{VALID}{}",
            chatgpt_credentials(&auth)
                .split_once("credentials:\n")
                .expect("fixture has a credentials list")
                .1
        );
        write_credentials(directory.path(), &mixed, 0o600).await;

        load(&directory.path().join("credentials.yaml"), uid())
            .await
            .expect("a fixed secret and a refreshing credential coexist");
    }

    #[tokio::test]
    async fn every_untrusted_auth_file_refuses_startup_with_its_cause() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let good = write_auth_file(directory.path(), "good", u64::MAX, 0o600).await;

        let missing = directory.path().join("good").join("absent.json");
        let symlinked = directory.path().join("good").join("linked.json");
        std::os::unix::fs::symlink(&good, &symlinked).expect("create symlink fixture");
        let group_readable = write_auth_file(directory.path(), "group", u64::MAX, 0o640).await;
        let world_readable = write_auth_file(directory.path(), "world", u64::MAX, 0o644).await;
        let unparseable = write_auth_file(directory.path(), "unparseable", u64::MAX, 0o600).await;
        tokio::fs::write(&unparseable, "not a credential document")
            .await
            .expect("overwrite with a non-credential document");
        let read_only = write_auth_file(directory.path(), "read-only", u64::MAX, 0o600).await;
        tokio::fs::set_permissions(
            read_only.parent().expect("auth parent"),
            std::fs::Permissions::from_mode(0o500),
        )
        .await
        .expect("make the parent read-only");
        let open_parent = write_auth_file(directory.path(), "open-parent", u64::MAX, 0o600).await;
        tokio::fs::set_permissions(
            open_parent.parent().expect("auth parent"),
            std::fs::Permissions::from_mode(0o750),
        )
        .await
        .expect("widen the parent");

        for (path, expected) in [
            (missing, "not trusted input"),
            (symlinked, "not trusted input"),
            (group_readable, "not trusted input"),
            (world_readable, "not trusted input"),
            (unparseable, "not a Dekopon ChatGPT credential"),
            (read_only, "cannot write"),
            (open_parent, "owner-only parent directory"),
            (
                std::path::PathBuf::from("relative/chatgpt-auth.json"),
                "is relative",
            ),
        ] {
            write_credentials(directory.path(), &chatgpt_credentials(&path), 0o600).await;
            let error = match load(&directory.path().join("credentials.yaml"), uid()).await {
                Ok(_) => panic!("{} must refuse startup", path.display()),
                Err(error) => error,
            };
            let rendered = problems(&error);
            assert!(
                rendered.contains(expected),
                "{} was refused as {rendered:?}, which does not name {expected:?}",
                path.display()
            );
        }

        for subdir in ["read-only", "open-parent"] {
            tokio::fs::set_permissions(
                directory.path().join(subdir),
                std::fs::Permissions::from_mode(0o700),
            )
            .await
            .expect("restore mode");
        }
    }

    #[tokio::test]
    async fn every_field_problem_in_the_file_is_reported_at_once() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let auth = write_auth_file(directory.path(), "broker-chatgpt", u64::MAX, 0o600).await;
        let contents = [
            "apiVersion: dekopon.dev/broker-credentials/v1alpha1".to_owned(),
            "credentials:".to_owned(),
            "  - name: chatgpt-with-a-secret".to_owned(),
            "    kind: chatgptSubscription".to_owned(),
            "    scheme: Bearer".to_owned(),
            "    secret: must-not-be-here".to_owned(),
            format!("    authFile: {}", auth.display()),
            "    destinations: [chatgpt.com]".to_owned(),
            "  - name: bearer-with-an-auth-file".to_owned(),
            "    kind: bearerToken".to_owned(),
            "    scheme: Bearer".to_owned(),
            "    secret: fixture-secret-value".to_owned(),
            format!("    authFile: {}", auth.display()),
            "    destinations: [api.github.com]".to_owned(),
            "  - name: bearer-with-no-scheme-or-secret".to_owned(),
            "    kind: bearerToken".to_owned(),
            "    destinations: [api.github.com]".to_owned(),
            "  - name: chatgpt-with-no-auth-file".to_owned(),
            "    kind: chatgptSubscription".to_owned(),
            "    destinations: [chatgpt.com]".to_owned(),
        ]
        .join("\n");
        write_credentials(directory.path(), &contents, 0o600).await;

        let error = load(&directory.path().join("credentials.yaml"), uid())
            .await
            .expect_err("four wrong entries must refuse startup");

        let CredentialsError::Invalid { problems } = &error else {
            panic!("expected an aggregated entry refusal, got {error:?}");
        };
        let rendered = problems.join(" | ");
        assert!(
            rendered.contains(
                "\"chatgpt-with-a-secret\" is kind chatgptSubscription and must not set secret"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "\"chatgpt-with-a-secret\" is kind chatgptSubscription and must not set scheme"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "\"bearer-with-an-auth-file\" is kind bearerToken and must not set authFile"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "\"bearer-with-no-scheme-or-secret\" is kind bearerToken and needs scheme"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "\"bearer-with-no-scheme-or-secret\" is kind bearerToken and needs secret"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "\"chatgpt-with-no-auth-file\" is kind chatgptSubscription and needs authFile"
            ),
            "{rendered}"
        );
        assert!(
            problems.len() >= 6,
            "every problem must be reported at once, got {problems:?}"
        );
        assert!(
            !rendered.contains("must-not-be-here") && !rendered.contains("fixture-secret-value"),
            "a refusal echoed a secret: {rendered}"
        );
    }
}
