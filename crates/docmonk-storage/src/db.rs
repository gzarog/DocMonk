//! Connection factory and transactions. Pragmas:
//! WAL, foreign keys on, `synchronous=NORMAL`, 5 s busy timeout, in-memory
//! temp store, a 64 MB `journal_size_limit` and a configurable page cache (MB).

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Result, StorageError};

pub const DEFAULT_CACHE_SIZE_MB: i64 = 64;
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// Bytes a checkpointed WAL is truncated back to (`journal_size_limit`).
pub const JOURNAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// Opens (creating if needed) a read-write database.
pub fn open(path: &Path, cache_size_mb: i64) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| StorageError::Io(format!("cannot create {}: {e}", parent.display())))?;
    }
    let ctx = format!("open {}", path.display());
    let mut conn = Connection::open(path).map_err(StorageError::sqlite(ctx.clone()))?;
    crate::stats::install(&mut conn);
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(StorageError::sqlite(ctx.clone()))?;
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA journal_size_limit = {};
         PRAGMA cache_size = -{};",
        JOURNAL_SIZE_LIMIT,
        cache_size_mb.max(1) * 1024
    ))
    .map_err(StorageError::sqlite(ctx))?;
    conn.set_prepared_statement_cache_capacity(64);
    install_deadline(&conn);
    Ok(conn)
}

/// SQLite virtual-machine steps between deadline checks.
const DEADLINE_CHECK_STEPS: i32 = 1000;

/// Aborts a running statement (`SQLITE_INTERRUPT`) once the request on the
/// executing thread has expired (see `docmonk_core::deadline`). The handler
/// reads the deadline of whichever thread runs the statement, so pooled
/// connections shared across requests stay correct; threads without a
/// request deadline are never interrupted.
pub fn install_deadline(conn: &Connection) {
    conn.progress_handler(
        DEADLINE_CHECK_STEPS,
        Some(docmonk_core::deadline::expired as fn() -> bool),
    );
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

#[cfg(test)]
mod deadline_tests {
    use std::time::Duration;

    use docmonk_core::deadline::Token;

    /// Audit F09: a statement still running when its request expires is
    /// interrupted; the same connection serves later requests normally.
    #[test]
    fn expired_requests_interrupt_running_statements() {
        let dir = tempfile::tempdir().unwrap();
        let conn = super::open(&dir.path().join("t.db"), 1).unwrap();
        let endless = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) \
                       SELECT count(*) FROM n";
        let token = Token::new(Some(Duration::from_millis(50)));
        let started = std::time::Instant::now();
        let err = {
            let _g = token.enter();
            conn.query_row(endless, [], |r| r.get::<_, i64>(0))
                .unwrap_err()
        };
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(err.to_string().contains("interrupt"), "{err}");
        // No token on this thread: never interrupted.
        let one: i64 = conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(one, 1);
    }
}
