//! The current SQLite schemas, one complete definition per database.
//!
//! A missing (empty) database is created directly at this schema. An
//! existing database is used only when the schema fingerprint it recorded
//! at creation equals [`control_fingerprint`] / [`knowledge_fingerprint`];
//! anything else is refused with a reset/reindex instruction and left
//! untouched. RagMonk never upgrades a database in place: every source can
//! be rebuilt from its original files.
//!
//! Every index-derived row carries the `build_id` that produced it. Reads
//! must join on the source's `active_build_id` so an in-progress (or
//! aborted) build is never visible — the local equivalent of server-side
//! atomic publication.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::db::write_tx;
use crate::error::{Result, StorageError};

pub const CONTROL_SCHEMA: &str = r#"
CREATE TABLE metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- User-owned source definitions.
CREATE TABLE sources (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    source_type TEXT NOT NULL CHECK (source_type IN ('local', 'network')),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    include_patterns TEXT NOT NULL DEFAULT '[]',
    exclude_patterns TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Index-derived state. A source without a published build is always
-- 'needs_full_rebuild'.
CREATE TABLE source_state (
    source_id TEXT PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
    build_state TEXT NOT NULL
        CHECK (build_state IN ('needs_full_rebuild', 'building', 'ready', 'failed')),
    rebuild_reason TEXT,
    online_status TEXT NOT NULL DEFAULT 'active' CHECK (online_status IN ('active', 'offline')),
    active_build_id TEXT,
    pending_build_id TEXT,
    versions TEXT,
    last_full_build_at TEXT,
    last_scan_at TEXT,
    last_error TEXT,
    updated_at TEXT NOT NULL
);
"#;

pub const KNOWLEDGE_SCHEMA: &str = r#"
CREATE TABLE metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE builds (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('full', 'incremental')),
    status TEXT NOT NULL CHECK (status IN ('building', 'published', 'aborted', 'superseded')),
    versions TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE TABLE files (
    id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('code', 'document', 'unknown')),
    size INTEGER NOT NULL,
    mtime REAL NOT NULL,
    content_hash TEXT,
    status TEXT NOT NULL,
    build_id TEXT NOT NULL REFERENCES builds(id),
    parser_version TEXT,
    chunker_version TEXT,
    converter_version TEXT,
    embedding_model_id TEXT,
    embedding_text_version TEXT,
    last_indexed_at TEXT,
    last_error TEXT,
    -- Durable per-file retry state: a transiently failing file is retried
    -- on a later run once next_attempt_at is due, without blocking
    -- publication of the rest of the source.
    attempt_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (build_id, id),
    UNIQUE (build_id, rel_path)
);
CREATE INDEX idx_files_build ON files(build_id);
CREATE INDEX idx_files_hash ON files(content_hash);

CREATE TABLE index_jobs (
    id TEXT PRIMARY KEY,
    build_id TEXT NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    file_id TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    job_type TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'processing', 'retry', 'failed', 'completed')),
    priority INTEGER NOT NULL DEFAULT 0,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT,
    error_code TEXT,
    error_message TEXT
);
CREATE INDEX idx_jobs_status ON index_jobs(status, next_attempt_at);

CREATE TABLE index_errors (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    build_id TEXT,
    file_id TEXT,
    rel_path TEXT,
    error_code TEXT NOT NULL,
    error_message TEXT NOT NULL,
    occurred_at TEXT NOT NULL
);
CREATE INDEX idx_errors_time ON index_errors(occurred_at);

CREATE TABLE entities (
    id TEXT NOT NULL,
    build_id TEXT NOT NULL REFERENCES builds(id),
    file_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    language TEXT NOT NULL,
    parent_id TEXT,
    signature TEXT,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_col INTEGER NOT NULL DEFAULT 0,
    end_col INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (build_id, id)
);
CREATE INDEX idx_entities_name ON entities(build_id, name);
CREATE INDEX idx_entities_qname ON entities(build_id, qualified_name);
CREATE INDEX idx_entities_file ON entities(build_id, file_id);

CREATE TABLE relationships (
    id TEXT NOT NULL,
    build_id TEXT NOT NULL REFERENCES builds(id),
    file_id TEXT NOT NULL,
    relationship_type TEXT NOT NULL,
    source_entity_id TEXT NOT NULL,
    target_entity_id TEXT,
    target_symbol TEXT,
    resolver TEXT NOT NULL,
    confidence TEXT NOT NULL,
    source_location TEXT,
    evidence TEXT,
    -- The raw reference text, so cross-file targets are re-resolved against
    -- the whole build before publication instead of depending on file order.
    reference_text TEXT,
    PRIMARY KEY (build_id, id)
);
CREATE INDEX idx_rel_resolver ON relationships(build_id, resolver);
CREATE INDEX idx_rel_symbol ON relationships(build_id, target_symbol);
CREATE INDEX idx_rel_source ON relationships(build_id, source_entity_id);
CREATE INDEX idx_rel_target ON relationships(build_id, target_entity_id);
CREATE INDEX idx_rel_file ON relationships(build_id, file_id);

