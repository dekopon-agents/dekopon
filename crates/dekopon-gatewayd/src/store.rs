#![expect(
    dead_code,
    reason = "skeleton: state/durable-store wires the store into boot and serving"
)]

mod convert;
mod ledger;
mod lock;
mod record;
mod schema;

use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use parking_lot::Mutex;
use thiserror::Error;

use crate::transport::ThreadClaim;

pub(crate) use convert::{Conversion, LegacyInputs};
#[expect(unused_imports, reason = "skeleton: state/spend-slice1")]
pub(crate) use ledger::{Hold, HoldId, HoldOutcome, Ledger, SpendMicros};
pub(crate) use lock::StoreOwner;
pub use record::ConversationKey;
#[expect(unused_imports, reason = "skeleton: state/durable-store")]
pub(crate) use record::{
    AnchorRow, Change, FollowRow, JobRow, PendingDelivery, Snapshot, WakeRow, WakeRowKind, WatchRow,
};
pub use schema::SchemaVersion;

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

pub(crate) struct DurableStore {
    owner: StoreOwner,
    connection: Mutex<rusqlite::Connection>,
}

pub(crate) struct Restored {
    pub(crate) wakes: Vec<WakeRow>,
    pub(crate) follows: Vec<FollowRow>,
    pub(crate) ownership: Vec<(ConversationKey, ThreadClaim)>,
    pub(crate) interrupted_jobs: Vec<JobRow>,
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

impl DurableStore {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn acquire(
        path: &Path,
        legacy: &LegacyInputs,
    ) -> Result<(Self, BootReport), StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn restore(&self, now: SystemTime) -> Result<Restored, StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn commit(&self, changes: Vec<(ConversationKey, Change)>) -> Result<(), StoreError> {
        todo!()
    }
}
