//! Per-thread SQL statement counter for diagnostics.
//!
//! Every connection opened through [`crate::db::open`] reports each
//! completed statement here (SQLite's profile hook). A connection is used
//! from one thread at a time, so the difference of [`sql_statements`]
//! before and after a stage is that stage's statement count. Only counts
//! are kept: no SQL text, parameter or path is recorded.

use std::cell::Cell;
use std::time::Duration;

thread_local! {
    static STATEMENTS: Cell<u64> = const { Cell::new(0) };
}

fn on_statement(_sql: &str, _elapsed: Duration) {
    STATEMENTS.with(|c| c.set(c.get() + 1));
}

pub(crate) fn install(conn: &mut rusqlite::Connection) {
    conn.profile(Some(on_statement));
}

/// Statements completed on this thread since it started.
pub fn sql_statements() -> u64 {
    STATEMENTS.with(Cell::get)
}

#[cfg(test)]
mod tests {
    #[test]
    fn statements_are_counted_per_thread() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db"), 1).unwrap();
        let before = super::sql_statements();
        conn.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
            .unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(super::sql_statements() - before >= 3);
        let other = std::thread::spawn(super::sql_statements).join().unwrap();
        assert_eq!(other, 0, "another thread's count is its own");
    }
}
