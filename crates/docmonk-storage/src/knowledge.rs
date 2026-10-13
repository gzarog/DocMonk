//! Per-source knowledge store (`<home>/projects/<id>/knowledge.db`).
//!
//! All index-derived rows belong to a build. Readers pass the source's
//! active build id; rows of a building/aborted build are invisible. Writes
//! for one file are a single short transaction (callers prepare/convert/
//! embed before calling, never inside).

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::control::{IndexVersions, RebuildPlan};
use crate::db::{now_iso, write_tx};
use crate::error::{Result, StorageError};
use crate::schema::{open_current, KNOWLEDGE_SCHEMA};
use crate::StorageLayout;

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct FileRow {
    pub id: String,
    pub rel_path: String,
    pub kind: String,
    pub size: i64,
    pub mtime: f64,
    pub content_hash: Option<String>,
    pub status: String,
    pub parser_version: Option<String>,
    pub chunker_version: Option<String>,
    pub converter_version: Option<String>,
    pub embedding_model_id: Option<String>,
    pub embedding_text_version: Option<String>,
    pub last_error: Option<String>,
    /// Failed processing attempts so far (status `retry`/`failed`).
    pub attempt_count: i64,
    /// When a `retry` file becomes due again.
    pub next_attempt_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct EntityRow {
    pub id: String,
    pub file_id: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub language: String,
    pub parent_id: Option<String>,
    pub signature: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub start_col: i64,
    pub end_col: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct RelationshipRow {
    pub id: String,
    pub file_id: String,
    pub relationship_type: String,
    pub source_entity_id: String,
    pub target_entity_id: Option<String>,
    pub target_symbol: Option<String>,
    pub resolver: String,
    pub confidence: String,
    pub source_location: Option<String>,
    pub evidence: Option<String>,
    /// Raw reference text (call/base/import/decorator head) the target
    /// was resolved from; `None` for structural edges.
    pub reference_text: Option<String>,
}

/// EML attachment provenance for a child document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttachmentProvenance {
    pub parent_document_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub index: i64,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DocumentRow {
    pub id: String,
    pub file_id: String,
    pub format: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub page_count: Option<i64>,
    pub is_scanned: bool,
    pub content_hash: Option<String>,
    pub attachment: Option<AttachmentProvenance>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChunkRow {
    pub id: String,
    pub document_id: String,
    pub file_id: String,
    pub kind: String,
    pub ordinal: i64,
    pub heading_path: Vec<String>,
    pub heading_level: Option<i64>,
    pub text: String,
    pub search_text: String,
    pub embedding_text: Option<String>,
    pub token_count: Option<i64>,
    pub page_start: Option<i64>,
    pub page_end: Option<i64>,
    pub table_rows: Option<Vec<Vec<String>>>,
    pub caption: Option<String>,
    /// Ordinal of the enclosing heading chunk in the same document.
    pub parent_ordinal: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkRow {
    pub id: String,
    pub link_type: String,
    pub entity_id: String,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub resolver: String,
    pub confidence: String,
    pub evidence: Option<String>,
}

/// Everything extracted from one file, published in one transaction.
#[derive(Debug, Clone, Default)]
pub struct FileKnowledge {
    pub entities: Vec<EntityRow>,
    pub relationships: Vec<RelationshipRow>,
    pub documents: Vec<DocumentRow>,
    pub chunks: Vec<ChunkRow>,
    /// Email attachment outcomes for this file (reported, not stored).
    pub attachments: AttachmentStats,
}

/// Per-file email attachment outcomes (`AttachmentStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AttachmentStats {
    pub seen: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes_processed: usize,
}

impl std::ops::AddAssign for AttachmentStats {
    fn add_assign(&mut self, o: Self) {
        self.seen += o.seen;
        self.indexed += o.indexed;
        self.skipped += o.skipped;
        self.failed += o.failed;
        self.bytes_processed += o.bytes_processed;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Job {
    pub id: String,
    pub build_id: String,
    pub file_id: String,
    pub rel_path: String,
    pub job_type: String,
    pub status: String,
    pub priority: i64,
    pub attempt_count: i64,
    pub next_attempt_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub id: String,
    pub rank: f64,
}

/// Quotes each whitespace-separated token so arbitrary user input is a
/// valid FTS5 query of AND-ed terms (no operator injection/syntax errors).
pub fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

pub struct ProjectStore {
    pub(crate) conn: Connection,
    source_id: String,
}

impl ProjectStore {
    /// Opens `<home>/projects/<project_id>/knowledge.db`, creating it at
    /// the current schema when missing and refusing any other schema.
    pub fn open(
        layout: &StorageLayout,
        project_id: &str,
        source_id: &str,
        cache_size_mb: i64,
    ) -> Result<Self> {
        let path = layout.project_db(project_id);
        let conn = open_current(&path, cache_size_mb, KNOWLEDGE_SCHEMA)?;
        Ok(Self {
            conn,
            source_id: source_id.to_owned(),
        })
    }

    /// Directory holding this project's database (derived caches such as
    /// the ANN index live next to it).
    pub fn project_dir(&self) -> Option<std::path::PathBuf> {
        self.conn
            .path()
            .filter(|p| !p.is_empty())
            .and_then(|p| std::path::Path::new(p).parent().map(|d| d.to_path_buf()))
    }

    /// The source this project store belongs to.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    // ---- sessions ----------------------------------------------------

    /// Starts one write transaction spanning many operations (an in-place
    /// incremental build). Readers keep seeing the last committed state
    /// until [`commit_session`](Self::commit_session); a crash or
    /// [`rollback_session`](Self::rollback_session) discards everything.
    pub fn begin_session(&mut self) -> Result<()> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(StorageError::sqlite("begin session"))
    }

    pub fn commit_session(&mut self) -> Result<()> {
        self.conn
            .execute_batch("COMMIT")
            .map_err(StorageError::sqlite("commit session"))
    }

    pub fn rollback_session(&mut self) -> Result<()> {
        if self.conn.is_autocommit() {
            return Ok(());
        }
        self.conn
            .execute_batch("ROLLBACK")
            .map_err(StorageError::sqlite("rollback session"))
    }

    pub fn in_session(&self) -> bool {
        !self.conn.is_autocommit()
    }

    /// Records that an in-place incremental update of `build_id` finished.
    pub fn touch_build(&mut self, build_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE builds SET finished_at = ?2 WHERE id = ?1",
                params![build_id, now_iso()],
            )
            .map_err(StorageError::sqlite("touch build"))?;
        Ok(())
    }

    // ---- builds ----------------------------------------------------

    pub fn create_build(
        &mut self,
        build_id: &str,
        full: bool,
        versions: &IndexVersions,
    ) -> Result<()> {
        let v =
            serde_json::to_string(versions).map_err(|e| StorageError::Invalid(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO builds (id, source_id, kind, status, versions, started_at)
                 VALUES (?1, ?2, ?3, 'building', ?4, ?5)",
                params![
                    build_id,
                    self.source_id,
                    if full { "full" } else { "incremental" },
                    v,
                    now_iso()
                ],
            )
            .map_err(StorageError::sqlite("create build"))?;
        Ok(())
    }

    /// Marks `build_id` published and every older published build superseded.
    pub fn mark_published(&mut self, build_id: &str) -> Result<()> {
        let now = now_iso();
        write_tx(&mut self.conn, |tx| {
            tx.execute(
                "UPDATE builds SET status = 'superseded' WHERE status = 'published' AND id <> ?1",
                [build_id],
            )
            .map_err(StorageError::sqlite("supersede builds"))?;
            let n = tx
                .execute(
                    "UPDATE builds SET status = 'published', finished_at = ?2
                     WHERE id = ?1 AND status = 'building'",
                    params![build_id, now],
                )
                .map_err(StorageError::sqlite("publish build"))?;
            if n == 0 {
                return Err(StorageError::Invalid(format!(
                    "build {build_id} is not building"
                )));
            }
            Ok(())
        })
    }

    pub fn mark_aborted(&mut self, build_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE builds SET status = 'aborted', finished_at = ?2 WHERE id = ?1 AND status = 'building'",
                params![build_id, now_iso()],
            )
            .map_err(StorageError::sqlite("abort build"))?;
        Ok(())
    }

    /// Deletes every row of aborted/superseded builds (never the given
    /// active build). Returns the number of builds removed.
    pub fn gc_builds(&mut self, keep_active: Option<&str>) -> Result<usize> {
        let keep = keep_active.unwrap_or("");
        write_tx(&mut self.conn, |tx| {
            let ids: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        // 'building' rows are leftovers of a run that died
                        // after committing a build but before publishing it
                        // (callers hold the run lock, so none is in flight).
                        "SELECT id FROM builds WHERE status IN ('aborted', 'superseded', 'building')
                           AND id <> ?1",
                    )
                    .map_err(StorageError::sqlite("gc builds"))?;
                let rows = stmt
                    .query_map([keep], |r| r.get(0))
                    .map_err(StorageError::sqlite("gc builds"))?;
                rows.collect::<rusqlite::Result<_>>()
                    .map_err(StorageError::sqlite("gc builds"))?
            };
            for id in &ids {
                delete_build_rows(tx, id)?;
                tx.execute("DELETE FROM builds WHERE id = ?1", [id])
                    .map_err(StorageError::sqlite("gc builds"))?;
            }
            Ok(ids.len())
        })
    }

    // ---- files -----------------------------------------------------

    /// File states an indexing pass may compare against. For a full
    /// rebuild this is always empty, so no earlier state can cause a file
    /// to be skipped; for an incremental pass it is the active build's files.
    pub fn reusable_files(&self, plan: &RebuildPlan) -> Result<Vec<FileRow>> {
        match plan {
            RebuildPlan::Full { .. } => Ok(Vec::new()),
            RebuildPlan::Incremental { active_build_id } => self.files(active_build_id),
        }
    }

    pub fn files(&self, build_id: &str) -> Result<Vec<FileRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, rel_path, kind, size, mtime, content_hash, status, parser_version,
                    chunker_version, converter_version, embedding_model_id,
                    embedding_text_version, last_error, attempt_count, next_attempt_at
                 FROM files WHERE build_id = ?1 ORDER BY rel_path",
            )
            .map_err(StorageError::sqlite("list files"))?;
        let rows = stmt
            .query_map([build_id], |r| {
                Ok(FileRow {
                    id: r.get(0)?,
                    rel_path: r.get(1)?,
                    kind: r.get(2)?,
                    size: r.get(3)?,
                    mtime: r.get(4)?,
                    content_hash: r.get(5)?,
                    status: r.get(6)?,
                    parser_version: r.get(7)?,
                    chunker_version: r.get(8)?,
                    converter_version: r.get(9)?,
                    embedding_model_id: r.get(10)?,
                    embedding_text_version: r.get(11)?,
                    last_error: r.get(12)?,
                    attempt_count: r.get(13)?,
                    next_attempt_at: r.get(14)?,
                })
            })
            .map_err(StorageError::sqlite("list files"))?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(StorageError::sqlite("list files"))
    }

    /// Publishes one file and everything extracted from it into `build_id`,
    /// replacing any earlier rows of that file in the same build.
    pub fn put_file(
        &mut self,
        build_id: &str,
        file: &FileRow,
        knowledge: &FileKnowledge,
    ) -> Result<()> {
        self.put_files(build_id, &[(file, knowledge)])
    }

    /// Writes several files in one transaction (the writer batches results
    /// that are already available; each file still replaces all its rows).
    pub fn put_files(
        &mut self,
        build_id: &str,
        files: &[(&FileRow, &FileKnowledge)],
    ) -> Result<()> {
        let now = now_iso();
        let source_id = self.source_id.clone();
        write_tx(&mut self.conn, |tx| {
            for (file, knowledge) in files {
                put_file_rows(tx, &source_id, build_id, file, knowledge, &now)?;
            }
            Ok(())
        })
    }
}