-- Top-level documents and EML attachment child documents.
CREATE TABLE documents (
    id TEXT NOT NULL,
    build_id TEXT NOT NULL REFERENCES builds(id),
    file_id TEXT NOT NULL,
    format TEXT NOT NULL,
    title TEXT,
    author TEXT,
    page_count INTEGER,
    is_scanned INTEGER NOT NULL DEFAULT 0,
    content_hash TEXT,
    parent_document_id TEXT,
    attachment_name TEXT,
    attachment_content_type TEXT,
    attachment_index INTEGER,
    attachment_content_id TEXT,
    PRIMARY KEY (build_id, id),
    CHECK ((parent_document_id IS NULL) = (attachment_index IS NULL))
);
CREATE INDEX idx_documents_file ON documents(build_id, file_id);
CREATE INDEX idx_documents_parent ON documents(build_id, parent_document_id);

-- Retrieval chunks (heading / paragraph / table).
CREATE TABLE chunks (
    id TEXT NOT NULL,
    build_id TEXT NOT NULL REFERENCES builds(id),
    document_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('heading', 'paragraph', 'table')),
    ordinal INTEGER NOT NULL,
    heading_path TEXT NOT NULL DEFAULT '[]',
    heading_level INTEGER,
    text TEXT NOT NULL,
    search_text TEXT NOT NULL,
    embedding_text TEXT,
    token_count INTEGER,
    page_start INTEGER,
    page_end INTEGER,
    table_rows TEXT,
    caption TEXT,
    -- Ordinal of the enclosing heading chunk in the same document; siblings
    -- share it (search context expansion).
    parent_ordinal INTEGER,
    PRIMARY KEY (build_id, id)
);
CREATE INDEX idx_chunks_document ON chunks(build_id, document_id, ordinal);
CREATE INDEX idx_chunks_file ON chunks(build_id, file_id);

CREATE TABLE cross_links (
    id TEXT NOT NULL,
    build_id TEXT NOT NULL REFERENCES builds(id),
    link_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    chunk_id TEXT,
    resolver TEXT NOT NULL,
    confidence TEXT NOT NULL,
    evidence TEXT,
    PRIMARY KEY (build_id, id)
);
CREATE INDEX idx_links_entity ON cross_links(build_id, entity_id);
CREATE INDEX idx_links_document ON cross_links(build_id, document_id);
CREATE INDEX idx_links_resolver ON cross_links(build_id, resolver);

-- Manual links are user data, not index-derived: they survive rebuilds.
-- A link may pin a specific chunk (ordinal within the document). Optional
-- parts use the -1 sentinel so the natural key is enforced (SQLite treats
-- NULLs in UNIQUE as distinct).
CREATE TABLE manual_links (
    id TEXT PRIMARY KEY,
    link_type TEXT NOT NULL,
    entity_qualified_name TEXT NOT NULL,
    document_rel_path TEXT NOT NULL,
    attachment_index INTEGER NOT NULL DEFAULT -1,
    chunk_ordinal INTEGER NOT NULL DEFAULT -1,
    note TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (link_type, entity_qualified_name, document_rel_path, attachment_index, chunk_ordinal)
);

-- Embeddings are build-scoped like every other derived row and stamped
-- with the model fingerprint (model id, revision, asset checksums,
-- preprocessing version) so a model change is detected and re-embedded.
CREATE TABLE embeddings (
    build_id TEXT NOT NULL,
    subject_type TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    model_fingerprint TEXT NOT NULL,
    text_hash TEXT NOT NULL,
    dims INTEGER NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY (build_id, subject_type, subject_id)
);
CREATE INDEX idx_embeddings_file ON embeddings(build_id, file_id);
-- Cross-build cache: exact text hash + model fingerprint + text version.
CREATE TABLE embedding_cache (
    text_hash TEXT NOT NULL,
    model_fingerprint TEXT NOT NULL,
    embedding_text_version TEXT NOT NULL,
    dims INTEGER NOT NULL,
    vector BLOB NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (text_hash, model_fingerprint, embedding_text_version)
);

-- Lexical indexes. Rows are written explicitly together with their base
-- rows, share the base row's rowid (so per-file and per-build deletes are
-- rowid lookups) and carry build_id so searches filter to the active build.
CREATE VIRTUAL TABLE code_fts USING fts5(
    entity_id UNINDEXED, build_id UNINDEXED, name, qualified_name, signature
);
CREATE VIRTUAL TABLE chunk_fts USING fts5(
    chunk_id UNINDEXED, build_id UNINDEXED, heading, body, title
);
CREATE VIRTUAL TABLE path_fts USING fts5(
    file_id UNINDEXED, build_id UNINDEXED, path
);
"#;

/// `metadata` key holding the fingerprint of the schema a database was
/// created with.
pub const FINGERPRINT_KEY: &str = "schema_fingerprint";

