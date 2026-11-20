#![expect(
    dead_code,
    reason = "skeleton: state/durable-store wires the store into boot and serving"
)]

mod convert;
mod delivery;
mod ledger;
mod lock;
mod record;
mod schema;

use std::{io, path::PathBuf, time::SystemTime};

use parking_lot::Mutex;
use thiserror::Error;

pub(crate) use convert::{Conversion, LegacyInputs};
#[expect(unused_imports, reason = "skeleton: state/durable-store")]
pub(crate) use delivery::{DeliveryAck, DeliveryId, PendingDelivery};
#[expect(unused_imports, reason = "skeleton: state/spend-slice1")]
pub(crate) use ledger::{Hold, HoldId, HoldOutcome, Ledger};
pub(crate) use lock::StoreOwner;
pub use record::ConversationKey;
#[expect(unused_imports, reason = "skeleton: state/durable-store")]
pub(crate) use record::{
    AnchorRow, Change, FollowRow, JobRow, OwnershipRow, Record, RecordVersion, Snapshot, WakeRow,
    WakeRowKind,
};
pub(crate) use schema::SchemaVersion;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not read or write the durable store")]
    Io { kind: io::ErrorKind },
    #[error("the durable store refused a statement")]
    Sqlite { code: Option<i32> },
    #[error("another gateway owns the durable store at {path}")]
    Owned { path: PathBuf },
    #[error("the durable store schema is newer than this gateway")]
    SchemaTooNew { found: u32, known: u32 },
    #[error("{} durable records could not be upgraded; restore {backup} to roll back", rows.len())]
    Unupgradable {
        rows: Vec<ConversationKey>,
        backup: PathBuf,
    },
    #[error("conversion of the legacy stores stopped before it finished")]
    ConversionInterrupted,
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
            Self::Unupgradable { .. } => "unupgradable",
            Self::ConversionInterrupted => "conversion-interrupted",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyncMode {
    Off,
    Normal,
    Full,
}

#[derive(Clone, Debug)]
pub(crate) struct StoreLocation {
    pub(crate) path: PathBuf,
    pub(crate) sync: SyncMode,
}

pub(crate) struct DurableStore {
    owner: StoreOwner,
    connection: Mutex<rusqlite::Connection>,
    location: StoreLocation,
}

pub(crate) struct Restored {
    pub(crate) wakes: Vec<WakeRow>,
    pub(crate) follows: Vec<FollowRow>,
    pub(crate) ownership: Vec<OwnershipRow>,
    pub(crate) interrupted_jobs: Vec<JobRow>,
    pub(crate) pending: Vec<PendingDelivery>,
}

pub(crate) struct BootReport {
    pub(crate) schema: SchemaVersion,
    pub(crate) migrated_at: Option<SystemTime>,
    pub(crate) backup: Option<PathBuf>,
    pub(crate) conversion: Option<Conversion>,
}

impl DurableStore {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn acquire(
        location: &StoreLocation,
        legacy: Option<&LegacyInputs>,
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

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn load(
        &self,
        conversation: &ConversationKey,
    ) -> Result<Option<Record>, StoreError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: state/durable-store")]
    pub(crate) fn next_due(&self) -> Result<Option<SystemTime>, StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn take_due(&self, now: SystemTime) -> Result<Vec<PendingDelivery>, StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn acknowledge(&self, ack: DeliveryAck) -> Result<(), StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn expire(&self, now: SystemTime) -> Result<Vec<WakeRow>, StoreError> {
        todo!()
    }
}