fn put_file_rows(
    tx: &Connection,
    source_id: &str,
    build_id: &str,
    file: &FileRow,
    knowledge: &FileKnowledge,
    now: &str,
) -> Result<()> {
    {
        {
            delete_file_rows(tx, build_id, &file.id)?;
            cached(tx,
                "INSERT INTO files (id, source_id, rel_path, kind, size, mtime, content_hash, status,
                    build_id, parser_version, chunker_version, converter_version,
                    embedding_model_id, embedding_text_version, last_indexed_at, last_error,
                    created_at, updated_at, attempt_count, next_attempt_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?15, ?15,
                    ?17, ?18)",
                params![
                    file.id, source_id, file.rel_path, file.kind, file.size, file.mtime,
                    file.content_hash, file.status, build_id, file.parser_version,
                    file.chunker_version, file.converter_version, file.embedding_model_id,
                    file.embedding_text_version, now, file.last_error, file.attempt_count,
                    file.next_attempt_at,
                ],
            )
            .map_err(StorageError::sqlite("insert file"))?;
            cached(
                tx,
                "INSERT INTO path_fts (rowid, file_id, build_id, path)
                 VALUES (last_insert_rowid(), ?1, ?2, ?3)",
                params![file.id, build_id, file.rel_path],
            )
            .map_err(StorageError::sqlite("index path"))?;
            insert_knowledge(tx, build_id, knowledge)
        }
    }
}

