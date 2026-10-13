//! Read-only inspection and consistent snapshots of RagMonk databases, for
//! `backup`, `restore` and `doctor`. Nothing here writes to the database it
//! inspects.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::schema::{control_fingerprint, knowledge_fingerprint, recorded_fingerprint};

fn read_only(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// Whether a database file has this build's schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaState {
    /// No file (it is created on first use).
    Missing,
    Current,
    /// Anything else; the string says what was found.
    Incompatible(String),
}

impl SchemaState {
    pub fn is_incompatible(&self) -> bool {
        matches!(self, SchemaState::Incompatible(_))
    }
}

fn schema_state(path: &Path, expected: &str) -> SchemaState {
    if !path.is_file() {
        return SchemaState::Missing;
    }
    let found = read_only(path)
        .map_err(|e| e.to_string())
        .and_then(|conn| recorded_fingerprint(&conn));
    match found {
        Ok(Some(f)) if f == expected => SchemaState::Current,
        Ok(None) => SchemaState::Missing,
        Ok(Some(f)) => SchemaState::Incompatible(format!("schema {f}, expected {expected}")),
        Err(e) => SchemaState::Incompatible(e),
    }
}

/// Schema state of a control database, read without modifying it.
pub fn control_schema_state(path: &Path) -> SchemaState {
    schema_state(path, &control_fingerprint())
}

/// Schema state of a project knowledge database, read without modifying it.
pub fn knowledge_schema_state(path: &Path) -> SchemaState {
    schema_state(path, &knowledge_fingerprint())
}

/// `PRAGMA integrity_check`; the error describes what is wrong.
pub fn integrity_check(path: &Path) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let conn = read_only(path).map_err(|e| format!("cannot open {name}: {e}"))?;
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| format!("{name} is not a valid SQLite database: {e}"))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(format!("integrity check failed for {name}: {result}"))
    }
}

/// A transactionally consistent copy of `src` at `dest` (`VACUUM INTO`),
/// safe while a writer holds `src` open (WAL included).
pub fn snapshot(src: &Path, dest: &Path) -> Result<(), String> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let conn = read_only(src).map_err(|e| format!("open {}: {e}", src.display()))?;
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().into_owned()])
        .map_err(|e| format!("snapshot {}: {e}", src.display()))?;
    Ok(())
}

/// `(id, path)` of every registered source, read without creating or altering the schema.
pub fn registered_sources(control_db: &Path) -> Vec<(String, String)> {
    let Ok(conn) = read_only(control_db) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT id, path FROM sources ORDER BY created_at, id") else {
        return Vec::new();
    };
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// `PRAGMA journal_mode` of an open connection.
pub fn journal_mode(conn: &Connection) -> String {
    conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_state_snapshot_and_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("a.db");
        assert_eq!(control_schema_state(&db), SchemaState::Missing);
        let mut conn = crate::db::open(&db, 8).unwrap();
        crate::schema::create_or_verify(&mut conn, &db, crate::schema::CONTROL_SCHEMA).unwrap();
        assert_eq!(control_schema_state(&db), SchemaState::Current);
        assert!(knowledge_schema_state(&db).is_incompatible());
        let copy = dir.path().join("out/b.db");
        snapshot(&db, &copy).unwrap();
        assert_eq!(control_schema_state(&copy), SchemaState::Current);
        integrity_check(&copy).unwrap();
        std::fs::write(dir.path().join("bad.db"), b"not sqlite at all, sorry").unwrap();
        assert!(integrity_check(&dir.path().join("bad.db")).is_err());
        assert!(control_schema_state(&dir.path().join("bad.db")).is_incompatible());
    }
}
