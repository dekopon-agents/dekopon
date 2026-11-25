use std::{path::Path, time::Duration};

use rusqlite::{ErrorCode, TransactionBehavior};

use super::{StoreError, SyncMode};

pub(super) fn own(
    connection: &mut rusqlite::Connection,
    path: &Path,
    sync: SyncMode,
) -> Result<(), StoreError> {
    let owned = |error: rusqlite::Error| match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StoreError::Owned {
            path: path.to_path_buf(),
        },
        Some(_) | None => StoreError::from(error),
    };
    connection.busy_timeout(Duration::ZERO).map_err(owned)?;
    connection
        .pragma_update(None, "locking_mode", "EXCLUSIVE")
        .map_err(owned)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(owned)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Sqlite { code: None });
    }
    connection
        .pragma_update(None, "synchronous", sync.pragma())
        .map_err(owned)?;
    // A write takes the exclusive lock, which the connection then holds until it closes.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(owned)?;
    let version: i64 = transaction
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(owned)?;
    transaction
        .pragma_update(None, "user_version", version)
        .map_err(owned)?;
    transaction.commit().map_err(owned)?;
    Ok(())
}
