//! Embedding vectors and the cross-build embedding cache.
//!
//! Vectors are build-scoped rows keyed by subject (`entity` or `chunk`) and
//! stamped with the model fingerprint. Anything in a build that lacks a
//! vector under the current fingerprint is "pending"; that single rule
//! covers touched files (their rows were replaced), crash recovery (a
//! missing vector is simply pending again) and model changes (every row
//! carries the old fingerprint, so everything is re-embedded explicitly).

use std::collections::HashMap;

use rusqlite::params;

use crate::db::{now_iso, write_tx};
use crate::error::{Result, StorageError};
use crate::knowledge::{cached, ProjectStore};

/// Subject kinds stored in `embeddings.subject_type`.
pub const SUBJECT_ENTITY: &str = "entity";
pub const SUBJECT_CHUNK: &str = "chunk";
/// An implementation passage of a code file (ADR 0037).
pub const SUBJECT_PASSAGE: &str = "passage";

/// Something that still needs a vector under the current model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSubject {
    pub subject_type: &'static str,
    pub subject_id: String,
    pub file_id: String,
    /// The exact text handed to the model.
    pub text: String,
}

/// Where a paged read of pending embedding subjects resumes: the subject
/// kind (index into the read order: entities, chunks, passages) and the
/// last row's order key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubjectCursor {
    pub kind: usize,
    pub file_id: String,
    pub ordinal: i64,
    pub id: String,
}

/// Pending-subject reads per kind, in order. Each selects `(id, file_id,
/// ordinal, text)` after the keyset position `(?3, ?4, ?5)`, at most `?6`
/// rows; entities have no ordinal (0).
const PENDING_KINDS: [(&str, &str); 3] = [
    (
        SUBJECT_ENTITY,
        "SELECT e.id, e.file_id, 0, COALESCE(NULLIF(e.signature, ''), e.qualified_name)
         FROM entities e
         LEFT JOIN embeddings v ON v.build_id = e.build_id AND v.subject_type = 'entity'
            AND v.subject_id = e.id AND v.model_fingerprint = ?2
         WHERE e.build_id = ?1 AND v.subject_id IS NULL
           AND (e.file_id, 0, e.id) > (?3, ?4, ?5)
         ORDER BY e.file_id, e.id LIMIT ?6",
    ),
    (
        SUBJECT_CHUNK,
        "SELECT c.id, c.file_id, c.ordinal, COALESCE(NULLIF(c.embedding_text, ''), c.text)
         FROM chunks c
         LEFT JOIN embeddings v ON v.build_id = c.build_id AND v.subject_type = 'chunk'
            AND v.subject_id = c.id AND v.model_fingerprint = ?2
         WHERE c.build_id = ?1 AND v.subject_id IS NULL
           AND (c.file_id, c.ordinal, c.id) > (?3, ?4, ?5)
         ORDER BY c.file_id, c.ordinal, c.id LIMIT ?6",
    ),
    (
        SUBJECT_PASSAGE,
        "SELECT p.id, p.file_id, p.ordinal, p.header || char(10) || p.text
         FROM code_passages p
         LEFT JOIN embeddings v ON v.build_id = p.build_id AND v.subject_type = 'passage'
            AND v.subject_id = p.id AND v.model_fingerprint = ?2
         WHERE p.build_id = ?1 AND v.subject_id IS NULL
           AND (p.file_id, p.ordinal, p.id) > (?3, ?4, ?5)
         ORDER BY p.file_id, p.ordinal, p.id LIMIT ?6",
    ),
];

/// A computed vector ready to be written.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingRow {
    pub subject_type: &'static str,
    pub subject_id: String,
    pub file_id: String,
    pub text_hash: String,
    pub text_version: String,
    pub vector: Vec<f32>,
}

/// A stored vector as read back for search.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEmbedding {
    pub subject_type: String,
    pub subject_id: String,
    pub file_id: String,
    pub model_fingerprint: String,
    pub vector: Vec<f32>,
}

/// Characters of chunk text shown as a hit snippet.
pub const SNIPPET_CHARS: usize = 280;

