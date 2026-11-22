use std::fmt;

use super::StoreError;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    pub(crate) const CURRENT: Self = Self(1);

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn read(connection: &rusqlite::Connection) -> Result<Self, StoreError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: state/durable-store"
    )]
    pub(crate) fn migrate(
        self,
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<Self, StoreError> {
        todo!()
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}