impl ProjectStore {
    /// Copies an unchanged file's rows from the active build into a new
    /// incremental build without re-extracting it.
    pub fn carry_forward(&mut self, from_build: &str, to_build: &str, file_id: &str) -> Result<()> {
        write_tx(&mut self.conn, |tx| {
            delete_file_rows(tx, to_build, file_id)?;
            let exec = |sql: &str| {
                cached(tx, sql, params![to_build, from_build, file_id])
                    .map_err(StorageError::sqlite("carry forward"))
            };
            exec("INSERT INTO files (id, source_id, rel_path, kind, size, mtime, content_hash, status,
                    build_id, parser_version, chunker_version, converter_version, embedding_model_id,
                    embedding_text_version, last_indexed_at, last_error, created_at, updated_at,
                    attempt_count, next_attempt_at)
                  SELECT id, source_id, rel_path, kind, size, mtime, content_hash, status,
                    ?1, parser_version, chunker_version, converter_version, embedding_model_id,
                    embedding_text_version, last_indexed_at, last_error, created_at, updated_at,
                    attempt_count, next_attempt_at
                  FROM files WHERE build_id = ?2 AND id = ?3")?;
            exec(
                "INSERT INTO path_fts (rowid, file_id, build_id, path)
                  SELECT rowid, id, ?1, rel_path FROM files WHERE build_id = ?1 AND id = ?3",
            )?;
            exec(
                "INSERT INTO entities SELECT id, ?1, file_id, kind, name, qualified_name, language,
                    parent_id, signature, start_line, end_line, start_col, end_col
                  FROM entities WHERE build_id = ?2 AND file_id = ?3",
            )?;
            exec(
                "INSERT INTO code_fts (rowid, entity_id, build_id, name, qualified_name, signature)
                  SELECT rowid, id, ?1, name, qualified_name, COALESCE(signature, name)
                  FROM entities WHERE build_id = ?1 AND file_id = ?3",
            )?;
            exec("INSERT INTO relationships SELECT id, ?1, file_id, relationship_type, source_entity_id,
                    target_entity_id, target_symbol, resolver, confidence, source_location, evidence,
                    reference_text
                  FROM relationships WHERE build_id = ?2 AND file_id = ?3")?;
            exec(
                "INSERT INTO documents SELECT id, ?1, file_id, format, title, author, page_count,
                    is_scanned, content_hash, parent_document_id, attachment_name,
                    attachment_content_type, attachment_index, attachment_content_id
                  FROM documents WHERE build_id = ?2 AND file_id = ?3",
            )?;
            exec("INSERT INTO chunks SELECT id, ?1, document_id, file_id, kind, ordinal, heading_path,
                    heading_level, text, search_text, embedding_text, token_count, page_start,
                    page_end, table_rows, caption, parent_ordinal
                  FROM chunks WHERE build_id = ?2 AND file_id = ?3")?;
            exec(
                "INSERT INTO embeddings SELECT ?1, subject_type, subject_id, file_id,
                    model_fingerprint, text_hash, dims, vector
                  FROM embeddings WHERE build_id = ?2 AND file_id = ?3",
            )?;
            exec(
                "INSERT INTO chunk_fts (rowid, chunk_id, build_id, heading, body, title)
                  SELECT n.rowid, n.id, ?1, f.heading, f.body, f.title
                  FROM chunks o JOIN chunk_fts f ON f.rowid = o.rowid
                    JOIN chunks n ON n.build_id = ?1 AND n.id = o.id
                  WHERE o.build_id = ?2 AND o.file_id = ?3",
            )?;
            Ok(())
        })
    }

    /// Copies every file of `from_build` except `exclude` (and all of their
    /// knowledge) into `to_build` in one set-based transaction, applying
    /// `restat` `(file_id, size, mtime)` updates. This is the incremental
    /// fast path: cost is a few statements, not one transaction per file.
    pub fn carry_forward_except(
        &mut self,
        from_build: &str,
        to_build: &str,
        exclude: &[String],
        restat: &[(String, i64, f64)],
    ) -> Result<usize> {
        write_tx(&mut self.conn, |tx| {
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS carry_skip (id TEXT PRIMARY KEY);
                 DELETE FROM carry_skip;",
            )
            .map_err(StorageError::sqlite("prepare carry"))?;
            {
                let mut ins = tx
                    .prepare("INSERT OR IGNORE INTO carry_skip (id) VALUES (?1)")
                    .map_err(StorageError::sqlite("prepare carry"))?;
                for id in exclude {
                    ins.execute([id])
                        .map_err(StorageError::sqlite("prepare carry"))?;
                }
            }
            let exec = |sql: &str| {
                cached(tx, sql, params![to_build, from_build])
                    .map_err(StorageError::sqlite("carry forward"))
            };
            let n = exec(
                "INSERT INTO files (id, source_id, rel_path, kind, size, mtime, content_hash, status,
                    build_id, parser_version, chunker_version, converter_version, embedding_model_id,
                    embedding_text_version, last_indexed_at, last_error, created_at, updated_at,
                    attempt_count, next_attempt_at)
                 SELECT id, source_id, rel_path, kind, size, mtime, content_hash, status,
                    ?1, parser_version, chunker_version, converter_version, embedding_model_id,
                    embedding_text_version, last_indexed_at, last_error, created_at, updated_at,
                    attempt_count, next_attempt_at
                 FROM files WHERE build_id = ?2 AND id NOT IN (SELECT id FROM carry_skip)",
            )?;
            exec(
                "INSERT INTO path_fts (rowid, file_id, build_id, path)
                  SELECT n.rowid, n.id, ?1, n.rel_path
                  FROM files o JOIN files n ON n.build_id = ?1 AND n.id = o.id
                  WHERE o.build_id = ?2 AND o.id NOT IN (SELECT id FROM carry_skip)",
            )?;
            exec(
                "INSERT INTO entities SELECT id, ?1, file_id, kind, name, qualified_name, language,
                    parent_id, signature, start_line, end_line, start_col, end_col
                  FROM entities WHERE build_id = ?2 AND file_id NOT IN (SELECT id FROM carry_skip)",
            )?;
            exec(
                "INSERT INTO code_fts (rowid, entity_id, build_id, name, qualified_name, signature)
                  SELECT n.rowid, n.id, ?1, n.name, n.qualified_name, COALESCE(n.signature, n.name)
                  FROM entities o JOIN entities n ON n.build_id = ?1 AND n.id = o.id
                  WHERE o.build_id = ?2 AND o.file_id NOT IN (SELECT id FROM carry_skip)",
            )?;
            exec("INSERT INTO relationships SELECT id, ?1, file_id, relationship_type, source_entity_id,
                    target_entity_id, target_symbol, resolver, confidence, source_location, evidence,
                    reference_text
                  FROM relationships WHERE build_id = ?2 AND file_id NOT IN (SELECT id FROM carry_skip)")?;
            exec("INSERT INTO documents SELECT id, ?1, file_id, format, title, author, page_count,
                    is_scanned, content_hash, parent_document_id, attachment_name,
                    attachment_content_type, attachment_index, attachment_content_id
                  FROM documents WHERE build_id = ?2 AND file_id NOT IN (SELECT id FROM carry_skip)")?;
            exec("INSERT INTO chunks SELECT id, ?1, document_id, file_id, kind, ordinal, heading_path,
                    heading_level, text, search_text, embedding_text, token_count, page_start,
                    page_end, table_rows, caption, parent_ordinal
                  FROM chunks WHERE build_id = ?2 AND file_id NOT IN (SELECT id FROM carry_skip)")?;
            exec(
                "INSERT INTO embeddings SELECT ?1, subject_type, subject_id, file_id,
                    model_fingerprint, text_hash, dims, vector
                  FROM embeddings WHERE build_id = ?2 AND file_id NOT IN (SELECT id FROM carry_skip)",
            )?;
            exec(
                "INSERT INTO chunk_fts (rowid, chunk_id, build_id, heading, body, title)
                  SELECT n.rowid, n.id, ?1, f.heading, f.body, f.title
                  FROM chunks o JOIN chunk_fts f ON f.rowid = o.rowid
                    JOIN chunks n ON n.build_id = ?1 AND n.id = o.id
                  WHERE o.build_id = ?2 AND o.file_id NOT IN (SELECT id FROM carry_skip)",
            )?;
            {
                let mut up = tx
                    .prepare(
                        "UPDATE files SET size = ?1, mtime = ?2 WHERE build_id = ?3 AND id = ?4",
                    )
                    .map_err(StorageError::sqlite("restat"))?;
                for (id, size, mtime) in restat {
                    up.execute(params![size, mtime, to_build, id])
                        .map_err(StorageError::sqlite("restat"))?;
                }
            }
            Ok(n)
        })
    }

    /// Updates the stat of a carried-forward file whose content hash proved
    /// unchanged, so the next run can fast-skip it without re-hashing.
    pub fn set_file_stat(
        &mut self,
        build_id: &str,
        file_id: &str,
        size: i64,
        mtime: f64,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE files SET size = ?1, mtime = ?2 WHERE build_id = ?3 AND id = ?4",
                params![size, mtime, build_id, file_id],
            )
            .map_err(StorageError::sqlite("update file stat"))?;
        Ok(())
    }

    /// Removes a file from a build (deleted/moved files in an incremental build).
    pub fn remove_file(&mut self, build_id: &str, file_id: &str) -> Result<()> {
        write_tx(&mut self.conn, |tx| delete_file_rows(tx, build_id, file_id))
    }

    // ---- links -----------------------------------------------------

    pub fn put_links(&mut self, build_id: &str, links: &[LinkRow]) -> Result<()> {
        write_tx(&mut self.conn, |tx| {
            let mut stmt = tx
                .prepare(
                    // First wins on the natural key (link ids hash it), like
                    // INSERT OR IGNORE.
                    "INSERT OR IGNORE INTO cross_links (id, build_id, link_type, entity_id,
                        document_id, chunk_id, resolver, confidence, evidence)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(StorageError::sqlite("insert links"))?;
            for l in links {
                stmt.execute(params![
                    l.id,
                    build_id,
                    l.link_type,
                    l.entity_id,
                    l.document_id,
                    l.chunk_id,
                    l.resolver,
                    l.confidence,
                    l.evidence
                ])
                .map_err(StorageError::sqlite("insert link"))?;
            }
            Ok(())
        })
    }

    // ---- reads (always scoped to one build) ---------------------------

    pub fn entities_named(&self, build_id: &str, name: &str) -> Result<Vec<EntityRow>> {
        self.query_rows(
            "entities by name",
            &format!(
                "SELECT {ENTITY_COLUMNS} FROM entities
                 WHERE build_id = ?1 AND (name = ?2 OR qualified_name = ?2)
                 ORDER BY qualified_name, id"
            ),
            &[&build_id, &name],
            entity_from_row,
        )
    }

    pub fn documents(&self, build_id: &str) -> Result<Vec<DocumentRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, file_id, format, title, author, page_count, is_scanned, content_hash,
                    parent_document_id, attachment_name, attachment_content_type,
                    attachment_index, attachment_content_id
                 FROM documents WHERE build_id = ?1
                 ORDER BY file_id, COALESCE(attachment_index, -1)",
            )
            .map_err(StorageError::sqlite("list documents"))?;
        let rows = stmt
            .query_map([build_id], |r| {
                let parent: Option<String> = r.get(8)?;
                let index: Option<i64> = r.get(11)?;
                Ok(DocumentRow {
                    id: r.get(0)?,
                    file_id: r.get(1)?,
                    format: r.get(2)?,
                    title: r.get(3)?,
                    author: r.get(4)?,
                    page_count: r.get(5)?,
                    is_scanned: r.get::<_, i64>(6)? != 0,
                    content_hash: r.get(7)?,
                    attachment: match (parent, index) {
                        (Some(parent_document_id), Some(index)) => Some(AttachmentProvenance {
                            parent_document_id,
                            name: r.get(9)?,
                            content_type: r.get(10)?,
                            index,
                            content_id: r.get(12)?,
                        }),
                        _ => None,
                    },
                })
            })
            .map_err(StorageError::sqlite("list documents"))?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(StorageError::sqlite("list documents"))
    }