/// Display metadata of one semantic hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectMeta {
    pub subject_type: String,
    pub subject_id: String,
    /// `entity`, `document` or `passage` (the hit kinds). A body passage
    /// is reported as a hit on its owning entity.
    pub kind: String,
    /// The record the hit names: the subject itself, or the owning entity
    /// of a body passage.
    pub record_id: String,
    pub rel_path: String,
    /// Qualified name (entity) or document title / heading path (chunk).
    pub title: String,
    pub snippet: String,
    /// Heading path of a chunk (`A > B`), if any.
    pub section: Option<String>,
    /// Entity or passage start line, or chunk ordinal.
    pub position: i64,
    /// Entity or passage end line (`None` for chunks).
    pub end_line: Option<i64>,
    /// Attachment index for chunks of an email attachment.
    pub attachment_index: Option<i64>,
    /// Owning document of a chunk.
    pub document_id: Option<String>,
}

/// `metadata` key prefix of a build's embeddings revision token.
pub const EMBEDDINGS_REVISION_KEY: &str = "embeddings_revision:";

/// Replaces `build_id`'s embeddings revision token with a fresh random
/// one. Every statement that inserts or deletes embedding rows calls this
/// in the same transaction, so a derived index stamped with a token is
/// current exactly while the token is unchanged. Tokens are random rather
/// than counters, so a rolled-back write can never make a stale index
/// match again.
pub(crate) fn bump_embeddings_revision(tx: &rusqlite::Connection, build_id: &str) -> Result<()> {
    cached(
        tx,
        "INSERT INTO metadata (key, value) VALUES (?1, lower(hex(randomblob(12))))
         ON CONFLICT(key) DO UPDATE SET value = lower(hex(randomblob(12)))",
        params![format!("{EMBEDDINGS_REVISION_KEY}{build_id}")],
    )
    .map_err(StorageError::sqlite("bump embeddings revision"))?;
    Ok(())
}

