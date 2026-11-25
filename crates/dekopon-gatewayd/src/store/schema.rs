use std::{fmt, path::Path};

use rusqlite::Transaction;

use super::StoreError;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SchemaVersion(u32);

#[cfg(test)]
impl SchemaVersion {
    pub(crate) const fn new(version: u32) -> Self {
        Self(version)
    }
}

pub(crate) struct Step {
    pub(crate) to: SchemaVersion,
    pub(crate) sql: &'static str,
}

pub(crate) const STEPS: &[Step] = &[Step {
    to: SchemaVersion(1),
    sql: "CREATE TABLE conversations (
            key TEXT PRIMARY KEY NOT NULL,
            version INTEGER NOT NULL,
            snapshot TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE retained_follows (
            key TEXT PRIMARY KEY NOT NULL,
            marker TEXT NOT NULL
        ) STRICT;
        CREATE TABLE meta (
            name TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        ) STRICT;",
}];

impl SchemaVersion {
    pub(crate) const CURRENT: Self = Self(1);
    pub(super) const EMPTY: Self = Self(0);

    pub(crate) fn read(connection: &rusqlite::Connection) -> Result<Self, StoreError> {
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(Self(u32::try_from(version).unwrap_or(u32::MAX)))
    }

    pub(super) fn backup_path(self, store: &Path) -> std::path::PathBuf {
        let mut name = store.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".schema-{}.bak", self.0));
        store.with_file_name(name)
    }

    pub(crate) fn migrate(
        self,
        transaction: &Transaction<'_>,
        steps: &[Step],
    ) -> Result<Self, StoreError> {
        let mut version = self;
        for step in steps.iter().filter(|step| step.to > self) {
            transaction.execute_batch(step.sql)?;
            version = step.to;
        }
        transaction.pragma_update(None, "user_version", version.0)?;
        Ok(version)
    }

    pub(super) fn latest(steps: &[Step]) -> Self {
        steps
            .iter()
            .map(|step| step.to)
            .max()
            .unwrap_or(Self::EMPTY)
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}