    /// Chunk counts per document: `(headings, paragraphs, tables)`. Reported
    /// as `section_count`, `paragraph_count` and
    /// `table_count`, counted from the same chunks.
    pub fn chunk_kind_counts(
        &self,
        build_id: &str,
    ) -> Result<std::collections::HashMap<String, (i64, i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT document_id, kind, COUNT(*) FROM chunks WHERE build_id = ?1
                 GROUP BY document_id, kind",
            )
            .map_err(StorageError::sqlite("count chunks"))?;
        let rows = stmt
            .query_map([build_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(StorageError::sqlite("count chunks"))?;
        let mut out: std::collections::HashMap<String, (i64, i64, i64)> = Default::default();
        for row in rows {
            let (doc, kind, n) = row.map_err(StorageError::sqlite("count chunks"))?;
            let e = out.entry(doc).or_default();
            match kind.as_str() {
                "heading" => e.0 += n,
                "paragraph" => e.1 += n,
                "table" => e.2 += n,
                _ => {}
            }
        }
        Ok(out)
    }

    fn fts(&self, sql: &str, build_id: &str, query: &str, limit: i64) -> Result<Vec<Hit>> {
        let Some(q) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(StorageError::sqlite("fts search"))?;
        let rows = stmt
            .query_map(params![q, build_id, limit], |r| {
                Ok(Hit {
                    id: r.get(0)?,
                    rank: r.get(1)?,
                })
            })
            .map_err(StorageError::sqlite("fts search"))?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(StorageError::sqlite("fts search"))
    }

    /// BM25-ranked chunk ids (best first) within one build.
    pub fn search_chunks(&self, build_id: &str, query: &str, limit: i64) -> Result<Vec<Hit>> {
        self.fts(
            "SELECT chunk_id, bm25(chunk_fts) FROM chunk_fts
             WHERE chunk_fts MATCH ?1 AND build_id = ?2 ORDER BY bm25(chunk_fts), chunk_id LIMIT ?3",
            build_id,
            query,
            limit,
        )
    }

    pub fn search_code(&self, build_id: &str, query: &str, limit: i64) -> Result<Vec<Hit>> {
        self.fts(
            "SELECT entity_id, bm25(code_fts) FROM code_fts
             WHERE code_fts MATCH ?1 AND build_id = ?2 ORDER BY bm25(code_fts), entity_id LIMIT ?3",
            build_id,
            query,
            limit,
        )
    }

    pub fn search_paths(&self, build_id: &str, query: &str, limit: i64) -> Result<Vec<Hit>> {
        self.fts(
            "SELECT file_id, bm25(path_fts) FROM path_fts
             WHERE path_fts MATCH ?1 AND build_id = ?2 ORDER BY bm25(path_fts), file_id LIMIT ?3",
            build_id,
            query,
            limit,
        )
    }

    pub fn count(&self, table: &str, build_id: &str) -> Result<i64> {
        const ALLOWED: &[&str] = &[
            "files",
            "entities",
            "relationships",
            "documents",
            "chunks",
            "cross_links",
        ];
        if !ALLOWED.contains(&table) {
            return Err(StorageError::Invalid(format!("cannot count {table}")));
        }
        self.conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE build_id = ?1"),
                [build_id],
                |r| r.get(0),
            )
            .map_err(StorageError::sqlite("count"))
    }

