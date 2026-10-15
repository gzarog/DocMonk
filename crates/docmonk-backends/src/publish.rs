//! Publishing a staged build to the server.
//!
//! Server-mode indexing runs the backend-agnostic pipeline (scan, diff,
//! conversion, parsing, chunking, embedding) into a disposable local
//! staging store, then publishes the base index here (the relationship
//! graph is published separately, after this, by
//! [`ServerBackend::publish_graph`](crate::graph)):
//!
//! 1. Every staged file gets a *knowledge digest* over its file row, its
//!    entities, documents, chunks and vector keys.
//! 2. A new, immutable pending build is begun on the server (fenced by the
//!    writer lease).
//! 3. Files whose digest equals the one recorded in the server's active
//!    build are **copied forward server-side**: an in-place
//!    `_update_by_query` adds the new build id to their records'
//!    multi-valued `build_id` (no reconversion, no client round trip, no
//!    duplicated record).
//! 4. Every other file's records (and vectors) are bulk-written. No
//!    relationship or link is written: graph generations are independent.
//! 5. The build is published with an atomic compare-and-swap; any error
//!    aborts it and the previous build stays visible.
//!
//! When the staged build's overall digest equals the server's, nothing is
//! written and no generation is created (a warm pass).

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use docmonk_storage::knowledge::{DocumentRow, FileRow, ProjectStore};
use docmonk_storage::vectors::SUBJECT_ENTITY;

use crate::backend::{ChunkDoc, DocumentDoc, EntityDoc, FileDoc, Lease, ServerBackend};
use crate::bulk::BulkReport;
use crate::error::{BackendError, Result};
use crate::schema::IndexKind;
use crate::transport::Method;

/// Most file ids per server-side copy request.
const COPY_BATCH: usize = 5_000;

/// What to publish.
pub struct PublishInput<'a> {
    pub source_id: &'a str,
    /// Canonical source root.
    pub path: &'a str,
    pub store: &'a ProjectStore,
    /// The staged build to publish.
    pub local_build: &'a str,
    /// Index versions recorded with the build.
    pub versions: Value,
    /// Rewrite everything (no copy-forward), e.g. for a full rebuild.
    pub full: bool,
    pub lease: Option<&'a Lease>,
    /// The new server build id.
    pub build_id: &'a str,
}

/// What a publication did (the copy-forward amplification is
/// `records_copied` against `records_written`).
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PublishReport {
    pub build_id: Option<String>,
    /// `false` when the server already had exactly this content.
    pub published: bool,
    pub files: usize,
    pub files_copied: usize,
    pub files_written: usize,
    pub records_copied: u64,
    pub records_written: usize,
    pub vectors_written: usize,
    pub bulk: BulkReport,
    pub seconds: f64,
}

fn hex(h: Sha256) -> String {
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn feed(h: &mut Sha256, v: &impl Serialize) {
    h.update(serde_json::to_vec(v).unwrap_or_default());
    h.update([0]);
}

/// Everything staged for one file.
struct Staged {
    row: FileRow,
    digest: String,
}

fn embedding_fingerprint(store: &ProjectStore, build: &str) -> Result<Option<String>> {
    Ok(store
        .embedding_fingerprints(build)
        .map_err(|e| BackendError::Invalid(e.to_string()))?
        .pop())
}

fn storage(e: docmonk_storage::StorageError) -> BackendError {
    BackendError::Invalid(format!("staging store: {e}"))
}

/// Per-file digests of the staged build, plus the overall digest of the
/// base index (relationships and links are not part of it).
fn digests(
    store: &ProjectStore,
    build: &str,
    fingerprint: Option<&str>,
) -> Result<(Vec<Staged>, String)> {
    let mut files = store.files(build).map_err(storage)?;
    files.sort_by(|a, b| a.id.cmp(&b.id));
    let docs = store.documents(build).map_err(storage)?;
    let keys: HashMap<String, Vec<(String, String)>> = match fingerprint {
        Some(fp) => {
            let mut m: HashMap<String, Vec<(String, String)>> = HashMap::new();
            for (t, id) in store.embedding_keys(build, fp).map_err(storage)? {
                m.entry(t.clone()).or_default().push((t, id));
            }
            m
        }
        None => HashMap::new(),
    };
    let vec_keys: std::collections::HashSet<(String, String)> =
        keys.into_values().flatten().collect();
    let mut overall = Sha256::new();
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let mut h = Sha256::new();
        feed(&mut h, &f);
        let entities = store.file_entities(build, &f.id).map_err(storage)?;
        for e in &entities {
            feed(&mut h, e);
            feed(
                &mut h,
                &vec_keys.contains(&(SUBJECT_ENTITY.to_owned(), e.id.clone())),
            );
        }
        for d in docs.iter().filter(|d| d.file_id == f.id) {
            feed(&mut h, d);
        }
        for c in store.file_chunks(build, &f.id).map_err(storage)? {
            feed(&mut h, &c);
            feed(
                &mut h,
                &vec_keys.contains(&("chunk".to_owned(), c.id.clone())),
            );
        }
        feed(&mut h, &fingerprint);
        let digest = hex(h);
        overall.update(digest.as_bytes());
        out.push(Staged { row: f, digest });
    }
    Ok((out, hex(overall)))
}

