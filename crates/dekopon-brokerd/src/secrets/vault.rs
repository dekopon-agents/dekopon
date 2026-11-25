#![expect(
    clippy::todo,
    dead_code,
    reason = "skeleton: egress/vault-rotation fills and calls these"
)]

use std::{collections::BTreeMap, path::PathBuf};

use dekopon_broker::{CookieEntry, CookieSet};
use dekopon_core::SecretDrn;
use thiserror::Error;

pub(crate) struct CookieVault {
    records: BTreeMap<SecretDrn, VaultRecord>,
    expected_uid: u32,
}

struct VaultRecord {
    path: PathBuf,
    seed: u64,
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
