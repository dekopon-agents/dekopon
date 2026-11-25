#![expect(
    clippy::todo,
    dead_code,
    reason = "skeleton: egress/vault-rotation fills and calls these"
)]

use std::{collections::BTreeMap, path::PathBuf};

use dekopon_broker::{CookieEntry, CookieSet};
use dekopon_core::SecretDrn;
use thiserror::Error;

// One vault per process, owned by Coordination across generations; the record file under its
// PrivateFileLock is the only state, so seed and rev are re-read under the lock on every operation.
pub(crate) struct CookieVault {
    records: BTreeMap<SecretDrn, PathBuf>,
    expected_uid: u32,
}

impl CookieVault {
    pub(crate) fn open(_records: BTreeMap<SecretDrn, PathBuf>, _expected_uid: u32) -> Self {
        todo!()
    }

    pub(crate) async fn cookie_set(&self, _profile: &SecretDrn) -> Result<CookieSet, VaultError> {
        todo!()
    }

    pub(crate) async fn store_rotations(
        &self,
        _profile: &SecretDrn,
        _rotations: Vec<CookieEntry>,
    ) -> Result<StoredRotations, VaultError> {
        todo!()
    }

    pub(crate) fn same_records(&self, _records: &BTreeMap<SecretDrn, PathBuf>) -> bool {
        todo!()
    }

    pub(crate) async fn drain(&self) {
        todo!()
    }

    pub(crate) async fn reseed(
        &self,
        _profile: &SecretDrn,
        _cookies: CookieSet,
    ) -> Result<(), VaultError> {
        todo!()
    }
}

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoredRotations {
    Stored { count: usize },
    NothingStorable,
}

#[derive(Debug, Error)]
pub(crate) enum VaultError {
    #[error("no cookie vault record is configured for this profile")]
    UnknownProfile,
    #[error("the cookie vault record could not be read or written ({kind:?})")]
    Io { kind: std::io::ErrorKind },
    #[error("the cookie vault record is not valid")]
    Corrupt,
    #[error("the stored cookie set exceeds its budget")]
    OverBudget,
}

impl VaultError {
    pub(crate) const fn category(&self) -> &'static str {
        match self {
            Self::UnknownProfile => "vault-unknown-profile",
            Self::Io { .. } => "vault-io",
            Self::Corrupt => "vault-corrupt",
            Self::OverBudget => "vault-over-budget",
        }
    }
}