    // ---- jobs & errors -----------------------------------------------

    pub fn enqueue(
        &mut self,
        build_id: &str,
        file_id: &str,
        rel_path: &str,
        priority: i64,
    ) -> Result<String> {
        let id = docmonk_core::ids::record::build_id(build_id, &format!("job\x1f{file_id}"));
        self.conn
            .execute(
                "INSERT OR IGNORE INTO index_jobs (id, build_id, file_id, rel_path, job_type, status,
                    priority, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'index_file', 'queued', ?5, ?6)",
                params![id, build_id, file_id, rel_path, priority, now_iso()],
            )
            .map_err(StorageError::sqlite("enqueue"))?;
        Ok(id)
    }

    /// Claims the next due job (priority, then age) and marks it processing.
    pub fn claim_next(&mut self, build_id: &str) -> Result<Option<Job>> {
        let now = now_iso();
        write_tx(&mut self.conn, |tx| {
            let job = tx
                .query_row(
                    "SELECT id, build_id, file_id, rel_path, job_type, status, priority,
                        attempt_count, next_attempt_at
                     FROM index_jobs
                     WHERE build_id = ?1 AND status IN ('queued', 'retry')
                       AND (next_attempt_at IS NULL OR next_attempt_at <= ?2)
                     ORDER BY priority DESC, created_at ASC, id ASC LIMIT 1",
                    params![build_id, now],
                    |r| {
                        Ok(Job {
                            id: r.get(0)?,
                            build_id: r.get(1)?,
                            file_id: r.get(2)?,
                            rel_path: r.get(3)?,
                            job_type: r.get(4)?,
                            status: "processing".into(),
                            priority: r.get(6)?,
                            attempt_count: r.get(7)?,
                            next_attempt_at: r.get(8)?,
                        })
                    },
                )
                .optional()
                .map_err(StorageError::sqlite("claim job"))?;
            if let Some(job) = &job {
                cached(
                    tx,
                    "UPDATE index_jobs SET status = 'processing', started_at = ?1 WHERE id = ?2",
                    params![now, job.id],
                )
                .map_err(StorageError::sqlite("claim job"))?;
            }
            Ok(job)
        })
    }

    pub fn complete_job(&mut self, job_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE index_jobs SET status = 'completed', completed_at = ?1 WHERE id = ?2",
                params![now_iso(), job_id],
            )
            .map_err(StorageError::sqlite("complete job"))?;
        Ok(())
    }

    /// `permanent` = failed; otherwise retry at `next_attempt_at`.
    pub fn fail_job(
        &mut self,
        job_id: &str,
        error_code: &str,
        error_message: &str,
        next_attempt_at: Option<&str>,
        permanent: bool,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE index_jobs SET status = ?1, attempt_count = attempt_count + 1,
                    next_attempt_at = ?2, error_code = ?3, error_message = ?4,
                    completed_at = ?5 WHERE id = ?6",
                params![
                    if permanent { "failed" } else { "retry" },
                    next_attempt_at,
                    error_code,
                    error_message,
                    permanent.then(now_iso),
                    job_id
                ],
            )
            .map_err(StorageError::sqlite("fail job"))?;
        Ok(())
    }

    /// Crash recovery: jobs left `processing` go back to `queued`.
    pub fn recover_stuck(&mut self) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE index_jobs SET status = 'queued', started_at = NULL WHERE status = 'processing'",
                [],
            )
            .map_err(StorageError::sqlite("recover stuck jobs"))
    }

    pub fn queue_depth(&self, build_id: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM index_jobs WHERE build_id = ?1 AND status IN ('queued', 'retry', 'processing')",
                [build_id],
                |r| r.get(0),
            )
            .map_err(StorageError::sqlite("queue depth"))
    }

    pub fn record_error(
        &mut self,
        build_id: Option<&str>,
        file_id: Option<&str>,
        rel_path: Option<&str>,
        code: &str,
        message: &str,
    ) -> Result<()> {
        let now = now_iso();
        let id = docmonk_core::ids::record::build_id(
            &self.source_id,
            &format!("error\x1f{now}\x1f{code}\x1f{}", rel_path.unwrap_or("")),
        );
        self.conn
            .execute(
                "INSERT OR REPLACE INTO index_errors (id, source_id, build_id, file_id, rel_path,
                    error_code, error_message, occurred_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    self.source_id,
                    build_id,
                    file_id,
                    rel_path,
                    code,
                    message,
                    now
                ],
            )
            .map_err(StorageError::sqlite("record error"))?;
        Ok(())
    }
}

fn delete_build_rows(tx: &Connection, build_id: &str) -> Result<()> {
    for sql in [
        "DELETE FROM code_fts WHERE rowid IN (SELECT rowid FROM entities WHERE build_id = ?1)",
        "DELETE FROM chunk_fts WHERE rowid IN (SELECT rowid FROM chunks WHERE build_id = ?1)",
        "DELETE FROM path_fts WHERE rowid IN (SELECT rowid FROM files WHERE build_id = ?1)",
        "DELETE FROM cross_links WHERE build_id = ?1",
        "DELETE FROM embeddings WHERE build_id = ?1",
        "DELETE FROM chunks WHERE build_id = ?1",
        "DELETE FROM documents WHERE build_id = ?1",
        "DELETE FROM relationships WHERE build_id = ?1",
        "DELETE FROM entities WHERE build_id = ?1",
        "DELETE FROM index_jobs WHERE build_id = ?1",
        "DELETE FROM files WHERE build_id = ?1",
    ] {
        cached(tx, sql, [build_id]).map_err(StorageError::sqlite("delete build rows"))?;
    }
    Ok(())
}

fn delete_file_rows(tx: &Connection, build_id: &str, file_id: &str) -> Result<()> {
    for sql in [
        "DELETE FROM code_fts WHERE rowid IN
            (SELECT rowid FROM entities WHERE build_id = ?1 AND file_id = ?2)",
        "DELETE FROM chunk_fts WHERE rowid IN
            (SELECT rowid FROM chunks WHERE build_id = ?1 AND file_id = ?2)",
        "DELETE FROM path_fts WHERE rowid IN
            (SELECT rowid FROM files WHERE build_id = ?1 AND id = ?2)",
        "DELETE FROM cross_links WHERE build_id = ?1 AND document_id IN
            (SELECT id FROM documents WHERE build_id = ?1 AND file_id = ?2)",
        "DELETE FROM cross_links WHERE build_id = ?1 AND entity_id IN
            (SELECT id FROM entities WHERE build_id = ?1 AND file_id = ?2)",
        "DELETE FROM embeddings WHERE build_id = ?1 AND file_id = ?2",
        "DELETE FROM chunks WHERE build_id = ?1 AND file_id = ?2",
        "DELETE FROM documents WHERE build_id = ?1 AND file_id = ?2",
        "DELETE FROM relationships WHERE build_id = ?1 AND file_id = ?2",
        "DELETE FROM entities WHERE build_id = ?1 AND file_id = ?2",
        "DELETE FROM files WHERE build_id = ?1 AND id = ?2",
    ] {
        cached(tx, sql, params![build_id, file_id])
            .map_err(StorageError::sqlite("delete file rows"))?;
    }
    Ok(())
}