impl ServerBackend {
    /// `file_id -> knowledge_digest` of one build (paged).
    fn build_file_digests(
        &self,
        source_id: &str,
        build_id: &str,
    ) -> Result<HashMap<String, String>> {
        let path = format!("/{}/_search", self.index(IndexKind::Files));
        let mut out = HashMap::new();
        let mut after: Option<Value> = None;
        loop {
            let mut body = json!({
                "size": 1000,
                "_source": ["file_id", "knowledge_digest"],
                "query": { "bool": { "filter": [
                    { "term": { "source_id": source_id } },
                    { "term": { "build_id": build_id } },
                ] } },
                "sort": [ { "file_id": "asc" } ],
                "track_total_hits": false,
            });
            if let Some(a) = &after {
                body["search_after"] = a.clone();
            }
            let v = self.post_json(Method::Post, &path, &body)?;
            let hits = v
                .pointer("/hits/hits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for h in &hits {
                if let (Some(f), Some(d)) = (
                    h["_source"]["file_id"].as_str(),
                    h["_source"]["knowledge_digest"].as_str(),
                ) {
                    out.insert(f.to_owned(), d.to_owned());
                }
            }
            if hits.len() < 1000 {
                return Ok(out);
            }
            after = hits.last().map(|h| h["sort"].clone());
        }
    }

    /// Copies the records of `file_ids` from build `from` into build `to`
    /// server-side and in place: each record gains `to` in its
    /// (multi-valued) `build_id` and drops ids not in `keep`. No payload
    /// leaves the cluster and no record is duplicated. Returns the number
    /// of records attached.
    pub fn copy_forward(
        &self,
        source_id: &str,
        from: &str,
        to: &str,
        file_ids: &[String],
        keep: &[String],
    ) -> Result<u64> {
        self.copy_forward_in(
            &[
                IndexKind::Files,
                IndexKind::Code,
                IndexKind::Documents,
                IndexKind::Chunks,
            ],
            source_id,
            from,
            to,
            file_ids,
            keep,
        )
    }

    /// [`Self::copy_forward`] over the records of `kinds` only.
    pub fn copy_forward_in(
        &self,
        kinds: &[IndexKind],
        source_id: &str,
        from: &str,
        to: &str,
        file_ids: &[String],
        keep: &[String],
    ) -> Result<u64> {
        let mut copied = 0;
        let names: Vec<String> = kinds.iter().map(|k| self.index(*k)).collect();
        let mut keep: Vec<String> = keep.to_vec();
        keep.push(to.to_owned());
        for batch in file_ids.chunks(COPY_BATCH) {
            let v = self.by_query(
                &format!(
                    "/{}/_update_by_query?conflicts=proceed&refresh=false",
                    names.join(",")
                ),
                &json!({
                    "query": { "bool": { "filter": [
                        { "term": { "source_id": source_id } },
                        { "term": { "build_id": from } },
                        { "terms": { "file_id": batch } },
                    ] } },
                    "script": {
                        "lang": "painless",
                        "source": crate::backend::ATTACH_SCRIPT,
                        "params": { "b": to, "keep": keep },
                    },
                }),
            )?;
            if let Some(f) = v["failures"].as_array().filter(|f| !f.is_empty()) {
                return Err(BackendError::Bulk {
                    failed: f.len(),
                    total: v["total"].as_u64().unwrap_or(0) as usize,
                    first_error: f[0].to_string().chars().take(400).collect(),
                });
            }
            copied += v["updated"].as_u64().unwrap_or(0);
        }
        Ok(copied)
    }

