use super::StoreError;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SchemaVersion(u32);

impl SchemaVersion {
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