fn insert_knowledge(tx: &Connection, build_id: &str, k: &FileKnowledge) -> Result<()> {
    let err = |what: &'static str| StorageError::sqlite(what);
    for e in &k.entities {
        cached(
            tx,
            "INSERT INTO entities (id, build_id, file_id, kind, name, qualified_name, language,
                parent_id, signature, start_line, end_line, start_col, end_col)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                e.id,
                build_id,
                e.file_id,
                e.kind,
                e.name,
                e.qualified_name,
                e.language,
                e.parent_id,
                e.signature,
                e.start_line,
                e.end_line,
                e.start_col,
                e.end_col
            ],
        )
        .map_err(err("insert entity"))?;
        cached(
            tx,
            "INSERT INTO code_fts (rowid, entity_id, build_id, name, qualified_name, signature)
             VALUES (last_insert_rowid(), ?1, ?2, ?3, ?4, ?5)",
            params![
                e.id,
                build_id,
                e.name,
                e.qualified_name,
                e.signature.clone().unwrap_or_else(|| e.name.clone())
            ],
        )
        .map_err(err("index entity"))?;
    }
    for r in &k.relationships {
        cached(
            tx,
            "INSERT INTO relationships (id, build_id, file_id, relationship_type, source_entity_id,
                target_entity_id, target_symbol, resolver, confidence, source_location, evidence,
                reference_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                r.id,
                build_id,
                r.file_id,
                r.relationship_type,
                r.source_entity_id,
                r.target_entity_id,
                r.target_symbol,
                r.resolver,
                r.confidence,
                r.source_location,
                r.evidence,
                r.reference_text
            ],
        )
        .map_err(err("insert relationship"))?;
    }
    for d in &k.documents {
        let a = d.attachment.as_ref();
        cached(
            tx,
            "INSERT INTO documents (id, build_id, file_id, format, title, author, page_count,
                is_scanned, content_hash, parent_document_id, attachment_name,
                attachment_content_type, attachment_index, attachment_content_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                d.id,
                build_id,
                d.file_id,
                d.format,
                d.title,
                d.author,
                d.page_count,
                i64::from(d.is_scanned),
                d.content_hash,
                a.map(|a| a.parent_document_id.clone()),
                a.and_then(|a| a.name.clone()),
                a.and_then(|a| a.content_type.clone()),
                a.map(|a| a.index),
                a.and_then(|a| a.content_id.clone())
            ],
        )
        .map_err(err("insert document"))?;
    }
    for c in &k.chunks {
        let heading_path = serde_json::to_string(&c.heading_path).unwrap_or_else(|_| "[]".into());
        let rows = c
            .table_rows
            .as_ref()
            .map(|r| serde_json::to_string(r).unwrap_or_else(|_| "[]".into()));
        cached(
            tx,
            "INSERT INTO chunks (id, build_id, document_id, file_id, kind, ordinal, heading_path,
                heading_level, text, search_text, embedding_text, token_count, page_start,
                page_end, table_rows, caption, parent_ordinal)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                c.id,
                build_id,
                c.document_id,
                c.file_id,
                c.kind,
                c.ordinal,
                heading_path,
                c.heading_level,
                c.text,
                c.search_text,
                c.embedding_text,
                c.token_count,
                c.page_start,
                c.page_end,
                rows,
                c.caption,
                c.parent_ordinal
            ],
        )
        .map_err(err("insert chunk"))?;
        let title: Option<String> = k
            .documents
            .iter()
            .find(|d| d.id == c.document_id)
            .and_then(|d| d.title.clone());
        cached(
            tx,
            "INSERT INTO chunk_fts (rowid, chunk_id, build_id, heading, body, title)
             VALUES (last_insert_rowid(), ?1, ?2, ?3, ?4, ?5)",
            params![
                c.id,
                build_id,
                // A heading chunk is indexed under its own text, every other
                // chunk under its heading path (its
                // `fts_heading`).
                if c.kind.as_str() == "heading" {
                    c.text.clone()
                } else {
                    c.heading_path.join(" > ")
                },
                c.search_text,
                title.unwrap_or_default()
            ],
        )
        .map_err(err("index chunk"))?;
    }
    Ok(())
}

// ---- code graph -----------------------------------------------------

pub(crate) const ENTITY_COLUMNS: &str =
    "id, file_id, kind, name, qualified_name, language, parent_id,
    signature, start_line, end_line, start_col, end_col";
pub(crate) const RELATIONSHIP_COLUMNS: &str = "id, file_id, relationship_type, source_entity_id,
    target_entity_id, target_symbol, resolver, confidence, source_location, evidence,
    reference_text";

/// Resolvers whose outcome depends on other files and is therefore
/// recomputed against the whole build before publication.
pub const CROSS_FILE_RESOLVERS: &[&str] = &[
    "pending",
    "cross_file_qualified",
    "cross_file_qualified_ambiguous",
    "name_only",
    "unresolved",
];

pub(crate) fn entity_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EntityRow> {
    Ok(EntityRow {
        id: r.get(0)?,
        file_id: r.get(1)?,
        kind: r.get(2)?,
        name: r.get(3)?,
        qualified_name: r.get(4)?,
        language: r.get(5)?,
        parent_id: r.get(6)?,
        signature: r.get(7)?,
        start_line: r.get(8)?,
        end_line: r.get(9)?,
        start_col: r.get(10)?,
        end_col: r.get(11)?,
    })
}

pub(crate) fn relationship_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RelationshipRow> {
    Ok(RelationshipRow {
        id: r.get(0)?,
        file_id: r.get(1)?,
        relationship_type: r.get(2)?,
        source_entity_id: r.get(3)?,
        target_entity_id: r.get(4)?,
        target_symbol: r.get(5)?,
        resolver: r.get(6)?,
        confidence: r.get(7)?,
        source_location: r.get(8)?,
        evidence: r.get(9)?,
        reference_text: r.get(10)?,
    })
}

/// A new cross-file resolution outcome for one relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub id: String,
    pub target_entity_id: Option<String>,
    pub target_symbol: Option<String>,
    pub resolver: String,
    pub confidence: String,
}

/// Which endpoint of a relationship to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Source,
    Target,
}

impl ProjectStore {
    pub(crate) fn query_rows<T>(
        &self,
        what: &'static str,
        sql: &str,
        args: &[&dyn rusqlite::ToSql],
        map: fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare(sql).map_err(StorageError::sqlite(what))?;
        let rows = stmt
            .query_map(args, map)
            .map_err(StorageError::sqlite(what))?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(StorageError::sqlite(what))
    }