    /// Publishes a staged build (see the module docs).
    pub fn publish_from_store(&self, input: &PublishInput<'_>) -> Result<PublishReport> {
        let started = Instant::now();
        let mut report = PublishReport::default();
        let store = input.store;
        let build = input.local_build;
        let fingerprint = if self.vector_spec().is_some() {
            embedding_fingerprint(store, build)?
        } else {
            None
        };
        let (staged, overall) = digests(store, build, fingerprint.as_deref())?;
        report.files = staged.len();
        let state = self.source_state(input.source_id)?;
        let active = state.as_ref().and_then(|s| s.active_build_id.clone());
        let same = state
            .as_ref()
            .and_then(|s| s.field("content_digest"))
            .is_some_and(|d| d == overall);
        if active.is_some() && same && !input.full {
            report.build_id = active;
            report.seconds = started.elapsed().as_secs_f64();
            return Ok(report);
        }
        let server_digests = match (&active, input.full) {
            (Some(a), false) => self.build_file_digests(input.source_id, a)?,
            _ => HashMap::new(),
        };
        self.begin_build_with(input.lease, input.source_id, input.path, input.build_id)?;
        let outcome = self.write_build(
            input,
            &staged,
            &server_digests,
            active.as_deref(),
            fingerprint.as_deref(),
            &mut report,
        );
        let outcome = outcome.and_then(|()| {
            self.publish_build_ext(
                input.lease,
                input.source_id,
                input.build_id,
                &input.versions,
                input.full || active.is_none(),
                &[("content_digest", json!(overall))],
            )
        });
        if let Err(e) = outcome {
            let _ = self.abort_build_with(
                input.lease,
                input.source_id,
                input.build_id,
                Some(&docmonk_telemetry::redact::redact_urls_in_text(
                    &e.to_string(),
                )),
            );
            return Err(e);
        }
        report.build_id = Some(input.build_id.to_owned());
        report.published = true;
        report.seconds = started.elapsed().as_secs_f64();
        Ok(report)
    }

