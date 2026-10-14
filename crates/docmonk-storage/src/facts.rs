//! Disposable cache of per-file graph facts.
//!
//! A fact set is what the graph stage derives from one file on its own:
//! its structural edges, same-file resolutions and `pending` cross-file
//! references (the rows written before cross-file resolution). The base
//! index computes it from the parse it already makes and stores it here,
//! keyed by everything the rows depend on: file id, relative path, content
//! hash and derivation identity. The graph stage still reads and hashes
//! every file it derives (a file changed since indexing is still `stale`)
//! and only skips the second parse when the bytes match a stored key.
//!
//! The cache lives outside the knowledge database (`<home>/cache`), so
//! adding, deleting or ignoring it never changes a published index or
//! requires a reindex. Any entry that cannot be read is a miss; a cache
//! file with another layout is recreated empty.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::{Result, StorageError};
use crate::knowledge::RelationshipRow;

/// Layout of the cache file (`PRAGMA user_version`).
const LAYOUT: i64 = 1;

pub struct FactCache {
    conn: Mutex<Connection>,
}

/// The cache key of one file's facts.
pub fn fact_key(file_id: &str, rel_path: &str, content_hash: &str, derivation: &str) -> String {
    let mut h = Sha256::new();
    for part in [file_id, rel_path, content_hash, derivation] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl FactCache {
    /// Opens (creating if needed) the cache at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = crate::db::open(path, 8)?;
        let layout: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(StorageError::sqlite("graph facts layout"))?;
        if layout != LAYOUT {
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS graph_facts;
                 CREATE TABLE graph_facts (
                     key TEXT PRIMARY KEY,
                     source_id TEXT NOT NULL,
                     file_id TEXT NOT NULL,
                     rows TEXT NOT NULL
                 );
                 CREATE INDEX graph_facts_source ON graph_facts(source_id);
                 PRAGMA user_version = {LAYOUT};"
            ))
            .map_err(StorageError::sqlite("create graph facts"))?;
        }
        conn.set_prepared_statement_cache_capacity(8);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        f(&conn)
    }

    /// The facts stored under `key`; `None` when absent or unreadable.
    pub fn get(&self, key: &str) -> Option<Vec<RelationshipRow>> {
        let text: Option<String> = self
            .with(|c| {
                c.prepare_cached("SELECT rows FROM graph_facts WHERE key = ?1")
                    .and_then(|mut s| s.query_row([key], |r| r.get(0)).optional())
                    .map_err(StorageError::sqlite("read graph facts"))
            })
            .ok()
            .flatten();
        serde_json::from_str(&text?).ok()
    }

    pub fn put(
        &self,
        source_id: &str,
        file_id: &str,
        key: &str,
        rows: &[RelationshipRow],
    ) -> Result<()> {
        let text = serde_json::to_string(rows)
            .map_err(|e| StorageError::Invalid(format!("graph facts: {e}")))?;
        self.with(|c| {
            c.prepare_cached(
                "INSERT OR REPLACE INTO graph_facts (key, source_id, file_id, rows)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .and_then(|mut s| s.execute(params![key, source_id, file_id, text]))
            .map(|_| ())
            .map_err(StorageError::sqlite("write graph facts"))
        })
    }

    /// Drops `source_id`'s entries whose key is not in `keep` (facts of
    /// superseded content or derivations). Returns the number removed.
    pub fn retain(&self, source_id: &str, keep: &HashSet<String>) -> Result<usize> {
        self.with(|c| {
            let keys: Vec<String> = c
                .prepare_cached("SELECT key FROM graph_facts WHERE source_id = ?1")
                .and_then(|mut s| {
                    s.query_map([source_id], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()
                })
                .map_err(StorageError::sqlite("list graph facts"))?;
            let mut removed = 0;
            for k in keys.iter().filter(|k| !keep.contains(*k)) {
                removed += c
                    .prepare_cached("DELETE FROM graph_facts WHERE key = ?1")
                    .and_then(|mut s| s.execute([k]))
                    .map_err(StorageError::sqlite("drop graph facts"))?;
            }
            Ok(removed)
        })
    }

    /// `(entries, bytes)` stored for `source_id`.
    pub fn usage(&self, source_id: &str) -> Result<(u64, u64)> {
        self.with(|c| {
            c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(rows)), 0) FROM graph_facts
                 WHERE source_id = ?1",
                [source_id],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
            .map_err(StorageError::sqlite("graph facts usage"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> RelationshipRow {
        RelationshipRow {
            id: id.into(),
            file_id: "f".into(),
            relationship_type: "calls".into(),
            source_entity_id: "e".into(),
            resolver: "pending".into(),
            confidence: "medium".into(),
            reference_text: Some("x.y".into()),
            ..RelationshipRow::default()
        }
    }

    #[test]
    fn facts_round_trip_by_key_and_are_retained_per_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.db");
        let c = FactCache::open(&path).unwrap();
        let k1 = fact_key("f", "a.py", "h1", "d");
        let k2 = fact_key("f", "a.py", "h2", "d");
        assert_ne!(k1, fact_key("f", "a.py", "h1", "other derivation"));
        assert_ne!(
            k1,
            fact_key("f", "b.py", "h1", "d"),
            "paths are in the rows"
        );
        assert!(c.get(&k1).is_none());
        c.put("s", "f", &k1, &[row("r1"), row("r2")]).unwrap();
        c.put("s", "f", &k2, &[row("r3")]).unwrap();
        c.put("t", "g", &fact_key("g", "b.py", "h", "d"), &[])
            .unwrap();
        assert_eq!(c.get(&k1).unwrap(), vec![row("r1"), row("r2")]);
        assert_eq!(c.usage("s").unwrap().0, 2);
        assert_eq!(c.retain("s", &[k2.clone()].into()).unwrap(), 1);
        assert!(c.get(&k1).is_none());
        assert_eq!(c.get(&k2).unwrap(), vec![row("r3")]);
        assert_eq!(c.usage("t").unwrap().0, 1, "other sources untouched");
    }

    #[test]
    fn unreadable_entries_are_misses_and_another_layout_is_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.db");
        let c = FactCache::open(&path).unwrap();
        let k = fact_key("f", "a.py", "h", "d");
        c.put("s", "f", &k, &[row("r1")]).unwrap();
        c.with(|conn| {
            conn.execute("UPDATE graph_facts SET rows = 'not json'", [])
                .map_err(StorageError::sqlite("corrupt"))
        })
        .unwrap();
        assert!(c.get(&k).is_none());
        drop(c);
        let conn = crate::db::open(&path, 1).unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        drop(conn);
        let c = FactCache::open(&path).unwrap();
        assert_eq!(c.usage("s").unwrap(), (0, 0));
    }
}