    /// Every entity of the build (for whole-build resolution).
    pub fn all_entities(&self, build_id: &str) -> Result<Vec<EntityRow>> {
        self.query_rows(
            "all entities",
            &format!("SELECT {ENTITY_COLUMNS} FROM entities WHERE build_id = ?1 ORDER BY file_id, start_line, id"),
            &[&build_id],
            entity_from_row,
        )
    }

    pub fn entity(&self, build_id: &str, id: &str) -> Result<Option<EntityRow>> {
        Ok(self
            .query_rows(
                "entity",
                &format!("SELECT {ENTITY_COLUMNS} FROM entities WHERE build_id = ?1 AND id = ?2"),
                &[&build_id, &id],
                entity_from_row,
            )?
            .pop())
    }

    pub fn file_entities(&self, build_id: &str, file_id: &str) -> Result<Vec<EntityRow>> {
        self.query_rows(
            "file entities",
            &format!(
                "SELECT {ENTITY_COLUMNS} FROM entities WHERE build_id = ?1 AND file_id = ?2
                 ORDER BY start_line, start_col, id"
            ),
            &[&build_id, &file_id],
            entity_from_row,
        )
    }

    pub fn file_relationships(
        &self,
        build_id: &str,
        file_id: &str,
    ) -> Result<Vec<RelationshipRow>> {
        self.query_rows(
            "file relationships",
            &format!(
                "SELECT {RELATIONSHIP_COLUMNS} FROM relationships
                 WHERE build_id = ?1 AND file_id = ?2 ORDER BY id"
            ),
            &[&build_id, &file_id],
            relationship_from_row,
        )
    }

    /// Relationships whose target depends on other files.
    pub fn cross_file_references(&self, build_id: &str) -> Result<Vec<RelationshipRow>> {
        let list = CROSS_FILE_RESOLVERS
            .iter()
            .map(|r| format!("'{r}'"))
            .collect::<Vec<_>>()
            .join(",");
        self.query_rows(
            "cross-file references",
            &format!(
                "SELECT {RELATIONSHIP_COLUMNS} FROM relationships
                 WHERE build_id = ?1 AND reference_text IS NOT NULL AND resolver IN ({list})
                 ORDER BY id"
            ),
            &[&build_id],
            relationship_from_row,
        )
    }

    /// Applies resolution outcomes in one transaction; returns rows changed.
    pub fn apply_resolutions(&mut self, build_id: &str, outcomes: &[Resolution]) -> Result<usize> {
        if outcomes.is_empty() {
            return Ok(0);
        }
        write_tx(&mut self.conn, |tx| {
            let mut stmt = tx
                .prepare(
                    "UPDATE relationships SET target_entity_id = ?3, target_symbol = ?4,
                        resolver = ?5, confidence = ?6
                     WHERE build_id = ?1 AND id = ?2",
                )
                .map_err(StorageError::sqlite("apply resolution"))?;
            let mut n = 0;
            for o in outcomes {
                n += stmt
                    .execute(params![
                        build_id,
                        o.id,
                        o.target_entity_id,
                        o.target_symbol,
                        o.resolver,
                        o.confidence
                    ])
                    .map_err(StorageError::sqlite("apply resolution"))?;
            }
            Ok(n)
        })
    }

    /// Relationships touching `entity_id` at `endpoint`, optionally filtered
    /// by type, deterministically ordered (type, target, id).
    pub fn edges(
        &self,
        build_id: &str,
        endpoint: Endpoint,
        entity_id: &str,
        types: &[&str],
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        let column = match endpoint {
            Endpoint::Source => "source_entity_id",
            Endpoint::Target => "target_entity_id",
        };
        self.edges_where(build_id, column, entity_id, types, limit)
    }

    /// Unresolved relationships recorded only under a bare symbol.
    pub fn edges_to_symbol(
        &self,
        build_id: &str,
        symbol: &str,
        types: &[&str],
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        self.edges_where(build_id, "target_symbol", symbol, types, limit)
    }

    fn edges_where(
        &self,
        build_id: &str,
        column: &str,
        value: &str,
        types: &[&str],
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        // Graph rows are only visible when they match the active base.
        if !self.graph_visible(build_id)? {
            return Ok(Vec::new());
        }
        let mut sql = format!(
            "SELECT {RELATIONSHIP_COLUMNS} FROM relationships WHERE build_id = ?1 AND {column} = ?2"
        );
        let type_list: Vec<String> = types.iter().map(|t| (*t).to_owned()).collect();
        if !type_list.is_empty() {
            let marks = (0..type_list.len())
                .map(|i| format!("?{}", i + 4))
                .collect::<Vec<_>>()
                .join(",");
            sql.push_str(&format!(" AND relationship_type IN ({marks})"));
        }
        sql.push_str(
            " ORDER BY relationship_type, COALESCE(target_entity_id, target_symbol, ''), id LIMIT ?3",
        );
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&build_id, &value, &limit];
        for t in &type_list {
            args.push(t);
        }
        self.query_rows("edges", &sql, &args, relationship_from_row)
    }
}

/// Executes through the connection's prepared-statement cache (the writer
/// runs the same few statements for every file).
pub(crate) fn cached(
    tx: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<usize> {
    tx.prepare_cached(sql)?.execute(params)
}

impl ProjectStore {
    /// A file's chunks in ordinal order.
    pub fn file_chunks(&self, build_id: &str, file_id: &str) -> Result<Vec<ChunkRow>> {
        self.query_rows(
            "file chunks",
            &format!(
                "SELECT {CHUNK_COLUMNS} FROM chunks WHERE build_id = ?1 AND file_id = ?2
                 ORDER BY ordinal"
            ),
            &[&build_id, &file_id],
            chunk_from_row,
        )
    }

    /// One chunk by id.
    pub fn chunk(&self, build_id: &str, id: &str) -> Result<Option<ChunkRow>> {
        Ok(self
            .query_rows(
                "chunk",
                &format!("SELECT {CHUNK_COLUMNS} FROM chunks WHERE build_id = ?1 AND id = ?2"),
                &[&build_id, &id],
                chunk_from_row,
            )?
            .pop())
    }

    /// The chunk at `ordinal` in `document_id`.
    pub fn chunk_at(
        &self,
        build_id: &str,
        document_id: &str,
        ordinal: i64,
    ) -> Result<Option<ChunkRow>> {
        Ok(self
            .query_rows(
                "chunk at",
                &format!(
                    "SELECT {CHUNK_COLUMNS} FROM chunks
                     WHERE build_id = ?1 AND document_id = ?2 AND ordinal = ?3"
                ),
                &[&build_id, &document_id, &ordinal],
                chunk_from_row,
            )?
            .pop())
    }

    /// Siblings of `c` (same document and enclosing heading), at most
    /// `previous` before and `next` after it, each in ordinal order
    /// (`documents_repo.get_chunk_neighbors`).
    pub fn chunk_siblings(
        &self,
        build_id: &str,
        c: &ChunkRow,
        previous: i64,
        next: i64,
    ) -> Result<(Vec<ChunkRow>, Vec<ChunkRow>)> {
        let mut before = if previous > 0 {
            self.query_rows(
                "chunk siblings",
                &format!(
                    "SELECT {CHUNK_COLUMNS} FROM chunks
                     WHERE build_id = ?1 AND document_id = ?2 AND parent_ordinal IS ?3
                       AND ordinal < ?4 ORDER BY ordinal DESC LIMIT ?5"
                ),
                &[
                    &build_id,
                    &c.document_id,
                    &c.parent_ordinal,
                    &c.ordinal,
                    &previous,
                ],
                chunk_from_row,
            )?
        } else {
            Vec::new()
        };
        before.reverse();
        let after = if next > 0 {
            self.query_rows(
                "chunk siblings",
                &format!(
                    "SELECT {CHUNK_COLUMNS} FROM chunks
                     WHERE build_id = ?1 AND document_id = ?2 AND parent_ordinal IS ?3
                       AND ordinal > ?4 ORDER BY ordinal LIMIT ?5"
                ),
                &[
                    &build_id,
                    &c.document_id,
                    &c.parent_ordinal,
                    &c.ordinal,
                    &next,
                ],
                chunk_from_row,
            )?
        } else {
            Vec::new()
        };
        Ok((before, after))
    }
}

const CHUNK_COLUMNS: &str = "id, document_id, file_id, kind, ordinal, heading_path, heading_level,
    text, search_text, embedding_text, token_count, page_start, page_end, table_rows, caption,
    parent_ordinal";

fn chunk_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    let heading_path: String = r.get(5)?;
    let table_rows: Option<String> = r.get(13)?;
    Ok(ChunkRow {
        id: r.get(0)?,
        document_id: r.get(1)?,
        file_id: r.get(2)?,
        kind: r.get(3)?,
        ordinal: r.get(4)?,
        heading_path: serde_json::from_str(&heading_path).unwrap_or_default(),
        heading_level: r.get(6)?,
        text: r.get(7)?,
        search_text: r.get(8)?,
        embedding_text: r.get(9)?,
        token_count: r.get(10)?,
        page_start: r.get(11)?,
        page_end: r.get(12)?,
        table_rows: table_rows.and_then(|t| serde_json::from_str(&t).ok()),
        caption: r.get(14)?,
        parent_ordinal: r.get(15)?,
    })
}

