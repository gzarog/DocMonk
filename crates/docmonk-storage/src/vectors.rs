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

/// Something that still needs a vector under the current model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSubject {
    pub subject_type: &'static str,
    pub subject_id: String,
    pub file_id: String,
    /// The exact text handed to the model.
    pub text: String,
}

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
    /// `entity` or `document` (the hit kinds).
    pub kind: String,
    pub rel_path: String,
    /// Qualified name (entity) or document title / heading path (chunk).
    pub title: String,
    pub snippet: String,
    /// Heading path of a chunk (`A > B`), if any.
    pub section: Option<String>,
    /// Entity start line, or chunk ordinal.
    pub position: i64,
    /// Entity end line (`None` for chunks).
    pub end_line: Option<i64>,
    /// Attachment index for chunks of an email attachment.
    pub attachment_index: Option<i64>,
    /// Owning document of a chunk.
    pub document_id: Option<String>,
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
    /// Entities (`signature`, else `qualified_name`) and chunks
    /// (`embedding_text`, else `text`) of `build_id` that have no vector
    /// under `fingerprint`, in a stable order. Blank texts are skipped.
    pub fn pending_embedding_subjects(
        &self,
        build_id: &str,
        fingerprint: &str,
    ) -> Result<Vec<EmbeddingSubject>> {
        let mut out = self.query_rows(
            "pending entity embeddings",
            "SELECT e.id, e.file_id, COALESCE(NULLIF(e.signature, ''), e.qualified_name)
             FROM entities e
             LEFT JOIN embeddings v ON v.build_id = e.build_id AND v.subject_type = 'entity'
                AND v.subject_id = e.id AND v.model_fingerprint = ?2
             WHERE e.build_id = ?1 AND v.subject_id IS NULL
             ORDER BY e.file_id, e.id",
            &[&build_id, &fingerprint],
            |r| {
                Ok(EmbeddingSubject {
                    subject_type: SUBJECT_ENTITY,
                    subject_id: r.get(0)?,
                    file_id: r.get(1)?,
                    text: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                })
            },
        )?;
        out.extend(self.query_rows(
            "pending chunk embeddings",
            "SELECT c.id, c.file_id, COALESCE(NULLIF(c.embedding_text, ''), c.text)
             FROM chunks c
             LEFT JOIN embeddings v ON v.build_id = c.build_id AND v.subject_type = 'chunk'
                AND v.subject_id = c.id AND v.model_fingerprint = ?2
             WHERE c.build_id = ?1 AND v.subject_id IS NULL
             ORDER BY c.file_id, c.ordinal, c.id",
            &[&build_id, &fingerprint],
            |r| {
                Ok(EmbeddingSubject {
                    subject_type: SUBJECT_CHUNK,
                    subject_id: r.get(0)?,
                    file_id: r.get(1)?,
                    text: r.get(2)?,
                })
            },
        )?);
        out.retain(|s| !s.text.trim().is_empty());
        Ok(out)
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
            cached(
                tx,
                "DELETE FROM embeddings WHERE build_id = ?1 AND model_fingerprint != ?2",
                params![build_id, fingerprint],
            )
            .map_err(StorageError::sqlite("delete stale embeddings"))
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
            Ok(())
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
