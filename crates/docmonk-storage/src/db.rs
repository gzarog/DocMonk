//! Connection factory and transactions. Pragmas:
//! WAL, foreign keys on, `synchronous=NORMAL`, 5 s busy timeout, in-memory
//! temp store and a configurable page cache (MB).

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Result, StorageError};

pub const DEFAULT_CACHE_SIZE_MB: i64 = 64;
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Opens (creating if needed) a read-write database.
pub fn open(path: &Path, cache_size_mb: i64) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| StorageError::Io(format!("cannot create {}: {e}", parent.display())))?;
    }
    let ctx = format!("open {}", path.display());
    let conn = Connection::open(path).map_err(StorageError::sqlite(ctx.clone()))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(StorageError::sqlite(ctx.clone()))?;
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -{};",
        cache_size_mb.max(1) * 1024
    ))
    .map_err(StorageError::sqlite(ctx))?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(conn)
}

/// `BEGIN IMMEDIATE` transaction: commits on `Ok`, rolls back on `Err`.
/// Callers must not hold it across ML inference, document conversion or
/// network I/O.
/// Runs `f` atomically: its own IMMEDIATE transaction, or a savepoint when
/// the connection is already inside a session (see
/// `ProjectStore::begin_session`), so nested writes commit with it.
pub fn write_tx<T>(conn: &mut Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    if !conn.is_autocommit() {
        let sp = conn
            .savepoint()
            .map_err(StorageError::sqlite("begin savepoint"))?;
        let out = f(&sp)?;
        sp.commit()
            .map_err(StorageError::sqlite("release savepoint"))?;
        return Ok(out);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(StorageError::sqlite("begin transaction"))?;
    let out = f(&tx)?;
    tx.commit().map_err(StorageError::sqlite("commit"))?;
    Ok(out)
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}