// ---- knowledge linker -----------------------------------------------

/// A chunk as the linker sees it (`text` is raw; tables carry rows).
#[derive(Debug, Clone, PartialEq)]
pub struct LinkUnitRow {
    pub id: String,
    pub document_id: String,
    pub file_id: String,
    pub kind: String,
    pub ordinal: i64,
    pub text: String,
    pub table_rows: Option<Vec<Vec<String>>>,
    pub caption: Option<String>,
}

/// A persistent, user-defined link (survives every rebuild).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManualLink {
    pub id: String,
    pub link_type: String,
    pub entity_qualified_name: String,
    pub document_rel_path: String,
    pub attachment_index: Option<i64>,
    pub chunk_ordinal: Option<i64>,
    pub note: Option<String>,
    pub created_at: String,
}

fn opt_sentinel(v: Option<i64>) -> i64 {
    v.unwrap_or(-1)
}

fn from_sentinel(v: i64) -> Option<i64> {
    (v >= 0).then_some(v)
}

impl ProjectStore {
    /// Every chunk of the build in document/ordinal order.
    pub fn link_units(&self, build_id: &str) -> Result<Vec<LinkUnitRow>> {
        self.query_rows(
            "link units",
            "SELECT id, document_id, file_id, kind, ordinal, text, table_rows, caption
             FROM chunks WHERE build_id = ?1 ORDER BY document_id, ordinal",
            &[&build_id],
            |r| {
                let rows: Option<String> = r.get(6)?;
                Ok(LinkUnitRow {
                    id: r.get(0)?,
                    document_id: r.get(1)?,
                    file_id: r.get(2)?,
                    kind: r.get(3)?,
                    ordinal: r.get(4)?,
                    text: r.get(5)?,
                    table_rows: rows.and_then(|t| serde_json::from_str(&t).ok()),
                    caption: r.get(7)?,
                })
            },
        )
    }

    /// Relationships whose unresolved target starts with `prefix`.
    pub fn relationships_with_symbol_prefix(
        &self,
        build_id: &str,
        prefix: &str,
    ) -> Result<Vec<RelationshipRow>> {
        let pattern = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
        self.query_rows(
            "relationships by prefix",
            &format!(
                "SELECT {RELATIONSHIP_COLUMNS} FROM relationships
                 WHERE build_id = ?1 AND target_symbol LIKE ?2 ESCAPE '\\' ORDER BY id"
            ),
            &[&build_id, &pattern],
            relationship_from_row,
        )
    }

    pub fn links(&self, build_id: &str) -> Result<Vec<LinkRow>> {
        self.query_rows(
            "links",
            "SELECT id, link_type, entity_id, document_id, chunk_id, resolver, confidence, evidence
             FROM cross_links WHERE build_id = ?1 ORDER BY id",
            &[&build_id],
            |r| {
                Ok(LinkRow {
                    id: r.get(0)?,
                    link_type: r.get(1)?,
                    entity_id: r.get(2)?,
                    document_id: r.get(3)?,
                    chunk_id: r.get(4)?,
                    resolver: r.get(5)?,
                    confidence: r.get(6)?,
                    evidence: r.get(7)?,
                })
            },
        )
    }

    pub fn delete_links_by_resolver(&mut self, build_id: &str, resolver: &str) -> Result<usize> {
        write_tx(&mut self.conn, |tx| {
            cached(
                tx,
                "DELETE FROM cross_links WHERE build_id = ?1 AND resolver = ?2",
                params![build_id, resolver],
            )
            .map_err(StorageError::sqlite("delete links"))
        })
    }

    /// Adds a manual link; `Ok(false)` when it already exists.
    pub fn add_manual_link(&mut self, link: &ManualLink) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO manual_links (id, link_type, entity_qualified_name,
                    document_rel_path, attachment_index, chunk_ordinal, note, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    link.id,
                    link.link_type,
                    link.entity_qualified_name,
                    link.document_rel_path,
                    opt_sentinel(link.attachment_index),
                    opt_sentinel(link.chunk_ordinal),
                    link.note,
                    link.created_at
                ],
            )
            .map_err(StorageError::sqlite("add manual link"))?;
        Ok(n > 0)
    }

    pub fn remove_manual_link(&mut self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM manual_links WHERE id = ?1", [id])
            .map_err(StorageError::sqlite("remove manual link"))?;
        Ok(n > 0)
    }

    pub fn manual_links(&self) -> Result<Vec<ManualLink>> {
        self.query_rows(
            "manual links",
            "SELECT id, link_type, entity_qualified_name, document_rel_path, attachment_index,
                chunk_ordinal, note, created_at FROM manual_links ORDER BY created_at, id",
            &[],
            |r| {
                Ok(ManualLink {
                    id: r.get(0)?,
                    link_type: r.get(1)?,
                    entity_qualified_name: r.get(2)?,
                    document_rel_path: r.get(3)?,
                    attachment_index: from_sentinel(r.get(4)?),
                    chunk_ordinal: from_sentinel(r.get(5)?),
                    note: r.get(6)?,
                    created_at: r.get(7)?,
                })
            },
        )
    }
}
