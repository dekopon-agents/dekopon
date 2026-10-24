use std::{
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dekopon_core::{
    FileHygieneError, FileTier, Redacted, SecretDrn, private_file::PrivateFileLock,
    read_trusted_file,
};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tracing::{Instrument as _, instrument::WithSubscriber as _};

use super::{SourceError, classified};

const RECORD_BYTES: usize = 64 * 1024;
const CYCLE: u64 = 22 * 60 * 60;
const BUFFER: u64 = 2 * 60 * 60;
const RETRY: u64 = 5 * 60;

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum OAuth2Record {
    Live {
        version: u8,
        #[serde(serialize_with = "dekopon_core::serialize_exposed")]
        access: Redacted<String>,
        #[serde(serialize_with = "dekopon_core::serialize_exposed")]
        refresh: Redacted<String>,
        expires_at: u64,
        last_refresh: u64,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_seed_digest"
        )]
        adopted_seed_sha256: Option<String>,
    },
    Sealed {
        version: u8,
        since: u64,
        cause: SealCause,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_seed_digest"
        )]
        adopted_seed_sha256: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum SealCause {
    ReauthorizationRequired,
    BootstrapReflected,
}

impl SealCause {
    const fn label(&self) -> &'static str {
        match self {
            Self::ReauthorizationRequired => "reauthorization-required",
            Self::BootstrapReflected => "bootstrap-reflected",
        }
    }
}

impl OAuth2Record {
    fn access(&self) -> Result<&Redacted<String>, SourceError> {
        match self {
            Self::Live { access, .. } => Ok(access),
            Self::Sealed { .. } => Err(SourceError::ReauthorizationRequired),
        }
    }

    const fn expires_at(&self) -> Option<u64> {
        match self {
            Self::Live { expires_at, .. } => Some(*expires_at),
            Self::Sealed { .. } => None,
        }
    }
}

struct OpenedRecord {
    lock: PrivateFileLock,
    record: Option<OAuth2Record>,
}

pub(super) struct RefreshInputs {
    pub(super) credential: Option<SecretDrn>,
    pub(super) path: PathBuf,
    pub(super) seed_path: Option<PathBuf>,
    pub(super) endpoint: String,
    pub(super) client_id: String,
    pub(super) timeout: Duration,
    pub(super) uid: u32,
    pub(super) client: reqwest::Client,
    pub(super) clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub(super) scheduled: bool,
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
}

impl RefreshResult {
    const fn label(&self) -> &'static str {
        match self {
            Self::NotDue(_) => "not-due",
            Self::Refreshed(_) => "refreshed",
        }
    }

    fn checked(self) -> Result<Self, SourceError> {
        match &self {
            Self::NotDue(record) => {
                if let OAuth2Record::Live {
                    access, refresh, ..
                } = record
                {
                    super::reject_bootstrap_reflection(
                        access.expose().as_bytes().to_vec(),
                        &[refresh.expose()],
                    )?;
                }
            }
            Self::Refreshed(record) => {
                if record.expires_at() == Some(0) {
                    return Err(SourceError::BootstrapReflected);
                }
            }
        }
        Ok(self)
    }

    fn record(self) -> OAuth2Record {
        match self {
            Self::NotDue(record) | Self::Refreshed(record) => record,
        }
    }
}

pub(super) struct OAuth2Source<'a> {
    pub(super) path: &'a Path,
    pub(super) seed_path: Option<&'a Path>,
    pub(super) endpoint: &'a str,
    pub(super) client_id: &'a str,
    pub(super) timeout_ms: u64,
}

