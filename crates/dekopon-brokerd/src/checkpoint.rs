use std::{
    fs::Metadata,
    io,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use dekopon_broker::{AuditError, AuditEvent, AuditLog, AuditRecord, FileAuditLog};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::Mutex,
};

use crate::socket;

/// Stable discriminator for the strict durable checkpoint document.
pub const CHECKPOINT_API_VERSION: &str = "dekopon.dev/audit-checkpoint/v1alpha1";
/// Hard checkpoint allocation and encoded-file ceiling.
pub const HARD_MAX_CHECKPOINT_BYTES: usize = 4 * 1024;
const SHA256_HEX_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum CheckpointApiVersion {
    #[serde(rename = "dekopon.dev/audit-checkpoint/v1alpha1")]
    V1Alpha1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StoredCheckpoint {
    api_version: CheckpointApiVersion,
    records: u64,
    head: Option<String>,
}

impl StoredCheckpoint {
    fn new(records: usize, head: Option<&str>) -> Result<Self, CheckpointError> {
        #[allow(
            clippy::map_err_ignore,
            reason = "TryFromIntError carries only out-of-range, which RecordOverflow already states"
        )]
        let records = u64::try_from(records).map_err(|_| CheckpointError::RecordOverflow)?;
        let checkpoint = Self {
            api_version: CheckpointApiVersion::V1Alpha1,
            records,
            head: head.map(ToOwned::to_owned),
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    #[allow(
        clippy::map_err_ignore,
        reason = "TryFromIntError carries only out-of-range, which RecordOverflow already states"
    )]
    pub fn records(&self) -> Result<usize, CheckpointError> {
        usize::try_from(self.records).map_err(|_| CheckpointError::RecordOverflow)
    }

    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    fn validate(&self) -> Result<(), CheckpointError> {
        let valid = match (self.records, self.head.as_deref()) {
            (0, None) => true,
            (0, Some(_)) | (_, None) => false,
            (_, Some(head)) => is_sha256(head),
        };
        if !valid {
            return Err(CheckpointError::InvalidState);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct CheckpointStore {
    path: PathBuf,
    temporary_path: PathBuf,
    expected_uid: u32,
    _lock: std::fs::File,
    writes: Mutex<()>,
}

impl CheckpointStore {
    pub async fn open(
        path: impl AsRef<Path>,
        lock_path: impl AsRef<Path>,
        expected_uid: u32,
    ) -> Result<(Self, Option<StoredCheckpoint>), CheckpointError> {
        let path = path.as_ref().to_path_buf();
        let lock_path = lock_path.as_ref().to_path_buf();
        if path == lock_path {
            return Err(CheckpointError::ConflictingPaths);
        }
        socket::validate_private_parent(&path, expected_uid)
            .map_err(CheckpointError::PathSecurity)?;
        socket::validate_private_parent(&lock_path, expected_uid)
            .map_err(CheckpointError::PathSecurity)?;
        let temporary_path = temporary_path(&path)?;
        if temporary_path == lock_path {
            return Err(CheckpointError::ConflictingPaths);
        }

        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let lock = options
            .open(&lock_path)
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        validate_metadata(
            &lock.metadata().await.map_err(io_error)?,
            &lock_path,
            expected_uid,
        )?;
        let lock = lock.into_std().await;
        lock.try_lock_exclusive()
            .map_err(|source| CheckpointError::Lock { source })?;
        remove_stale_temporary(&temporary_path, expected_uid).await?;

        let current = read_checkpoint(&path, expected_uid).await?;
        Ok((
            Self {
                path,
                temporary_path,
                expected_uid,
                _lock: lock,
                writes: Mutex::new(()),
            },
            current,
        ))
    }

    pub async fn write(&self, records: usize, head: Option<&str>) -> Result<(), CheckpointError> {
        let checkpoint = StoredCheckpoint::new(records, head)?;
        let _guard = self.writes.lock().await;
        remove_stale_temporary(&self.temporary_path, self.expected_uid).await?;
        self.stage(&checkpoint).await?;

        tokio::fs::rename(&self.temporary_path, &self.path)
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        let parent = self.path.parent().ok_or(CheckpointError::MissingParent)?;
        File::open(parent)
            .await
            .map_err(|source| CheckpointError::Io { source })?
            .sync_all()
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        let metadata = tokio::fs::symlink_metadata(&self.path)
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        validate_metadata(&metadata, &self.path, self.expected_uid)
    }

    async fn stage(&self, checkpoint: &StoredCheckpoint) -> Result<(), CheckpointError> {
        let mut bytes = serde_json::to_vec(checkpoint)
            .map_err(|source| CheckpointError::Serialize { source })?;
        bytes.push(b'\n');
        if bytes.len() > HARD_MAX_CHECKPOINT_BYTES {
            return Err(CheckpointError::TooLarge);
        }
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut temporary = options
            .open(&self.temporary_path)
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        validate_metadata(
            &temporary.metadata().await.map_err(io_error)?,
            &self.temporary_path,
            self.expected_uid,
        )?;
        temporary
            .write_all(&bytes)
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        temporary
            .flush()
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        temporary
            .sync_all()
            .await
            .map_err(|source| CheckpointError::Io { source })?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct CheckpointedAuditLog {
    audit: Arc<FileAuditLog>,
    checkpoint: Arc<CheckpointStore>,
    state: Mutex<CheckpointedState>,
}

#[derive(Debug, Default)]
struct CheckpointedState {
    poisoned: bool,
}

impl CheckpointedAuditLog {
    pub fn new(audit: Arc<FileAuditLog>, checkpoint: Arc<CheckpointStore>) -> Self {
        Self {
            audit,
            checkpoint,
            state: Mutex::new(CheckpointedState::default()),
        }
    }

    pub async fn checkpoint(&self) -> (usize, Option<String>) {
        self.audit.checkpoint().await
    }
}

impl AuditLog for CheckpointedAuditLog {
    async fn append(&self, event: AuditEvent) -> Result<AuditRecord, AuditError> {
        let mut state = self.state.lock().await;
        if state.poisoned {
            return Err(AuditError::Poisoned);
        }
        let record = self.audit.append(event).await?;
        #[allow(
            clippy::map_err_ignore,
            reason = "TryFromIntError carries only out-of-range, which SequenceOverflow already states"
        )]
        let records = usize::try_from(record.sequence).map_err(|_| AuditError::SequenceOverflow)?;
        if let Err(error) = self
            .checkpoint
            .write(records, Some(&record.record_hash))
            .await
        {
            // Poisoning is terminal for this process: every later append refuses until restart.
            // The audit record it wraps is already durable, so this line is the only account of
            // why the broker stopped being able to authorize anything.
            tracing::error!(
                event = "broker_checkpoint_poisoned",
                audit_records = records,
                error = %crate::error_chain(&error)
            );
            state.poisoned = true;
            return Err(AuditError::Io {
                source: io::Error::other(error),
            });
        }
        Ok(record)
    }
}

pub async fn reconcile(
    audit: &FileAuditLog,
    store: &CheckpointStore,
    stored: Option<&StoredCheckpoint>,
) -> Result<(usize, Option<String>), CheckpointError> {
    let current = audit.checkpoint().await;
    match stored {
        None if current.0 == 0 => store.write(0, None).await?,
        None => return Err(CheckpointError::MissingForNonEmptyAudit),
        Some(stored) => {
            let records = stored.records()?;
            // The size of the gap is classified before the hash is compared. An audit log can be
            // at most one append ahead of its checkpoint, and it retains exactly that window, so
            // a checkpoint further behind is a gap rather than a prefix anything could confirm —
            // and an operator needs to be told which of the two it is.
            if records != current.0 && records.checked_add(1) != Some(current.0) {
                if records > current.0 {
                    return Err(CheckpointError::AuditMismatch);
                }
                return Err(CheckpointError::AuditAheadByMultiple {
                    checkpoint_records: records,
                    audit_records: current.0,
                });
            }
            if !audit.contains_checkpoint(records, stored.head()).await {
                return Err(CheckpointError::AuditMismatch);
            }
            if records != current.0 || stored.head() != current.1.as_deref() {
                store.write(current.0, current.1.as_deref()).await?;
            }
        }
    }
    Ok(current)
}

fn temporary_path(path: &Path) -> Result<PathBuf, CheckpointError> {
    let name = path.file_name().ok_or(CheckpointError::MissingFileName)?;
    let mut temporary = name.to_os_string();
    temporary.push(".tmp");
    Ok(path.with_file_name(temporary))
}

async fn read_checkpoint(
    path: &Path,
    expected_uid: u32,
) -> Result<Option<StoredCheckpoint>, CheckpointError> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CheckpointError::Io { source }),
    };
    validate_metadata(
        &file.metadata().await.map_err(io_error)?,
        path,
        expected_uid,
    )?;
    let mut bytes = Vec::new();
    file.take((HARD_MAX_CHECKPOINT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| CheckpointError::Io { source })?;
    if bytes.len() > HARD_MAX_CHECKPOINT_BYTES {
        return Err(CheckpointError::TooLarge);
    }
    if bytes.last() != Some(&b'\n') || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n') {
        return Err(CheckpointError::InvalidEncoding);
    }
    bytes.pop();
    let checkpoint = serde_json::from_slice::<StoredCheckpoint>(&bytes)
        .map_err(|source| CheckpointError::Decode { source })?;
    checkpoint.validate()?;
    Ok(Some(checkpoint))
}

