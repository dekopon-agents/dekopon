use std::path::PathBuf;

use super::StoreError;

pub(crate) struct StoreOwner {
    path: PathBuf,
}

impl StoreOwner {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn acquire(
        connection: &rusqlite::Connection,
        path: PathBuf,
    ) -> Result<Self, StoreError> {
        todo!()
    }
}
