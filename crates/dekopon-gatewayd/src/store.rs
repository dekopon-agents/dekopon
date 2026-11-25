mod convert;
mod lock;
mod record;
mod schema;

use std::{
    collections::BTreeMap,
    fs::{DirBuilder, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use rusqlite::{OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::oneshot;

use crate::{memory::journal::FollowMarker, transport::ThreadClaim};

pub(crate) use convert::{Conversion, FollowConversion, LegacyInputs};
pub use record::ConversationKey;
use record::RecordVersion;
pub(crate) use record::Snapshot;
pub(crate) use record::{
    AnchorRow, Change, FollowRow, JobRow, PendingDelivery, WakeRow, WakeRowKind, WatchRow,
};
pub use schema::SchemaVersion;
use schema::{STEPS, Step};

const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const FIRST_SERVED: &str = "first-served";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not read or write the durable store")]
    Io { kind: io::ErrorKind },
    #[error("the durable store refused a statement")]
    Sqlite { code: Option<i32> },
    #[error("another gateway owns the durable store at {path}")]
    Owned { path: PathBuf },
    #[error("the durable store schema {found} is newer than this gateway's {known}")]
    SchemaTooNew {
        found: SchemaVersion,
        known: SchemaVersion,
    },
    #[error("migrating the durable store from schema {from} failed; restore {backup} to roll back")]
    MigrationFailed {
        from: SchemaVersion,
        backup: PathBuf,
        #[source]
        source: Box<StoreError>,
    },
    #[error("{} durable records could not be upgraded; restore {backup} to roll back", records.len())]
    Unupgradable {
        records: Vec<ConversationKey>,
        backup: PathBuf,
    },
    #[error(
        "legacy wake store line {line} did not parse; remove {path} to reset every pending wake"
    )]
    LegacyWakes { path: PathBuf, line: usize },
    #[error("a durable record did not parse")]
    Corrupt { conversation: ConversationKey },
}