async fn remove_stale_temporary(path: &Path, expected_uid: u32) -> Result<(), CheckpointError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(CheckpointError::Io { source }),
    };
    validate_metadata(&metadata, path, expected_uid)?;
    tokio::fs::remove_file(path)
        .await
        .map_err(|source| CheckpointError::Io { source })
}

fn validate_metadata(
    metadata: &Metadata,
    path: &Path,
    expected_uid: u32,
) -> Result<(), CheckpointError> {
    if !metadata.file_type().is_file()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(CheckpointError::InsecureFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == SHA256_HEX_BYTES
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn io_error(source: io::Error) -> CheckpointError {
    CheckpointError::Io { source }
}

/// Secure checkpoint storage or audit reconciliation failure.
#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint and checkpoint-lock paths must differ")]
    ConflictingPaths,
    #[error("checkpoint path has no parent")]
    MissingParent,
    #[error("checkpoint path has no file name")]
    MissingFileName,
    #[error("checkpoint filesystem path is insecure")]
    PathSecurity(#[source] socket::SocketError),
    #[error(
        "checkpoint or lock file is not private, server-owned, regular, and single-link: {path}"
    )]
    InsecureFile { path: PathBuf },
    #[error("checkpoint lock is already held by another broker")]
    Lock {
        #[source]
        source: io::Error,
    },
    #[error("checkpoint is too large")]
    TooLarge,
    #[error("checkpoint is not one newline-terminated JSON object")]
    InvalidEncoding,
    #[error("checkpoint JSON is invalid")]
    Decode {
        #[source]
        source: serde_json::Error,
    },
    #[error("checkpoint JSON could not be serialized")]
    Serialize {
        #[source]
        source: serde_json::Error,
    },
    #[error("checkpoint record count does not fit this platform")]
    RecordOverflow,
    #[error("checkpoint count and SHA-256 head are inconsistent")]
    InvalidState,
    #[error("non-empty audit log has no checkpoint; explicit recovery is required")]
    MissingForNonEmptyAudit,
    #[error("stored checkpoint is not a prefix of the verified audit chain")]
    AuditMismatch,
    #[error(
        "audit has {audit_records} records but checkpoint has {checkpoint_records}; the recoverable crash window is one record"
    )]
    AuditAheadByMultiple {
        checkpoint_records: usize,
        audit_records: usize,
    },
    #[error("checkpoint file operation failed")]
    Io {
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, sync::Arc};

    use dekopon_broker::{AuditError, AuditEvent, AuditLog, FileAuditLog};
    use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};

    use super::{
        CheckpointError, CheckpointStore, CheckpointedAuditLog, StoredCheckpoint, reconcile,
    };
    use crate::current_uid;

    fn decision(invocation: &str) -> AuditEvent {
        AuditEvent::Decision {
            invocation: invocation
                .parse::<InvocationId>()
                .expect("valid invocation fixture"),
            trace: "trace-checkpoint"
                .parse::<TraceId>()
                .expect("valid trace fixture"),
            principal: Some(
                "caller"
                    .parse::<PrincipalId>()
                    .expect("valid principal fixture"),
            ),
            actor: Some(Actor::Agent {
                agent: "checkpoint-test"
                    .parse::<AgentId>()
                    .expect("valid agent fixture"),
            }),
            via: None,
            attested_subject: None,
            capability: "echo.echo"
                .parse::<CapabilityId>()
                .expect("valid capability fixture"),
            secret: None,
            secret_sink: None,
            provider: None,
            authorized_by: Some(
                "broker"
                    .parse::<PrincipalId>()
                    .expect("valid principal fixture"),
            ),
            decision_id: format!("decision-{invocation}"),
            policy_revision: Some("policy-checkpoint".to_owned()),
            policy_ids: Vec::new(),
            policy_digest: None,
            allowed: false,
            reason: Some("policy-denied".to_owned()),
            decision_digest: format!("sha256:{}", "a".repeat(64)),
            storage_scope_commitment: None,
            storage: None,
        }
    }

    #[tokio::test]
    async fn checkpoint_is_private_strict_locked_and_atomic() {
        let uid = current_uid();
        let directory = tempfile::tempdir().expect("create checkpoint fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("secure checkpoint directory");
        let path = directory.path().join("checkpoint.json");
        let lock = directory.path().join("checkpoint.lock");
        let (store, current) = CheckpointStore::open(&path, &lock, uid)
            .await
            .expect("open new checkpoint");
        assert!(current.is_none());
        let head = format!("sha256:{}", "a".repeat(64));
        store.write(1, Some(&head)).await.expect("write checkpoint");
        assert!(matches!(
            CheckpointStore::open(&path, &lock, uid).await,
            Err(CheckpointError::Lock { .. })
        ));
        drop(store);

        let (store, current) = CheckpointStore::open(&path, &lock, uid)
            .await
            .expect("reopen checkpoint");
        let current = current.expect("checkpoint exists");
        assert_eq!(current.records().expect("count fits"), 1);
        assert_eq!(current.head(), Some(head.as_str()));
        assert!(!path.with_file_name("checkpoint.json.tmp").exists());

        let next_head = format!("sha256:{}", "b".repeat(64));
        let next = StoredCheckpoint::new(2, Some(&next_head)).expect("valid next checkpoint");
        store
            .stage(&next)
            .await
            .expect("synchronize staged replacement");
        assert!(path.with_file_name("checkpoint.json.tmp").exists());
        let retained = fs::read(&path).expect("read retained checkpoint");
        let retained = serde_json::from_slice::<StoredCheckpoint>(&retained[..retained.len() - 1])
            .expect("retained checkpoint decodes");
        assert_eq!(retained.records().expect("count fits"), 1);
        drop(store);
        let (store, current) = CheckpointStore::open(&path, &lock, uid)
            .await
            .expect("recover an interrupted pre-rename write");
        assert_eq!(
            current
                .expect("old checkpoint remains")
                .records()
                .expect("count fits"),
            1
        );
        assert!(!path.with_file_name("checkpoint.json.tmp").exists());
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&path)
                    .expect("checkpoint metadata")
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
        drop(store);

        fs::write(
            &path,
            b"{\"apiVersion\":\"dekopon.dev/audit-checkpoint/v1alpha1\",\"records\":1,\"head\":null,\"extra\":true}\n",
        )
        .expect("mutate checkpoint");
        assert!(matches!(
            CheckpointStore::open(&path, &lock, uid).await,
            Err(CheckpointError::Decode { .. })
        ));
        fs::write(&path, vec![b'a'; super::HARD_MAX_CHECKPOINT_BYTES + 1])
            .expect("write oversized checkpoint");
        assert!(matches!(
            CheckpointStore::open(&path, &lock, uid).await,
            Err(CheckpointError::TooLarge)
        ));
    }

    #[tokio::test]
    async fn reconciliation_rejects_tampered_and_replaced_checkpoints() {
        use std::os::unix::fs::symlink;

        let uid = current_uid();
        let directory = tempfile::tempdir().expect("create tamper fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("secure checkpoint directory");
        let audit = FileAuditLog::open(directory.path().join("audit.jsonl"), 8, 16 * 1024)
            .await
            .expect("open audit");
        audit
            .append(decision("invoke-tamper"))
            .await
            .expect("append audit record");
        let current = audit.checkpoint().await;
        let path = directory.path().join("checkpoint.json");
        let lock = directory.path().join("checkpoint.lock");
        let (store, _) = CheckpointStore::open(&path, &lock, uid)
            .await
            .expect("open checkpoint");
        store
            .write(current.0, current.1.as_deref())
            .await
            .expect("write matching checkpoint");
        drop(store);

        let tampered = StoredCheckpoint::new(1, Some(&format!("sha256:{}", "f".repeat(64))))
            .expect("well-formed tampered checkpoint");
        let mut bytes = serde_json::to_vec(&tampered).expect("serialize tampered checkpoint");
        bytes.push(b'\n');
        fs::write(&path, bytes).expect("replace checkpoint content");
        let (store, stored) = CheckpointStore::open(&path, &lock, uid)
            .await
            .expect("well-formed checkpoint opens");
        assert!(matches!(
            reconcile(&audit, &store, stored.as_ref()).await,
            Err(CheckpointError::AuditMismatch)
        ));
        drop(store);

        let replacement = directory.path().join("replacement.json");
        fs::write(&replacement, b"replacement").expect("write replacement target");
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600))
            .expect("secure replacement target");
        fs::remove_file(&path).expect("remove checkpoint before symlink replacement");
        symlink(&replacement, &path).expect("replace checkpoint with symlink");
        assert!(matches!(
            CheckpointStore::open(&path, &lock, uid).await,
            Err(CheckpointError::Io { .. })
        ));

        fs::remove_file(&path).expect("remove symlink replacement");
        fs::hard_link(&replacement, &path).expect("replace checkpoint with hard link");
        assert!(matches!(
            CheckpointStore::open(&path, &lock, uid).await,
            Err(CheckpointError::InsecureFile { .. })
        ));

        fs::remove_file(&path).expect("remove hard-linked checkpoint");
        fs::remove_file(&lock).expect("remove regular lock");
        symlink(&replacement, &lock).expect("replace lock with symlink");
        assert!(matches!(
            CheckpointStore::open(&path, &lock, uid).await,
            Err(CheckpointError::Io { .. })
        ));
        fs::remove_file(&lock).expect("remove lock symlink");
        fs::hard_link(&replacement, &lock).expect("replace lock with hard link");
        assert!(matches!(
            CheckpointStore::open(&path, &lock, uid).await,
            Err(CheckpointError::InsecureFile { .. })
        ));
    }

    #[tokio::test]
    async fn reconcile_recovers_ahead_audit_and_rejects_missing_nonempty_checkpoint() {
        let uid = current_uid();
        let directory = tempfile::tempdir().expect("create reconcile fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("secure checkpoint directory");
        let audit_path = directory.path().join("audit.jsonl");
        let checkpoint_path = directory.path().join("checkpoint.json");
        let lock_path = directory.path().join("checkpoint.lock");
        let audit = FileAuditLog::open(&audit_path, 8, 16 * 1024)
            .await
            .expect("open audit");
        let (store, stored) = CheckpointStore::open(&checkpoint_path, &lock_path, uid)
            .await
            .expect("open checkpoint");
        reconcile(&audit, &store, stored.as_ref())
            .await
            .expect("initialize empty checkpoint");
        audit
            .append(decision("invoke-ahead"))
            .await
            .expect("append audit ahead of checkpoint");
        drop(store);

        let (store, stored) = CheckpointStore::open(&checkpoint_path, &lock_path, uid)
            .await
            .expect("reopen stale checkpoint");
        let current = reconcile(&audit, &store, stored.as_ref())
            .await
            .expect("verified empty prefix advances checkpoint");
        assert_eq!(current.0, 1);
        audit
            .append(decision("invoke-ahead-again"))
            .await
            .expect("append beyond nonempty checkpoint");
        drop(store);
        let (store, stored) = CheckpointStore::open(&checkpoint_path, &lock_path, uid)
            .await
            .expect("reopen nonempty stale checkpoint");
        let current = reconcile(&audit, &store, stored.as_ref())
            .await
            .expect("verified nonempty prefix advances checkpoint");
        assert_eq!(current.0, 2);
        drop(store);
        drop(audit);

        let missing = directory.path().join("missing-checkpoint.json");
        let missing_lock = directory.path().join("missing-checkpoint.lock");
        let audit = FileAuditLog::open(&audit_path, 8, 16 * 1024)
            .await
            .expect("reopen nonempty audit");
        let (store, stored) = CheckpointStore::open(&missing, &missing_lock, uid)
            .await
            .expect("open absent checkpoint");
        assert!(matches!(
            reconcile(&audit, &store, stored.as_ref()).await,
            Err(CheckpointError::MissingForNonEmptyAudit)
        ));

        let gap = directory.path().join("gap-checkpoint.json");
        let gap_lock = directory.path().join("gap-checkpoint.lock");
        let (gap_store, _) = CheckpointStore::open(&gap, &gap_lock, uid)
            .await
            .expect("open gap checkpoint");
        gap_store
            .write(0, None)
            .await
            .expect("write stale empty checkpoint");
        let stale = StoredCheckpoint::new(0, None).expect("valid stale checkpoint");
        assert!(matches!(
            reconcile(&audit, &gap_store, Some(&stale)).await,
            Err(CheckpointError::AuditAheadByMultiple {
                checkpoint_records: 0,
                audit_records: 2
            })
        ));

        // A checkpoint claiming more records than the audit holds is the other direction: the
        // audit lost records, which is a mismatch rather than a checkpoint that fell behind.
        let ahead = StoredCheckpoint::new(5, Some(&format!("sha256:{}", "c".repeat(64))))
            .expect("well-formed ahead checkpoint");
        assert!(matches!(
            reconcile(&audit, &gap_store, Some(&ahead)).await,
            Err(CheckpointError::AuditMismatch)
        ));
    }

    #[tokio::test]
    async fn checkpointed_audit_synchronizes_every_append() {
        let uid = current_uid();
        let directory = tempfile::tempdir().expect("create wrapped audit fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("secure checkpoint directory");
        let audit = Arc::new(
            FileAuditLog::open(directory.path().join("audit.jsonl"), 8, 16 * 1024)
                .await
                .expect("open audit"),
        );
        let checkpoint_path = directory.path().join("checkpoint.json");
        let lock_path = directory.path().join("checkpoint.lock");
        let (store, stored) = CheckpointStore::open(&checkpoint_path, &lock_path, uid)
            .await
            .expect("open checkpoint");
        reconcile(&audit, &store, stored.as_ref())
            .await
            .expect("initialize checkpoint");
        let wrapped = CheckpointedAuditLog::new(Arc::clone(&audit), Arc::new(store));
        let record = wrapped
            .append(decision("invoke-wrapped"))
            .await
            .expect("wrapped append succeeds");
        assert_eq!(wrapped.checkpoint().await.0, 1);
        let bytes = fs::read(&checkpoint_path).expect("read synchronized checkpoint");
        let stored = serde_json::from_slice::<StoredCheckpoint>(&bytes[..bytes.len() - 1])
            .expect("checkpoint decodes");
        assert_eq!(stored.records().expect("count fits"), 1);
        assert_eq!(stored.head(), Some(record.record_hash.as_str()));

        let temporary_path = checkpoint_path.with_file_name("checkpoint.json.tmp");
        fs::create_dir(&temporary_path).expect("block the next checkpoint stage");
        let error = wrapped
            .append(decision("invoke-checkpoint-failure"))
            .await
            .expect_err("checkpoint failure follows the durable audit append");
        assert!(matches!(error, AuditError::Io { .. }));
        let error = wrapped
            .append(decision("invoke-after-poison"))
            .await
            .expect_err("checkpointed audit remains poisoned");
        assert!(matches!(error, AuditError::Poisoned));
        assert_eq!(audit.checkpoint().await.0, 2);
        drop(wrapped);
        fs::remove_dir(&temporary_path).expect("remove checkpoint failure fixture");

        let (store, stored) = CheckpointStore::open(&checkpoint_path, &lock_path, uid)
            .await
            .expect("reopen checkpoint after failed update");
        let recovered = reconcile(&audit, &store, stored.as_ref())
            .await
            .expect("recover durable audit ahead of checkpoint");
        assert_eq!(recovered.0, 2);
    }
}
