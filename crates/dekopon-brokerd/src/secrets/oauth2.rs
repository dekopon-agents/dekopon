use std::{
    num::NonZeroU64,
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_core::{
    FileHygieneError, FileTier, Redacted, private_file::PrivateFileLock, read_trusted_file,
};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tracing::{Instrument as _, instrument::WithSubscriber as _};

use super::{SourceError, classified};

const RECORD_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OAuth2Record {
    version: u8,
    #[serde(serialize_with = "dekopon_core::serialize_exposed")]
    access: Redacted<String>,
    #[serde(serialize_with = "dekopon_core::serialize_exposed")]
    refresh: Redacted<String>,
    expires_at: u64,
}

struct OpenedRecord {
    lock: PrivateFileLock,
    record: OAuth2Record,
}

struct RefreshInputs {
    path: PathBuf,
    endpoint: String,
    client_id: String,
    timeout: Duration,
    uid: u32,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Redacted<String>,
    refresh_token: Option<Redacted<String>>,
    expires_in: u64,
}

struct ValidatedTokenResponse {
    access_token: Redacted<String>,
    refresh_token: Option<Redacted<String>>,
    expires_in: NonZeroU64,
}

impl TryFrom<TokenResponse> for ValidatedTokenResponse {
    type Error = SourceError;

    fn try_from(value: TokenResponse) -> Result<Self, Self::Error> {
        if value.access_token.expose().is_empty()
            || value
                .refresh_token
                .as_ref()
                .is_some_and(|token| token.expose().is_empty())
        {
            return Err(SourceError::Malformed);
        }
        let expires_in = NonZeroU64::new(value.expires_in).ok_or(SourceError::Malformed)?;
        Ok(Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            expires_in,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
}

enum RefreshResult {
    NotDue(OAuth2Record),
    Refreshed(OAuth2Record),
    RefreshedUnsaved(OAuth2Record),
}

impl RefreshResult {
    const fn label(&self) -> &'static str {
        match self {
            Self::NotDue(_) => "not-due",
            Self::Refreshed(_) | Self::RefreshedUnsaved(_) => "refreshed",
        }
    }

    fn record(self) -> OAuth2Record {
        match self {
            Self::NotDue(record) | Self::Refreshed(record) | Self::RefreshedUnsaved(record) => {
                record
            }
        }
    }
}

pub(super) async fn resolve(
    path: &Path,
    endpoint: &str,
    client_id: &str,
    timeout_ms: u64,
    uid: u32,
    client: &reqwest::Client,
) -> Result<Vec<u8>, SourceError> {
    let span = tracing::Span::current();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    #[expect(
        clippy::disallowed_methods,
        reason = "owner: the broker request awaits the handle; bound: one refresh per authorized resolution, with a bounded HTTP deadline; the task must persist a spent rotation even if the caller drops"
    )]
    let task = tokio::task::spawn(
        run(
            RefreshInputs {
                path: path.to_path_buf(),
                endpoint: endpoint.to_owned(),
                client_id: client_id.to_owned(),
                timeout: Duration::from_millis(timeout_ms),
                uid,
                client: client.clone(),
            },
            span.clone(),
        )
        .instrument(span)
        .with_subscriber(dispatch),
    );
    task.await
        .map_err(|source| classified(SourceError::Internal, &source))?
}

fn read_locked(path: PathBuf, uid: u32) -> Result<OpenedRecord, SourceError> {
    let lock = PrivateFileLock::acquire(&path)
        .map_err(|error| classified(SourceError::Io, &error.source))?;
    let record = read_record(&path, uid)?;
    Ok(OpenedRecord { lock, record })
}

fn read_record(path: &Path, uid: u32) -> Result<OAuth2Record, SourceError> {
    let bytes = read_trusted_file(path, uid, FileTier::Private, RECORD_BYTES).map_err(|error| {
        let mapped = match error {
            FileHygieneError::Io { .. } => SourceError::Io,
            FileHygieneError::TooLarge { .. } => SourceError::TooLarge,
            FileHygieneError::NotRegular { .. }
            | FileHygieneError::InsecureMode { .. }
            | FileHygieneError::WrongOwner { .. }
            | FileHygieneError::HardLinked { .. }
            | FileHygieneError::UnsafeAncestor { .. } => SourceError::Insecure,
        };
        classified(mapped, &error)
    })?;
    let record: OAuth2Record = serde_json::from_slice(&bytes)
        .map_err(|source| classified(SourceError::Malformed, &source))?;
    if record.version != 1
        || record.access.expose().is_empty()
        || record.refresh.expose().is_empty()
    {
        return Err(SourceError::Malformed);
    }
    Ok(record)
}