impl StoreError {
    pub(crate) const fn category(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::Sqlite { .. } => "sqlite",
            Self::Owned { .. } => "owned",
            Self::SchemaTooNew { .. } => "schema-too-new",
            Self::MigrationFailed { .. } => "migration-failed",
            Self::Unupgradable { .. } => "unupgradable",
            Self::LegacyWakes { .. } => "legacy-wakes",
            Self::Corrupt { .. } => "corrupt",
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite {
            code: error.sqlite_error().map(|error| error.extended_code),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    Off,
    #[default]
    Normal,
    Full,
}

impl SyncMode {
    const fn pragma(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Durability {
    Durable,
    Memory,
}

impl Durability {
    pub(crate) fn of(memory: crate::config::MemoryPolicy) -> Self {
        match memory.window().map(|window| window.recall) {
            Some(crate::config::RecallSource::Journal) => Self::Durable,
            Some(crate::config::RecallSource::None | crate::config::RecallSource::Platform)
            | None => Self::Memory,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DurableSettings {
    pub(crate) path: PathBuf,
    pub(crate) sync: SyncMode,
}

pub(crate) enum Persistence {
    Durable(Box<Acquired>),
    Memory,
}

pub(crate) struct DurableStore {
    connection: Mutex<rusqlite::Connection>,
    migrated: bool,
    fatal: StoreFatal,
}

pub(crate) struct Acquired {
    pub(crate) store: Arc<DurableStore>,
    pub(crate) report: BootReport,
    pub(crate) fatal: FatalWatch,
}

pub(crate) struct Restored {
    pub(crate) wakes: Vec<(ConversationKey, WakeRow)>,
    pub(crate) follows: Vec<(Option<ConversationKey>, FollowRow)>,
    pub(crate) ownership: Vec<(ConversationKey, ThreadClaim)>,
    pub(crate) interrupted_jobs: Vec<(ConversationKey, JobRow)>,
}

pub(crate) struct BootReport {
    pub(crate) schema: SchemaVersion,
    pub(crate) migration: Option<Migrated>,
    pub(crate) conversion: Option<Conversion>,
}

pub(crate) struct Migrated {
    pub(crate) from: SchemaVersion,
    pub(crate) backup: PathBuf,
}

struct StoreFatal {
    first: Mutex<Option<oneshot::Sender<StoreError>>>,
}

pub(crate) struct FatalWatch(oneshot::Receiver<StoreError>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StoreFailed;

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Raised {
    First,
    Already,
}

impl Persistence {
    pub(crate) fn at_boot(
        settings: Option<&DurableSettings>,
        legacy: &LegacyInputs,
    ) -> Result<Self, StoreError> {
        match settings {
            Some(settings) => Ok(Self::Durable(Box::new(DurableStore::acquire(
                settings, legacy,
            )?))),
            None => Ok(Self::Memory),
        }
    }
}

impl DurableStore {
    pub(crate) fn acquire(
        settings: &DurableSettings,
        legacy: &LegacyInputs,
    ) -> Result<Acquired, StoreError> {
        Self::acquire_with(settings, legacy, STEPS)
    }

    fn acquire_with(
        settings: &DurableSettings,
        legacy: &LegacyInputs,
        steps: &[Step],
    ) -> Result<Acquired, StoreError> {
        let path = &settings.path;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let mut connection = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        lock::own(&mut connection, path, settings.sync)?;
        let (schema, migration) = upgrade(&mut connection, path, steps)?;
        check_records(&connection, path, schema)?;
        let now_ms = millis(SystemTime::now());
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let conversion = legacy.convert(&transaction, now_ms)?;
        transaction.commit()?;
        if let Some(conversion) = &conversion {
            legacy.retire();
            tracing::info!(
                event = "gateway_store_converted",
                store.conversations = conversion.conversations.len(),
                store.wakes = conversion.wakes,
                store.follows.retained = match conversion.follows {
                    FollowConversion::None => 0,
                    FollowConversion::Retained { markers } => markers,
                },
                store.ownership.lost = 0,
                "gateway.store.converted"
            );
        }
        let (sender, receiver) = oneshot::channel();
        Ok(Acquired {
            store: Arc::new(Self {
                connection: Mutex::new(connection),
                migrated: migration.is_some(),
                fatal: StoreFatal {
                    first: Mutex::new(Some(sender)),
                },
            }),
            report: BootReport {
                schema,
                migration,
                conversion,
            },
            fatal: FatalWatch(receiver),
        })
    }

    pub(crate) fn restore(&self, now: SystemTime) -> Result<Restored, StoreError> {
        let now_ms = millis(now);
        let retention_ms = u64::try_from(RETENTION.as_millis()).unwrap_or(u64::MAX);
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut restored = Restored {
            wakes: Vec::new(),
            follows: Vec::new(),
            ownership: Vec::new(),
            interrupted_jobs: Vec::new(),
        };
        let records = read_records(&transaction)?;
        for (key, stored) in records {
            let mut snapshot = stored.map_err(|()| StoreError::Corrupt {
                conversation: key.clone(),
            })?;
            if snapshot.expire(now_ms, retention_ms) {
                write_record(&transaction, &key, &snapshot, now_ms)?;
            }
            for wake in snapshot.wakes {
                restored.wakes.push((key.clone(), wake));
            }
            for follow in snapshot.follows {
                restored.follows.push((Some(key.clone()), follow));
            }
            for claim in snapshot.ownership {
                restored.ownership.push((key.clone(), claim));
            }
            for job in snapshot.jobs {
                restored.interrupted_jobs.push((key.clone(), job));
            }
        }
        transaction.execute(
            "DELETE FROM retained_follows WHERE json_extract(marker, '$.untilMs') <= ?1",
            [i64::try_from(now_ms).unwrap_or(i64::MAX)],
        )?;
        let mut statement = transaction.prepare("SELECT key, marker FROM retained_follows")?;
        let follows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (key, marker) in follows {
            if let Ok(marker) = serde_json::from_str::<FollowMarker>(&marker) {
                restored.follows.push((None, FollowRow { key, marker }));
            }
        }
        transaction.commit()?;
        Ok(restored)
    }

    pub(crate) fn commit(
        &self,
        changes: Vec<(ConversationKey, Change)>,
    ) -> Result<(), StoreFailed> {
        if changes.is_empty() {
            return Ok(());
        }
        if self.fatal.first.lock().is_none() {
            return Err(StoreFailed);
        }
        let result = self.apply(changes);
        result.map_err(|error| {
            self.failed(error);
            StoreFailed
        })
    }

    pub(crate) fn forget_retained_follow(&self, key: &str) -> Result<(), StoreFailed> {
        let result = self
            .connection
            .lock()
            .execute("DELETE FROM retained_follows WHERE key = ?1", [key])
            .map(drop)
            .map_err(StoreError::from);
        result.map_err(|error| {
            self.failed(error);
            StoreFailed
        })
    }

    pub(crate) fn retain_follow(&self, key: &str, marker: FollowMarker) -> Result<(), StoreFailed> {
        let result = serde_json::to_string(&marker)
            .map_err(|error| StoreError::from(io::Error::from(error)))
            .and_then(|marker| {
                self.connection
                    .lock()
                    .execute(
                        "INSERT OR REPLACE INTO retained_follows (key, marker) VALUES (?1, ?2)",
                        params![key, marker],
                    )
                    .map(drop)
                    .map_err(StoreError::from)
            });
        result.map_err(|error| {
            self.failed(error);
            StoreFailed
        })
    }

    fn apply(&self, changes: Vec<(ConversationKey, Change)>) -> Result<(), StoreError> {
        let now_ms = millis(SystemTime::now());
        let mut grouped: BTreeMap<ConversationKey, Vec<Change>> = BTreeMap::new();
        for (key, change) in changes {
            grouped.entry(key).or_default().push(change);
        }
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, changes) in grouped {
            let mut snapshot = read_record(&transaction, &key)?.unwrap_or_default();
            for change in changes {
                snapshot.apply(change);
            }
            write_record(&transaction, &key, &snapshot, now_ms)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn first_served(&self, at: SystemTime) -> Result<(), StoreFailed> {
        if !self.migrated {
            return Ok(());
        }
        let result = self
            .connection
            .lock()
            .execute(
                "INSERT OR IGNORE INTO meta (name, value) VALUES (?1, ?2)",
                params![
                    format!("{FIRST_SERVED}-{}", SchemaVersion::CURRENT),
                    millis(at).to_string()
                ],
            )
            .map(drop)
            .map_err(StoreError::from);
        result.map_err(|error| {
            self.failed(error);
            StoreFailed
        })
    }

    fn failed(&self, error: StoreError) {
        tracing::error!(
            event = "gateway_store_failed",
            error = %error,
            error.category = error.category(),
            "gateway.store.failed"
        );
        match self.raise(error) {
            Raised::First | Raised::Already => {}
        }
    }

    fn raise(&self, error: StoreError) -> Raised {
        match self.fatal.first.lock().take() {
            Some(sender) => {
                drop(sender.send(error));
                Raised::First
            }
            None => Raised::Already,
        }
    }
}

impl FatalWatch {
    pub(crate) async fn failed(&mut self) -> StoreError {
        match (&mut self.0).await {
            Ok(error) => error,
            Err(_) => std::future::pending().await,
        }
    }
}

fn upgrade(
    connection: &mut rusqlite::Connection,
    path: &Path,
    steps: &[Step],
) -> Result<(SchemaVersion, Option<Migrated>), StoreError> {
    let found = SchemaVersion::read(connection)?;
    let known = SchemaVersion::latest(steps);
    if found > known {
        return Err(StoreError::SchemaTooNew { found, known });
    }
    if found == known {
        return Ok((found, None));
    }
    let backup = (found != SchemaVersion::EMPTY)
        .then(|| backup(connection, path, found))
        .transpose()?;
    let migrate = |connection: &mut rusqlite::Connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version = found.migrate(&transaction, steps)?;
        transaction.commit()?;
        Ok::<_, StoreError>(version)
    };
    let version = match (migrate(connection), backup) {
        (Ok(version), backup) => (version, backup),
        (Err(source), Some(backup)) => {
            return Err(StoreError::MigrationFailed {
                from: found,
                backup,
                source: Box::new(source),
            });
        }
        (Err(source), None) => return Err(source),
    };
    match version {
        (version, Some(backup)) => {
            tracing::info!(
                event = "gateway_store_migrated",
                store.schema.from = %found,
                store.schema.to = %version,
                "gateway.store.migrated"
            );
            Ok((
                version,
                Some(Migrated {
                    from: found,
                    backup,
                }),
            ))
        }
        (version, None) => Ok((version, None)),
    }
}

fn backup(
    connection: &rusqlite::Connection,
    path: &Path,
    version: SchemaVersion,
) -> Result<PathBuf, StoreError> {
    let backup = version.backup_path(path);
    if !backup.exists() {
        connection.execute("VACUUM INTO ?1", [backup.to_string_lossy()])?;
    }
    Ok(backup)
}

fn check_records(
    connection: &rusqlite::Connection,
    path: &Path,
    schema: SchemaVersion,
) -> Result<(), StoreError> {
    let unupgradable: Vec<ConversationKey> = read_records(connection)?
        .into_iter()
        .filter_map(|(key, snapshot)| snapshot.is_err().then_some(key))
        .collect();
    if unupgradable.is_empty() {
        return Ok(());
    }
    Err(StoreError::Unupgradable {
        records: unupgradable,
        backup: backup(connection, path, schema)?,
    })
}

type Stored = Result<Snapshot, ()>;

fn read_records(
    connection: &rusqlite::Connection,
) -> Result<Vec<(ConversationKey, Stored)>, StoreError> {
    let mut statement = connection.prepare("SELECT key, version, snapshot FROM conversations")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut records = Vec::new();
    for row in rows {
        let (key, version, snapshot) = row?;
        records.push((ConversationKey::stored(key), decode(version, &snapshot)));
    }
    Ok(records)
}

fn decode(version: i64, snapshot: &str) -> Stored {
    match RecordVersion::of(version) {
        Some(version) if version == RecordVersion::CURRENT => {
            serde_json::from_str(snapshot).map_err(drop)
        }
        Some(_) | None => Err(()),
    }
}

fn read_record(
    connection: &rusqlite::Connection,
    key: &ConversationKey,
) -> Result<Option<Snapshot>, StoreError> {
    let stored: Option<(i64, String)> = connection
        .query_row(
            "SELECT version, snapshot FROM conversations WHERE key = ?1",
            [key.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    stored
        .map(|(version, snapshot)| {
            decode(version, &snapshot).map_err(|()| StoreError::Corrupt {
                conversation: key.clone(),
            })
        })
        .transpose()
}

fn write_record(
    connection: &rusqlite::Connection,
    key: &ConversationKey,
    snapshot: &Snapshot,
    now_ms: u64,
) -> Result<(), StoreError> {
    if snapshot.is_empty() {
        connection.execute(
            "DELETE FROM conversations WHERE key = ?1",
            [key.to_string()],
        )?;
        return Ok(());
    }
    connection.execute(
        "INSERT INTO conversations (key, version, snapshot, updated_ms) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET version = excluded.version,
            snapshot = excluded.snapshot, updated_ms = excluded.updated_ms",
        params![
            key.to_string(),
            RecordVersion::CURRENT.get(),
            serde_json::to_string(snapshot).map_err(io::Error::from)?,
            i64::try_from(now_ms).unwrap_or(i64::MAX),
        ],
    )?;
    Ok(())
}

pub(crate) fn millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
impl DurableStore {
    pub(crate) fn hold_for_test(&self) -> parking_lot::MutexGuard<'_, rusqlite::Connection> {
        self.connection.lock()
    }

    pub(crate) fn deny_writes_for_test(&self, denied: bool) {
        let connection = self.connection.lock();
        let sql = if denied {
            "CREATE TRIGGER IF NOT EXISTS deny_insert BEFORE INSERT ON conversations
                BEGIN SELECT RAISE(ABORT, 'denied'); END;
             CREATE TRIGGER IF NOT EXISTS deny_update BEFORE UPDATE ON conversations
                BEGIN SELECT RAISE(ABORT, 'denied'); END;
             CREATE TRIGGER IF NOT EXISTS deny_delete BEFORE DELETE ON conversations
                BEGIN SELECT RAISE(ABORT, 'denied'); END;"
        } else {
            "DROP TRIGGER IF EXISTS deny_insert;
             DROP TRIGGER IF EXISTS deny_update;
             DROP TRIGGER IF EXISTS deny_delete;"
        };
        connection
            .execute_batch(sql)
            .expect("the test trigger applies");
    }

    pub(crate) fn records_for_test(&self) -> Vec<ConversationKey> {
        let connection = self.connection.lock();
        read_records(&connection)
            .expect("records read")
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    }

    pub(crate) fn snapshot_for_test(&self, key: &ConversationKey) -> Option<Snapshot> {
        read_record(&self.connection.lock(), key).expect("record read")
    }

    pub(crate) fn meta_for_test(&self, name: &str) -> Vec<String> {
        let connection = self.connection.lock();
        let mut statement = connection
            .prepare("SELECT value FROM meta WHERE name LIKE ?1")
            .expect("prepared");
        statement
            .query_map([format!("{name}%")], |row| row.get(0))
            .expect("queried")
            .collect::<Result<_, _>>()
            .expect("read")
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::{Acquired, DurableSettings, DurableStore, LegacyInputs, StoreError, schema::Step};

    pub(crate) use super::schema::{STEPS, SchemaVersion, Step as MigrationStep};

    pub(crate) fn acquire_with(
        settings: &DurableSettings,
        legacy: &LegacyInputs,
        steps: &[Step],
    ) -> Result<Acquired, StoreError> {
        DurableStore::acquire_with(settings, legacy, steps)
    }
}