    fn write_build(
        &self,
        input: &PublishInput<'_>,
        staged: &[Staged],
        server_digests: &HashMap<String, String>,
        active: Option<&str>,
        fingerprint: Option<&str>,
        report: &mut PublishReport,
    ) -> Result<()> {
        let store = input.store;
        let build = input.local_build;
        let (copy, write): (Vec<&Staged>, Vec<&Staged>) = staged
            .iter()
            .partition(|s| server_digests.get(&s.row.id) == Some(&s.digest));
        if let (Some(from), false) = (active, copy.is_empty()) {
            let ids: Vec<String> = copy.iter().map(|s| s.row.id.clone()).collect();
            // Builds a reader may still be pinned to keep their ids.
            let mut keep = vec![from.to_owned()];
            if let Some(st) = self.source_state(input.source_id)? {
                keep.extend(
                    st.doc["retired_builds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r["build_id"].as_str().map(str::to_owned)),
                );
            }
            report.records_copied =
                self.copy_forward(input.source_id, from, input.build_id, &ids, &keep)?;
        }
        report.files_copied = copy.len();
        report.files_written = write.len();
        let docs = store.documents(build).map_err(storage)?;
        let docs_by_id: BTreeMap<&str, &DocumentRow> =
            docs.iter().map(|d| (d.id.as_str(), d)).collect();
        // When each failing file last failed (its newest error event).
        let error_times = store.file_error_times(build).map_err(storage)?;
        let mut w = self.build_writer(input.source_id, input.build_id);
        for s in &write {
            let f = &s.row;
            w.file(&FileDoc {
                file_id: f.id.clone(),
                rel_path: f.rel_path.clone(),
                kind: f.kind.clone(),
                size: f.size,
                mtime: f.mtime,
                content_hash: f.content_hash.clone(),
                parser_version: f.parser_version.clone(),
                chunker_version: f.chunker_version.clone(),
                converter_version: f.converter_version.clone(),
                embedding_model_id: f.embedding_model_id.clone(),
                embedding_text_version: f.embedding_text_version.clone(),
                status: Some(f.status.clone()),
                attempt_count: Some(f.attempt_count),
                last_error: f.last_error.clone(),
                last_error_at: f
                    .last_error
                    .as_ref()
                    .and_then(|_| error_times.get(&f.id))
                    .and_then(|t| crate::backend::iso_to_millis(t)),
                next_attempt_at: f.next_attempt_at.clone(),
                knowledge_digest: Some(s.digest.clone()),
            })?;
            report.records_written += 1;
            let entities = store.file_entities(build, &f.id).map_err(storage)?;
            let vectors = match fingerprint {
                Some(fp) => {
                    let mut keys: Vec<(String, String)> = entities
                        .iter()
                        .map(|e| (SUBJECT_ENTITY.to_owned(), e.id.clone()))
                        .collect();
                    let chunks = store.file_chunks(build, &f.id).map_err(storage)?;
                    keys.extend(chunks.iter().map(|c| ("chunk".to_owned(), c.id.clone())));
                    store
                        .embedding_vectors(build, fp, &keys)
                        .map_err(storage)?
                        .into_iter()
                        .map(|(t, id, v)| ((t, id), v))
                        .collect::<HashMap<_, _>>()
                }
                None => HashMap::new(),
            };
            for e in entities {
                let embedding = vectors
                    .get(&(SUBJECT_ENTITY.to_owned(), e.id.clone()))
                    .cloned();
                report.vectors_written += usize::from(embedding.is_some());
                w.entity(&EntityDoc {
                    entity_id: e.id.clone(),
                    file_id: f.id.clone(),
                    rel_path: f.rel_path.clone(),
                    kind: e.kind,
                    name: e.name,
                    qualified_name: e.qualified_name,
                    language: e.language,
                    parent_id: e.parent_id,
                    signature: e.signature,
                    start_line: e.start_line,
                    end_line: e.end_line,
                    start_col: Some(e.start_col),
                    end_col: Some(e.end_col),
                    mtime: Some(f.mtime),
                    embedding_fingerprint: embedding.as_ref().and(fingerprint.map(str::to_owned)),
                    embedding,
                })?;
                report.records_written += 1;
            }
            for d in docs.iter().filter(|d| d.file_id == f.id) {
                let parent_title = d
                    .attachment
                    .as_ref()
                    .and_then(|a| docs_by_id.get(a.parent_document_id.as_str()))
                    .and_then(|p| p.title.clone());
                w.document(&DocumentDoc {
                    document_id: d.id.clone(),
                    file_id: f.id.clone(),
                    rel_path: f.rel_path.clone(),
                    format: d.format.clone(),
                    title: d.title.clone(),
                    author: d.author.clone(),
                    page_count: d.page_count,
                    is_scanned: d.is_scanned,
                    content_hash: d.content_hash.clone(),
                    parent_document_id: d.attachment.as_ref().map(|a| a.parent_document_id.clone()),
                    attachment_name: d.attachment.as_ref().and_then(|a| a.name.clone()),
                    attachment_content_type: d
                        .attachment
                        .as_ref()
                        .and_then(|a| a.content_type.clone()),
                    attachment_index: d.attachment.as_ref().map(|a| a.index),
                    attachment_content_id: d.attachment.as_ref().and_then(|a| a.content_id.clone()),
                    parent_title,
                    mtime: Some(f.mtime),
                })?;
                report.records_written += 1;
            }
            for c in store.file_chunks(build, &f.id).map_err(storage)? {
                let d = docs_by_id.get(c.document_id.as_str());
                let att = d.and_then(|d| d.attachment.as_ref());
                let embedding = vectors.get(&("chunk".to_owned(), c.id.clone())).cloned();
                report.vectors_written += usize::from(embedding.is_some());
                w.chunk(&ChunkDoc {
                    chunk_id: c.id.clone(),
                    document_id: c.document_id.clone(),
                    parent_document_id: att.map(|a| a.parent_document_id.clone()),
                    file_id: f.id.clone(),
                    rel_path: f.rel_path.clone(),
                    fts_heading: Some(if c.kind == "heading" {
                        c.text.clone()
                    } else {
                        c.heading_path.join(" > ")
                    }),
                    kind: c.kind,
                    ordinal: c.ordinal,
                    heading_path: c.heading_path,
                    heading_level: c.heading_level,
                    title: d.and_then(|d| d.title.clone()),
                    search_text: c.search_text,
                    text: c.text,
                    embedding_text: c.embedding_text,
                    token_count: c.token_count,
                    page_start: c.page_start,
                    page_end: c.page_end,
                    table_rows: c.table_rows,
                    caption: c.caption,
                    embedding_fingerprint: embedding.as_ref().and(fingerprint.map(str::to_owned)),
                    embedding,
                    parent_ordinal: c.parent_ordinal,
                    document_format: d.map(|d| d.format.clone()),
                    attachment_name: att.and_then(|a| a.name.clone()),
                    attachment_content_type: att.and_then(|a| a.content_type.clone()),
                    attachment_index: att.map(|a| a.index),
                    parent_title: att
                        .and_then(|a| docs_by_id.get(a.parent_document_id.as_str()))
                        .and_then(|p| p.title.clone()),
                    mtime: Some(f.mtime),
                })?;
                report.records_written += 1;
            }
        }
        report.bulk = w.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_feed_is_order_sensitive_and_separated() {
        let mut a = Sha256::new();
        feed(&mut a, &"ab");
        feed(&mut a, &"c");
        let mut b = Sha256::new();
        feed(&mut b, &"a");
        feed(&mut b, &"bc");
        assert_ne!(hex(a), hex(b));
    }
}