/// Stable identity of a schema definition: the first 16 hex digits of the
/// SHA-256 of its DDL. Any change to the DDL changes the fingerprint, so a
/// database created by a different build is detected, never reinterpreted.
pub fn fingerprint(ddl: &str) -> String {
    let digest = Sha256::digest(ddl.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn control_fingerprint() -> String {
    fingerprint(CONTROL_SCHEMA)
}

pub fn knowledge_fingerprint() -> String {
    fingerprint(KNOWLEDGE_SCHEMA)
}

/// The fingerprint recorded in an open database: `Ok(None)` when it has no
/// tables at all (a new file), `Err` naming the problem when it has tables
/// but no readable fingerprint.
pub fn recorded_fingerprint(conn: &Connection) -> std::result::Result<Option<String>, String> {
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("not a readable SQLite database: {e}"))?;
    if tables == 0 {
        return Ok(None);
    }
    conn.query_row(
        "SELECT value FROM metadata WHERE key = ?1",
        [FINGERPRINT_KEY],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map_err(|_| "no RagMonk schema fingerprint".to_owned())?
    .map(Some)
    .ok_or_else(|| "no RagMonk schema fingerprint".to_owned())
}

/// Opens the database at `path` read-write at schema `ddl`: an existing
/// file is first checked through a read-only connection, so a database with
/// any other schema is refused before anything (not even the journal mode)
/// is written to it.
pub fn open_current(path: &Path, cache_size_mb: i64, ddl: &str) -> Result<Connection> {
    if path.is_file() {
        let found = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())
        .and_then(|conn| recorded_fingerprint(&conn));
        let expected = fingerprint(ddl);
        match found {
            Ok(None) => {}
            Ok(Some(f)) if f == expected => {}
            Ok(Some(f)) => return Err(incompatible(path, f, expected)),
            Err(problem) => return Err(incompatible(path, problem, expected)),
        }
    }
    let mut conn = crate::db::open(path, cache_size_mb)?;
    create_or_verify(&mut conn, path, ddl)?;
    Ok(conn)
}

fn incompatible(path: &Path, found: String, expected: String) -> StorageError {
    StorageError::IncompatibleSchema {
        path: path.display().to_string(),
        found,
        expected,
    }
}

/// Creates the schema in an empty database, or verifies that an existing
/// one has exactly the current schema. Never alters an existing database.
pub fn create_or_verify(conn: &mut Connection, path: &Path, ddl: &str) -> Result<()> {
    let expected = fingerprint(ddl);
    match recorded_fingerprint(conn) {
        Ok(None) => write_tx(conn, |tx| {
            tx.execute_batch(ddl)
                .map_err(StorageError::sqlite("create schema"))?;
            tx.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
                [FINGERPRINT_KEY, expected.as_str()],
            )
            .map_err(StorageError::sqlite("record schema fingerprint"))?;
            Ok(())
        }),
        Ok(Some(found)) if found == expected => Ok(()),
        Ok(Some(found)) => Err(incompatible(path, found, expected)),
        Err(problem) => Err(incompatible(path, problem, expected)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'index') ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn creates_directly_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.db");
        let mut conn = crate::db::open(&path, 8).unwrap();
        create_or_verify(&mut conn, &path, KNOWLEDGE_SCHEMA).unwrap();
        let names = tables(&conn);
        for t in [
            "builds",
            "files",
            "entities",
            "relationships",
            "documents",
            "chunks",
            "cross_links",
            "manual_links",
            "embeddings",
            "embedding_cache",
            "code_fts",
            "chunk_fts",
            "path_fts",
            "idx_rel_symbol",
            "idx_links_resolver",
        ] {
            assert!(names.iter().any(|n| n == t), "{t} missing: {names:?}");
        }
        // clean-slate-audit: allow-start
        assert!(!names.iter().any(|n| n.contains("migration")), "{names:?}");
        // clean-slate-audit: allow-end
        create_or_verify(&mut conn, &path, KNOWLEDGE_SCHEMA).unwrap();
    }

    #[test]
    fn refuses_foreign_or_changed_schema_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.db");
        let mut conn = crate::db::open(&path, 8).unwrap();
        conn.execute_batch("CREATE TABLE sources (id TEXT); INSERT INTO sources VALUES ('x');")
            .unwrap();
        let before = tables(&conn);
        let err = create_or_verify(&mut conn, &path, CONTROL_SCHEMA).unwrap_err();
        assert!(matches!(err, StorageError::IncompatibleSchema { .. }));
        assert!(err.to_string().contains("ragmonk index"), "{err}");
        assert_eq!(
            tables(&conn),
            before,
            "an incompatible database is never altered"
        );

        let other = dir.path().join("o.db");
        let mut conn = crate::db::open(&other, 8).unwrap();
        create_or_verify(
            &mut conn,
            &other,
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        assert!(matches!(
            create_or_verify(&mut conn, &other, CONTROL_SCHEMA),
            Err(StorageError::IncompatibleSchema { .. })
        ));
    }
}