pub(super) async fn resolve_for(
    credential: Option<&SecretDrn>,
    source: OAuth2Source<'_>,
    uid: u32,
    client: &reqwest::Client,
) -> Result<Vec<u8>, SourceError> {
    let OAuth2Source {
        path,
        seed_path,
        endpoint,
        client_id,
        timeout_ms,
    } = source;
    resolve_with_clock(RefreshInputs {
        credential: credential.cloned(),
        path: path.to_path_buf(),
        seed_path: seed_path.map(Path::to_path_buf),
        endpoint: endpoint.to_owned(),
        client_id: client_id.to_owned(),
        timeout: Duration::from_millis(timeout_ms),
        uid,
        client: client.clone(),
        clock: Arc::new(now_seconds),
        scheduled: false,
    })
    .await
}

pub(super) fn refresh_task(
    inputs: RefreshInputs,
    span: tracing::Span,
) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send + 'static {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    run(inputs, span.clone())
        .instrument(span)
        .with_subscriber(dispatch)
}

pub(super) async fn resolve_with_clock(inputs: RefreshInputs) -> Result<Vec<u8>, SourceError> {
    #[expect(
        clippy::disallowed_methods,
        reason = "owner: the broker request awaits the handle; bound: one refresh per authorized resolution, with a bounded HTTP deadline; the task must persist a spent rotation even if the caller drops"
    )]
    let task = tokio::task::spawn(refresh_task(inputs, tracing::Span::current()));
    task.await
        .map_err(|source| classified(SourceError::Internal, &source))?
}

