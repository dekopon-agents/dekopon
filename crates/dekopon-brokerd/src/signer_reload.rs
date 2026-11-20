#![expect(dead_code, reason = "skeleton: daemon/signer-reload")]

use std::path::PathBuf;

use dekopon_core::reload::{Reconciled, WatchSet};

use crate::BrokerdError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeyId(String);

pub(crate) struct StagedKey {
    kid: KeyId,
    key: PathBuf,
}

pub(crate) struct SignerGroup {
    key: PathBuf,
    active: KeyId,
    staged: Option<StagedKey>,
}

impl SignerGroup {
    #[expect(clippy::todo, reason = "skeleton: daemon/signer-reload")]
    pub(crate) fn watch_set(&self) -> WatchSet {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/signer-reload")]
    pub(crate) async fn reconcile_key(&mut self) -> Result<Reconciled, BrokerdError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/signer-reload"
    )]
    async fn jwks_serves(&self, kid: &KeyId) -> bool {
        todo!()
    }
}