async fn run(inputs: RefreshInputs, span: tracing::Span) -> Result<Vec<u8>, SourceError> {
    let RefreshInputs {
        path,
        endpoint,
        client_id,
        timeout,
        uid,
        client,
    } = inputs;
    let started = std::time::Instant::now();
    let locked_path = path.clone();
    let opened = tokio::task::spawn_blocking(move || read_locked(locked_path, uid))
        .await
        .map_err(|source| classified(SourceError::Internal, &source))?;
    let outcome = match opened {
        Ok(opened) => refresh(opened, path, endpoint, client_id, timeout, client).await,
        Err(error) => Err(error),
    };
    span.record(
        "duration_ms",
        i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    );
    match outcome {
        Ok(result) => {
            span.record("outcome", result.label());
            let record = result.record();
            span.record(
                "credential.expires_at_unix_ms",
                i64::try_from(record.expires_at.saturating_mul(1000)).unwrap_or(i64::MAX),
            );
            Ok(record.access.expose().as_bytes().to_vec())
        }
        Err(error) => {
            span.record(
                "outcome",
                match error {
                    SourceError::ReauthorizationRequired => "reauth-required",
                    SourceError::Config
                    | SourceError::Insecure
                    | SourceError::Io
                    | SourceError::Timeout
                    | SourceError::Transport
                    | SourceError::Rejected
                    | SourceError::Missing
                    | SourceError::Integrity
                    | SourceError::BootstrapReflected
                    | SourceError::Empty
                    | SourceError::Expired
                    | SourceError::Malformed
                    | SourceError::TooLarge
                    | SourceError::Internal => "failed",
                },
            );
            Err(error)
        }
    }
}

async fn refresh(
    opened: OpenedRecord,
    path: PathBuf,
    endpoint: String,
    client_id: String,
    timeout: Duration,
    client: reqwest::Client,
) -> Result<RefreshResult, SourceError> {
    if now_seconds() < opened.record.expires_at.saturating_sub(60) {
        return Ok(RefreshResult::NotDue(opened.record));
    }
    let replacement = post(
        &client,
        &endpoint,
        &client_id,
        &opened.record.refresh,
        timeout,
    )
    .await?;
    if replacement.access_token.expose().len() > super::HARD_MAX_SECRET_BYTES {
        return Err(SourceError::TooLarge);
    }
    let expires_at = now_seconds()
        .checked_add(replacement.expires_in.get())
        .ok_or(SourceError::Malformed)?;
    let record = OAuth2Record {
        version: 1,
        access: replacement.access_token,
        refresh: replacement.refresh_token.unwrap_or(opened.record.refresh),
        expires_at,
    };
    let bytes = serde_json::to_vec(&record)
        .map_err(|source| classified(SourceError::Malformed, &source))?;
    if bytes.len() > RECORD_BYTES {
        return Err(SourceError::Malformed);
    }
    let record_path = path.clone();
    let saved = tokio::task::spawn_blocking(move || {
        let _lock = opened.lock;
        dekopon_core::private_file::replace_private_file(&path, &bytes)
    })
    .await
    .map_err(|source| classified(SourceError::Internal, &source))?;
    match saved {
        Ok(()) => Ok(RefreshResult::Refreshed(record)),
        Err(error) => {
            tracing::error!(
                name: "broker.credential.refresh_save_failed",
                target: "credential",
                record_path = %record_path.display(),
                failed_path = %error.path.display(),
                error = %error.source,
                "OAuth refresh succeeded but the record could not be saved"
            );
            Ok(RefreshResult::RefreshedUnsaved(record))
        }
    }
}

async fn post(
    client: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    refresh: &Redacted<String>,
    timeout: Duration,
) -> Result<ValidatedTokenResponse, SourceError> {
    let request = client.post(endpoint).form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.expose()),
        ("client_id", client_id),
    ]);
    tokio::time::timeout(timeout, async {
        let response = request.send().await.map_err(|source| {
            classified(
                if source.is_timeout() {
                    SourceError::Timeout
                } else {
                    SourceError::Transport
                },
                &source,
            )
        })?;
        let status = response.status();
        if response.headers().len() > super::MAX_SOURCE_RESPONSE_HEADERS
            || response
                .headers()
                .iter()
                .try_fold(0_usize, |size, (name, value)| {
                    size.checked_add(name.as_str().len())?
                        .checked_add(value.as_bytes().len())?
                        .checked_add(4)
                })
                .is_none_or(|size| size > super::MAX_SOURCE_RESPONSE_HEADER_BYTES)
        {
            return Err(SourceError::TooLarge);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| {
                classified(
                    if source.is_timeout() {
                        SourceError::Timeout
                    } else {
                        SourceError::Transport
                    },
                    &source,
                )
            })?;
            if body.len().saturating_add(chunk.len()) > RECORD_BYTES {
                return Err(SourceError::Malformed);
            }
            body.extend_from_slice(&chunk);
        }
        if status.is_success() {
            let parsed: TokenResponse = serde_json::from_slice(&body)
                .map_err(|source| classified(SourceError::Malformed, &source))?;
            ValidatedTokenResponse::try_from(parsed)
        } else if status.is_client_error() {
            let response: ErrorResponse = serde_json::from_slice(&body)
                .map_err(|source| classified(SourceError::Malformed, &source))?;
            if crate::credentials::REAUTHORIZATION_CODES.contains(&response.error.as_str()) {
                Err(SourceError::ReauthorizationRequired)
            } else {
                Err(SourceError::Rejected)
            }
        } else if status.is_server_error() {
            Err(SourceError::Transport)
        } else {
            Err(SourceError::Rejected)
        }
    })
    .await
    .map_err(|source| classified(SourceError::Timeout, &source))?
}

fn now_seconds() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0)
}
