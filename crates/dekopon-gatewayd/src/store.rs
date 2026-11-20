#![expect(
    dead_code,
    reason = "skeleton: state/durable-store wires the store into boot and serving"
)]

mod convert;
mod ledger;
mod lock;
mod record;
mod schema;

use std::{io, path::PathBuf, sync::Arc, time::SystemTime};

use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::oneshot;

use crate::transport::ThreadClaim;

#[expect(unused_imports, reason = "skeleton: state/durable-store")]
pub(crate) use convert::FollowConversion;
pub(crate) use convert::{Conversion, LegacyInputs};
#[expect(unused_imports, reason = "skeleton: state/spend-slice1")]
pub(crate) use ledger::{ChargeRow, Hold, HoldId, Ledger, Provenance, Settlement};
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SyncMode {
    Off,
    #[default]
    Normal,
    Full,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreWarning {
    WaitsLostOnRestart,
}

pub(crate) struct DurableStore {
    owner: StoreOwner,
    connection: Mutex<rusqlite::Connection>,
    sync: SyncMode,
    fatal: StoreFatal,
}

pub(crate) struct Acquired {
    pub(crate) store: Arc<DurableStore>,
    pub(crate) report: BootReport,
    pub(crate) fatal: FatalWatch,
}

pub(crate) struct Restored {
    pub(crate) wakes: Vec<WakeRow>,
    pub(crate) follows: Vec<FollowRow>,
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
    pub(crate) at: SystemTime,
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
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn at_boot(
        settings: Option<&DurableSettings>,
        legacy: &LegacyInputs,
    ) -> Result<Self, StoreError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/durable-store")]
    pub(crate) fn warning(&self) -> Option<StoreWarning> {
        todo!()
    }
}

impl DurableStore {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn acquire(
        settings: &DurableSettings,
        legacy: &LegacyInputs,
    ) -> Result<Acquired, StoreError> {
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
    pub(crate) fn commit(
        &self,
        changes: Vec<(ConversationKey, Change)>,
    ) -> Result<(), StoreFailed> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn first_served(&self, at: SystemTime) -> Result<(), StoreFailed> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    fn raise(&self, error: StoreError) -> Raised {
        todo!()
    }
}

impl FatalWatch {
    #[expect(clippy::todo, reason = "skeleton: state/durable-store")]
    pub(crate) async fn failed(&mut self) -> StoreError {
        todo!()
    }
}