/// Little-endian f32 encoding used for every vector blob.
pub fn encode_vector(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl ProjectStore {
    /// Entities (`signature`, else `qualified_name`), chunks
    /// (`embedding_text`, else `text`) and passages (header and lines) of
    /// `build_id` that have no vector under `fingerprint`, in a stable
    /// order. Blank texts are skipped. Reads every page of
    /// [`Self::pending_embedding_subjects_page`]; the embedding pass itself
    /// works page by page.
    pub fn pending_embedding_subjects(
        &self,
        build_id: &str,
        fingerprint: &str,
    ) -> Result<Vec<EmbeddingSubject>> {
        let mut out = Vec::new();
        let mut cursor: Option<SubjectCursor> = None;
        loop {
            let (page, next) =
                self.pending_embedding_subjects_page(build_id, fingerprint, cursor.as_ref(), 1024)?;
            out.extend(page);
            match next {
                Some(c) => cursor = Some(c),
                None => return Ok(out),
            }
        }
    }

    /// One page of [`Self::pending_embedding_subjects`] (ADR 0041): at
    /// most `limit` rows after `after`, in the same order, and where the
    /// next page starts (`None` after the last). The cursor is a keyset
    /// position (kind, file, ordinal, id), never an offset: memory is
    /// bounded by `limit`, and vectors written between pages cannot shift
    /// the rows a later page reads. Blank texts are skipped but advance
    /// the cursor.
    pub fn pending_embedding_subjects_page(
        &self,
        build_id: &str,
        fingerprint: &str,
        after: Option<&SubjectCursor>,
        limit: usize,
    ) -> Result<(Vec<EmbeddingSubject>, Option<SubjectCursor>)> {
        let limit = limit.max(1);
        let mut out = Vec::new();
        let start = after.map_or(0, |c| c.kind);
        for (kind, &(subject_type, sql)) in PENDING_KINDS.iter().enumerate().skip(start) {
            let (file, ordinal, id) = match after {
                Some(c) if c.kind == kind => (c.file_id.as_str(), c.ordinal, c.id.as_str()),
                // Before every row of this kind.
                _ => ("", i64::MIN, ""),
            };
            let want = (limit - out.len()) as i64;
            let rows = self.query_rows(
                "pending embeddings",
                sql,
                &[&build_id, &fingerprint, &file, &ordinal, &id, &want],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    ))
                },
            )?;
            let full = rows.len() as i64 == want;
            let last = rows.last().map(|(id, file, ord, _)| SubjectCursor {
                kind,
                file_id: file.clone(),
                ordinal: *ord,
                id: id.clone(),
            });
            for (subject_id, file_id, _, text) in rows {
                if !text.trim().is_empty() {
                    out.push(EmbeddingSubject {
                        subject_type,
                        subject_id,
                        file_id,
                        text,
                    });
                }
            }
            if full {
                return Ok((out, last));
            }
        }
        Ok((out, None))
    }

    /// Number of vectors in `build_id` stamped with a different model.
    pub fn stale_embedding_count(&self, build_id: &str, fingerprint: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE build_id = ?1 AND model_fingerprint != ?2",
                params![build_id, fingerprint],
                |r| r.get(0),
            )
            .map_err(StorageError::sqlite("stale embeddings"))
    }

    /// Drops vectors of `build_id` that were produced by another model.
    pub fn delete_stale_embeddings(&mut self, build_id: &str, fingerprint: &str) -> Result<usize> {
        write_tx(&mut self.conn, |tx| {
            let n = cached(
                tx,
                "DELETE FROM embeddings WHERE build_id = ?1 AND model_fingerprint != ?2",
                params![build_id, fingerprint],
            )
            .map_err(StorageError::sqlite("delete stale embeddings"))?;
            bump_embeddings_revision(tx, build_id)?;
            Ok(n)
        })
    }

    /// Cache lookup by exact `(text_hash, text_version)` under `fingerprint`.
    pub fn cached_embeddings(
        &self,
        fingerprint: &str,
        keys: &[(String, String)],
    ) -> Result<HashMap<(String, String), Vec<f32>>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT vector FROM embedding_cache
                 WHERE text_hash = ?1 AND model_fingerprint = ?2 AND embedding_text_version = ?3",
            )
            .map_err(StorageError::sqlite("embedding cache"))?;
        let mut out = HashMap::new();
        for (hash, version) in keys {
            let blob: Option<Vec<u8>> = stmt
                .query_row(params![hash, fingerprint, version], |r| r.get(0))
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })
                .map_err(StorageError::sqlite("embedding cache"))?;
            if let Some(blob) = blob {
                out.insert((hash.clone(), version.clone()), decode_vector(&blob));
            }
        }
        Ok(out)
    }

    /// Writes vectors for `build_id` and upserts them into the cache, in one
    /// transaction.
    pub fn put_embeddings(
        &mut self,
        build_id: &str,
        fingerprint: &str,
        rows: &[EmbeddingRow],
    ) -> Result<()> {
        let now = now_iso();
        write_tx(&mut self.conn, |tx| {
            for row in rows {
                let blob = encode_vector(&row.vector);
                let dims = row.vector.len() as i64;
                cached(
                    tx,
                    "INSERT OR REPLACE INTO embeddings (build_id, subject_type, subject_id, file_id,
                        model_fingerprint, text_hash, dims, vector)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        build_id,
                        row.subject_type,
                        row.subject_id,
                        row.file_id,
                        fingerprint,
                        row.text_hash,
                        dims,
                        blob
                    ],
                )
                .map_err(StorageError::sqlite("put embedding"))?;
                cached(
                    tx,
                    "INSERT OR IGNORE INTO embedding_cache (text_hash, model_fingerprint,
                        embedding_text_version, dims, vector, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        row.text_hash,
                        fingerprint,
                        row.text_version,
                        dims,
                        blob,
                        now
                    ],
                )
                .map_err(StorageError::sqlite("put embedding cache"))?;
            }
            bump_embeddings_revision(tx, build_id)
        })
    }

    /// Every vector of `build_id` (ordered by subject).
    pub fn embeddings(&self, build_id: &str) -> Result<Vec<StoredEmbedding>> {
        self.query_rows(
            "embeddings",
            "SELECT subject_type, subject_id, file_id, model_fingerprint, vector
             FROM embeddings WHERE build_id = ?1 ORDER BY subject_type, subject_id",
            &[&build_id],
            |r| {
                Ok(StoredEmbedding {
                    subject_type: r.get(0)?,
                    subject_id: r.get(1)?,
                    file_id: r.get(2)?,
                    model_fingerprint: r.get(3)?,
                    vector: decode_vector(&r.get::<_, Vec<u8>>(4)?),
                })
            },
        )
    }

    /// Distinct model fingerprints with vectors in `build_id`.
    pub fn embedding_fingerprints(&self, build_id: &str) -> Result<Vec<String>> {
        self.query_rows(
            "embedding fingerprints",
            "SELECT DISTINCT model_fingerprint FROM embeddings WHERE build_id = ?1
             ORDER BY model_fingerprint",
            &[&build_id],
            |r| r.get(0),
        )
    }

    /// Rows in the persistent embedding cache.
    pub fn embedding_cache_len(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM embedding_cache", [], |r| r.get(0))
            .map_err(StorageError::sqlite("embedding cache size"))
    }

    /// `(subject_type, subject_id)` of every vector of `build_id` under
    /// `fingerprint`.
    pub fn embedding_keys(
        &self,
        build_id: &str,
        fingerprint: &str,
    ) -> Result<Vec<(String, String)>> {
        self.query_rows(
            "embedding keys",
            "SELECT subject_type, subject_id FROM embeddings
             WHERE build_id = ?1 AND model_fingerprint = ?2 ORDER BY subject_type, subject_id",
            &[&build_id, &fingerprint],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// Vectors for the given keys (missing keys are skipped).
    pub fn embedding_vectors(
        &self,
        build_id: &str,
        fingerprint: &str,
        keys: &[(String, String)],
    ) -> Result<Vec<(String, String, Vec<f32>)>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT vector FROM embeddings WHERE build_id = ?1 AND model_fingerprint = ?2
                 AND subject_type = ?3 AND subject_id = ?4",
            )
            .map_err(StorageError::sqlite("embedding vectors"))?;
        let mut out = Vec::with_capacity(keys.len());
        for (t, id) in keys {
            let blob: Option<Vec<u8>> = stmt
                .query_row(params![build_id, fingerprint, t, id], |r| r.get(0))
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })
                .map_err(StorageError::sqlite("embedding vectors"))?;
            if let Some(blob) = blob {
                out.push((t.clone(), id.clone(), decode_vector(&blob)));
            }
        }
        Ok(out)
    }

    /// `build_id`'s current embeddings revision token (`""` before any
    /// vector was written or removed). One indexed point read.
    pub fn embeddings_revision(&self, build_id: &str) -> Result<String> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [format!("{EMBEDDINGS_REVISION_KEY}{build_id}")],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::sqlite("embeddings revision"))?
            .unwrap_or_default())
    }

    /// sha256 over the ordered `(subject_type, subject_id, text_hash)` set
    /// of `build_id` under `fingerprint` (hex). Identifies exactly which
    /// vectors a derived index must contain.
    pub fn embedding_digest(&self, build_id: &str, fingerprint: &str) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT subject_type, subject_id, text_hash FROM embeddings
                 WHERE build_id = ?1 AND model_fingerprint = ?2
                 ORDER BY subject_type, subject_id",
            )
            .map_err(StorageError::sqlite("embedding digest"))?;
        let mut rows = stmt
            .query(params![build_id, fingerprint])
            .map_err(StorageError::sqlite("embedding digest"))?;
        let mut h = Sha256::new();
        while let Some(r) = rows
            .next()
            .map_err(StorageError::sqlite("embedding digest"))?
        {
            for i in 0..3 {
                let v: String = r.get(i).map_err(StorageError::sqlite("embedding digest"))?;
                h.update(v.as_bytes());
                h.update([0]);
            }
        }
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Display metadata for `(subject_type, subject_id)` keys of `build_id`
    /// (unknown keys are skipped; order follows `keys`).
    pub fn subject_meta(
        &self,
        build_id: &str,
        keys: &[(String, String)],
    ) -> Result<Vec<SubjectMeta>> {
        let mut entity = self
            .conn
            .prepare_cached(
                "SELECT f.rel_path, e.qualified_name, COALESCE(e.signature, e.qualified_name),
                    e.start_line, e.end_line
                 FROM entities e JOIN files f ON f.build_id = e.build_id AND f.id = e.file_id
                 WHERE e.build_id = ?1 AND e.id = ?2",
            )
            .map_err(StorageError::sqlite("subject meta"))?;
        let mut out = Vec::with_capacity(keys.len());
        for (t, id) in keys {
            if t == SUBJECT_ENTITY {
                let row = entity
                    .query_row(params![build_id, id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                        ))
                    })
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        e => Err(e),
                    })
                    .map_err(StorageError::sqlite("subject meta"))?;
                if let Some((path, qn, sig, line, end_line)) = row {
                    out.push(SubjectMeta {
                        subject_type: t.clone(),
                        subject_id: id.clone(),
                        kind: "entity".into(),
                        record_id: id.clone(),
                        rel_path: path,
                        title: qn,
                        snippet: sig,
                        section: None,
                        position: line.unwrap_or(0),
                        end_line,
                        attachment_index: None,
                        document_id: None,
                    });
                }
                continue;
            }
            if t == SUBJECT_PASSAGE {
                let row = self
                    .conn
                    .prepare_cached(
                        "SELECT f.rel_path, p.entity_id, e.qualified_name, p.text, p.start_line,
                            p.end_line
                         FROM code_passages p
                         JOIN files f ON f.build_id = p.build_id AND f.id = p.file_id
                         LEFT JOIN entities e ON e.build_id = p.build_id AND e.id = p.entity_id
                         WHERE p.build_id = ?1 AND p.id = ?2",
                    )
                    .and_then(|mut st| {
                        st.query_row(params![build_id, id], |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, i64>(4)?,
                                r.get::<_, i64>(5)?,
                            ))
                        })
                    })
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        e => Err(e),
                    })
                    .map_err(StorageError::sqlite("subject meta"))?;
                if let Some((path, owner, qn, text, start, end)) = row {
                    out.push(passage_meta(t, id, path, owner, qn, &text, start, end));
                }
                continue;
            }
            let row = self
                .conn
                .prepare_cached(
                    "SELECT f.rel_path, COALESCE(d.title, ''), c.heading_path, c.text, c.ordinal,
                        d.attachment_index, c.kind, c.table_rows, d.id
                     FROM chunks c
                     JOIN documents d ON d.build_id = c.build_id AND d.id = c.document_id
                     JOIN files f ON f.build_id = c.build_id AND f.id = c.file_id
                     WHERE c.build_id = ?1 AND c.id = ?2",
                )
                .and_then(|mut st| {
                    st.query_row(params![build_id, id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, Option<String>>(7)?,
                            r.get::<_, String>(8)?,
                        ))
                    })
                })
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })
                .map_err(StorageError::sqlite("subject meta"))?;
            if let Some((path, title, headings, text, ordinal, attachment_index, kind, rows, doc)) =
                row
            {
                // Display contract for vector hits:
                // title = document title (else path), snippet = the first 280
                // characters of the text (a table's cells joined by spaces).
                let headings: Vec<String> = serde_json::from_str(&headings).unwrap_or_default();
                let text = match (kind.as_str(), rows) {
                    ("table", Some(rows)) => serde_json::from_str::<Vec<Vec<String>>>(&rows)
                        .map(|rows| {
                            rows.iter()
                                .flatten()
                                .filter(|c| !c.is_empty())
                                .map(String::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or(text),
                    _ => text,
                };
                out.push(SubjectMeta {
                    subject_type: t.clone(),
                    subject_id: id.clone(),
                    kind: "document".into(),
                    record_id: id.clone(),
                    title: if title.is_empty() {
                        path.clone()
                    } else {
                        title
                    },
                    rel_path: path,
                    snippet: text.chars().take(SNIPPET_CHARS).collect(),
                    section: (!headings.is_empty()).then(|| headings.join(" > ")),
                    position: ordinal,
                    end_line: None,
                    attachment_index,
                    document_id: Some(doc),
                });
            }
        }
        Ok(out)
    }
}

/// Display metadata of a passage vector hit: a body passage is a hit on
/// its owning entity located at the passage's lines; a text passage is a
/// `passage` hit titled by its path. Shared by every backend.
#[allow(clippy::too_many_arguments)]
pub fn passage_meta(
    subject_type: &str,
    subject_id: &str,
    rel_path: String,
    owner: Option<String>,
    owner_name: Option<String>,
    text: &str,
    start_line: i64,
    end_line: i64,
) -> SubjectMeta {
    let (kind, record_id, title) = match owner {
        Some(e) => ("entity", e, owner_name.unwrap_or_else(|| rel_path.clone())),
        None => ("passage", subject_id.to_owned(), rel_path.clone()),
    };
    SubjectMeta {
        subject_type: subject_type.to_owned(),
        subject_id: subject_id.to_owned(),
        kind: kind.into(),
        record_id,
        rel_path,
        title,
        snippet: text.chars().take(SNIPPET_CHARS).collect(),
        section: None,
        position: start_line,
        end_line: Some(end_line),
        attachment_index: None,
        document_id: None,
    }
}