fn read_locked(path: PathBuf, uid: u32) -> Result<OpenedRecord, SourceError> {
    let lock = PrivateFileLock::acquire(&path)
        .map_err(|error| classified(SourceError::Io, &error.source))?;
    let record = match read_record(&path, uid) {
        Ok(record) => Some(record),
        Err(SourceError::Io)
            if std::fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    Ok(OpenedRecord { lock, record })
}

fn deserialize_seed_digest<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn valid_digest(digest: &Option<String>) -> bool {
    digest.as_ref().is_none_or(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
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
    let valid = match &record {
        OAuth2Record::Live {
            version,
            access,
            refresh,
            expires_at,
            last_refresh,
            adopted_seed_sha256,
        } => {
            *version == 2
                && !access.expose().is_empty()
                && !refresh.expose().is_empty()
                && *expires_at > *last_refresh
                && valid_digest(adopted_seed_sha256)
        }
        OAuth2Record::Sealed {
            version,
            since,
            cause: _,
            adopted_seed_sha256,
        } => *version == 2 && *since > 0 && valid_digest(adopted_seed_sha256),
    };
    if !valid {
        return Err(SourceError::Malformed);
    }
    Ok(record)
}

async fn run(inputs: RefreshInputs, span: tracing::Span) -> Result<Vec<u8>, SourceError> {
    let started = std::time::Instant::now();
    let locked_path = inputs.path.clone();
    let uid = inputs.uid;
    let read_span = span.clone();
    let read_dispatch = tracing::dispatcher::get_default(Clone::clone);
    let outcome = match tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&read_dispatch, || {
            read_span.in_scope(|| read_locked(locked_path, uid))
        })
    })
    .await
    {
        Ok(Ok(opened)) => refresh(opened, inputs)
            .await
            .and_then(RefreshResult::checked),
        Ok(Err(error)) => Err(error),
        Err(source) => Err(classified(SourceError::Internal, &source)),
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
                i64::try_from(record.expires_at().unwrap_or(0).saturating_mul(1000))
                    .unwrap_or(i64::MAX),
            );
            Ok(record.access()?.expose().as_bytes().to_vec())
        }
        Err(error) => {
            span.record("otel.status_code", "error");
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

#[expect(
    clippy::too_many_lines,
    reason = "one lock-held transaction serializes seed adoption, HTTP refresh, sealing and persistence"
)]
async fn refresh(
    opened: OpenedRecord,
    inputs: RefreshInputs,
) -> Result<RefreshResult, SourceError> {
    let RefreshInputs {
        credential,
        path,
        seed_path,
        endpoint,
        client_id,
        timeout,
        uid,
        client,
        clock,
        scheduled,
    } = inputs;
    let seed = match seed_path {
        Some(seed_path) => match tokio::fs::symlink_metadata(&seed_path).await {
            Ok(_) => Some(super::read_token(&seed_path, uid).await?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(classified(SourceError::Io, &error)),
        },
        None => None,
    };
    let digest = seed
        .as_ref()
        .map(|value| super::hex_sha256(value.expose().as_bytes()));
    let previous_digest = opened.record.as_ref().and_then(|record| match record {
        OAuth2Record::Live {
            adopted_seed_sha256,
            ..
        }
        | OAuth2Record::Sealed {
            adopted_seed_sha256,
            ..
        } => adopted_seed_sha256.as_ref(),
    });
    let adopt = seed.is_some() && digest.as_ref() != previous_digest;
    let (predecessor, adopted_seed_sha256) = if adopt {
        (seed.as_ref().ok_or(SourceError::Missing)?, digest)
    } else {
        let Some(OAuth2Record::Live {
            access,
            refresh,
            expires_at,
            ..
        }) = opened.record.as_ref()
        else {
            return Err(if opened.record.is_some() {
                SourceError::ReauthorizationRequired
            } else {
                SourceError::Missing
            });
        };
        let due = if scheduled {
            let OAuth2Record::Live { last_refresh, .. } =
                opened.record.as_ref().ok_or(SourceError::Missing)?
            else {
                return Err(SourceError::ReauthorizationRequired);
            };
            clock()
                >= last_refresh
                    .saturating_add(CYCLE)
                    .min(expires_at.saturating_sub(BUFFER))
        } else {
            clock() >= expires_at.saturating_sub(60)
        };
        if !due {
            if super::bootstrap_reflected(
                access.expose().as_bytes(),
                &[
                    refresh.expose(),
                    seed.as_ref().map_or("", |value| value.expose()),
                ],
            ) {
                save_record(
                    &path,
                    opened.lock,
                    &OAuth2Record::Sealed {
                        version: 2,
                        since: clock(),
                        cause: SealCause::BootstrapReflected,
                        adopted_seed_sha256: previous_digest.cloned(),
                    },
                )
                .await?;
                tracing::info!(target: "dekopon_brokerd::audit", { audit.event = "broker.credential.sealed", credential = %credential.as_ref().map_or("unbound", |drn| drn.as_str()), since = clock(), cause = "bootstrap-reflected", instruction = "provide a new seed" }, "broker.credential.sealed");
                return Err(SourceError::BootstrapReflected);
            }
            return Ok(RefreshResult::NotDue(
                opened.record.ok_or(SourceError::Missing)?,
            ));
        }
        (refresh, previous_digest.cloned())
    };
    let sent_at = clock();
    let replacement = match post(&client, &endpoint, &client_id, predecessor, timeout).await {
        Ok(value) => value,
        Err(SourceError::ReauthorizationRequired) => {
            save_record(
                &path,
                opened.lock,
                &OAuth2Record::Sealed {
                    version: 2,
                    since: sent_at,
                    cause: SealCause::ReauthorizationRequired,
                    adopted_seed_sha256,
                },
            )
            .await?;
            tracing::info!(target: "dekopon_brokerd::audit", { audit.event = "broker.credential.sealed", credential = %credential.as_ref().map_or("unbound", |drn| drn.as_str()), since = sent_at, cause = "reauthorization-required", instruction = "provide a new seed" }, "broker.credential.sealed");
            return Err(SourceError::ReauthorizationRequired);
        }
        Err(error) => return Err(error),
    };
    let next_refresh = replacement.refresh_token.as_ref().unwrap_or(predecessor);
    if super::bootstrap_reflected(
        replacement.access_token.expose().as_bytes(),
        &[
            predecessor.expose(),
            next_refresh.expose(),
            seed.as_ref().map_or("", |value| value.expose()),
        ],
    ) {
        save_record(
            &path,
            opened.lock,
            &OAuth2Record::Sealed {
                version: 2,
                since: sent_at,
                cause: SealCause::BootstrapReflected,
                adopted_seed_sha256,
            },
        )
        .await?;
        tracing::info!(target: "dekopon_brokerd::audit", { audit.event = "broker.credential.sealed", credential = %credential.as_ref().map_or("unbound", |drn| drn.as_str()), since = sent_at, cause = "bootstrap-reflected", instruction = "provide a new seed" }, "broker.credential.sealed");
        return Err(SourceError::BootstrapReflected);
    }
    let expires_at = sent_at
        .checked_add(replacement.expires_in.get())
        .ok_or(SourceError::Malformed)?;
    let record = OAuth2Record::Live {
        version: 2,
        access: replacement.access_token,
        refresh: replacement
            .refresh_token
            .unwrap_or_else(|| predecessor.clone()),
        expires_at,
        last_refresh: sent_at,
        adopted_seed_sha256,
    };
    let bytes = serde_json::to_vec(&record)
        .map_err(|source| classified(SourceError::Malformed, &source))?;
    if bytes.len() > RECORD_BYTES {
        return Err(SourceError::Malformed);
    }
    save_bytes(&path, opened.lock, bytes).await?;
    Ok(RefreshResult::Refreshed(record))
}

async fn save_record(
    path: &Path,
    lock: PrivateFileLock,
    record: &OAuth2Record,
) -> Result<(), SourceError> {
    let bytes = serde_json::to_vec(record).map_err(|_source| SourceError::Malformed)?;
    save_bytes(path, lock, bytes).await
}

async fn save_bytes(path: &Path, lock: PrivateFileLock, bytes: Vec<u8>) -> Result<(), SourceError> {
    if bytes.len() > RECORD_BYTES {
        return Err(SourceError::Malformed);
    }
    let path = path.to_path_buf();
    let saved = tokio::task::spawn_blocking(move || {
        let _lock = lock;
        drop(dekopon_core::private_file::sweep_stale_temporaries(
            &path, None,
        ));
        dekopon_core::private_file::replace_private_file(&path, &bytes)
    })
    .await
    .map_err(|_source| SourceError::Internal)?;
    saved.map_err(|_source| {
        tracing::error!(name: "broker.credential.refresh_save_failed", target: "credential", event = "broker.credential.refresh_save_failed", "broker.credential.refresh_save_failed");
        SourceError::Io
    })
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
        if !super::headers_within_limits(&response) {
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
            let response: Option<ErrorResponse> = serde_json::from_slice(&body).ok();
            if response.as_ref().is_some_and(|value| {
                crate::credentials::REAUTHORIZATION_CODES.contains(&value.error.as_str())
            }) {
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

pub(super) fn inspect_boot(path: &Path, uid: u32, credential: &SecretDrn) {
    match read_record(path, uid) {
        Ok(OAuth2Record::Sealed { since, cause, .. }) => {
            tracing::info!(target: "dekopon_brokerd::audit", { audit.event = "broker.credential.sealed", credential = %credential, since, cause = cause.label(), instruction = "provide a new seed" }, "broker.credential.sealed");
        }
        Err(SourceError::Io)
            if std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => {
            tracing::error!(event = "broker_credential_record_invalid", credential = %credential, category = error.category(), instruction = "delete the record and provide a new seed", "broker.credential.record.invalid")
        }
        Ok(OAuth2Record::Live { .. }) => {}
    }
}

pub(crate) fn due_at(path: &Path, uid: u32) -> Result<Option<u64>, SourceError> {
    match read_record(path, uid) {
        Ok(OAuth2Record::Live {
            expires_at,
            last_refresh,
            ..
        }) => Ok(Some(
            last_refresh
                .saturating_add(CYCLE)
                .min(expires_at.saturating_sub(BUFFER)),
        )),
        Ok(OAuth2Record::Sealed { .. }) => Ok(None),
        Err(SourceError::Io)
            if std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) const fn retry_seconds() -> u64 {
    RETRY
}

pub(crate) fn now_seconds() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0)
}
