//! [`KnowledgeRead`] over the server indexes.
//!
//! A [`ServerReader`] is one source's view of one published build. Every
//! query is filtered to `(source_id, build_id)` (never to "all builds"),
//! so records of other sources, pending builds and garbage-collected
//! builds can never appear. Ordering mirrors the local store's: equal
//! scores fall back to path, qualified name, line and id, never to
//! engine insertion order.
//!
//! Lexical scores are the engine's own BM25; they are reported as
//! `bm25 = -score` (lower is better, like SQLite FTS5) and are not claimed
//! to equal SQLite's values.

use std::sync::Arc;

use serde_json::{json, Value};

use docmonk_storage::error::{Result, StorageError};
use docmonk_storage::knowledge::{ChunkRow, EntityRow, PassageRow, RelationshipRow};
use docmonk_storage::read::{
    parse_expression, FtsTerm, KnowledgeRead, ReadFilter, VectorFilter, VectorHit,
};
use docmonk_storage::search::{
    AttachmentProvenance, DocumentSearchRow, EntityLinkRow, EntitySearchRow, PassageOwner,
    PassageSearchRow, PathSearchRow,
};
use docmonk_storage::vectors::{
    passage_meta, SubjectMeta, SNIPPET_CHARS, SUBJECT_CHUNK, SUBJECT_ENTITY, SUBJECT_PASSAGE,
};

use crate::backend::{ServerBackend, MANUAL_BUILD, MANUAL_LINK};
use crate::engine::Engine;
use crate::schema::IndexKind;
use crate::transport::Method;

/// Largest result window a single read asks for.
pub const MAX_READ: usize = 10_000;

/// One source's published build on the server.
pub struct ServerReader {
    backend: Arc<ServerBackend>,
    source_id: String,
    /// `base build -> (served graph generation, stale)`, resolved once per
    /// reader (one request's snapshot).
    graph: std::sync::Mutex<std::collections::HashMap<String, Option<(String, bool)>>>,
    /// `build or graph generation id -> visibility filter`, resolved once
    /// per reader (one request's snapshot).
    scopes: std::sync::Mutex<std::collections::HashMap<String, Value>>,
    /// `graph.require_current`.
    require_current: bool,
    /// While recording ([`ServerReader::prefetch`]), searches are collected
    /// here instead of sent.
    recording: std::sync::Mutex<Option<Vec<(IndexKind, Value)>>>,
    /// Hits of searches answered by a prefetching `_msearch`, keyed by
    /// index and exact request body; each is served once.
    prefetched: std::sync::Mutex<std::collections::HashMap<(String, String), Vec<Value>>>,
}

impl ServerReader {
    pub fn new(backend: Arc<ServerBackend>, source_id: &str) -> Self {
        Self {
            backend,
            source_id: source_id.to_owned(),
            graph: Default::default(),
            scopes: Default::default(),
            require_current: false,
            recording: Default::default(),
            prefetched: Default::default(),
        }
    }

    /// `graph.require_current`: serve only a graph derived from the
    /// current base (never the last one, flagged stale).
    pub fn require_current_graph(mut self, strict: bool) -> Self {
        self.require_current = strict;
        self
    }

    /// The graph generation graph queries of base build `b` read (see
    /// [`ServerGraph::served_generation`](crate::graph::ServerGraph::served_generation));
    /// `None` (graph queries come back empty) for a missing or disabled
    /// graph, and in strict mode for anything but the current one.
    pub fn graph_generation(&self, b: &str) -> Result<Option<String>> {
        Ok(self.served_graph(b)?.map(|(g, _)| g))
    }

    fn served_graph(&self, b: &str) -> Result<Option<(String, bool)>> {
        let mut cache = self.graph.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(g) = cache.get(b) {
            return Ok(g.clone());
        }
        let g = self
            .backend
            .served_graph(&self.source_id, b, self.require_current)
            .map_err(Self::err)?;
        cache.insert(b.to_owned(), g.clone());
        Ok(g)
    }

    /// Relative paths of build `b`'s files ending in `suffix`, sorted
    /// (the authoritative copy of a staging `file_paths_with_suffix`).
    pub fn file_paths_with_suffix(&self, b: &str, suffix: &str) -> Result<Vec<String>> {
        let pattern = format!("*{}", wildcard_literal(suffix));
        let mut out: Vec<String> = self
            .scan_all(
                IndexKind::Files,
                b,
                vec![json!({ "wildcard": { "rel_path": pattern } })],
                "file_id",
            )?
            .iter()
            .map(|s| str_of(s, "rel_path"))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    pub fn backend(&self) -> &ServerBackend {
        &self.backend
    }

    fn err(e: crate::BackendError) -> StorageError {
        StorageError::Backend(docmonk_telemetry::redact::redact_urls_in_text(
            &e.to_string(),
        ))
    }

    /// The records build (or graph generation) `id` sees: its
    /// generation's visibility filter ([`crate::generations`]).
    fn scope(&self, id: &str) -> Result<Vec<Value>> {
        let mut cache = self.scopes.lock().unwrap_or_else(|p| p.into_inner());
        let scope = match cache.get(id) {
            Some(s) => s.clone(),
            None => {
                let s = self
                    .backend
                    .scope_of(&self.source_id, id)
                    .map_err(Self::err)?;
                cache.insert(id.to_owned(), s.clone());
                s
            }
        };
        Ok(vec![
            json!({ "term": { "source_id": self.source_id } }),
            scope,
        ])
    }

    /// Runs `calls` against this reader in recording mode, then answers
    /// every search they would have sent with one `_msearch`; the calls
    /// made afterwards with the same arguments are served from it. Results
    /// are identical to sending each search alone (same bodies), so this
    /// only changes the number of round-trips.
    /// Request counters of the backend this reader talks to (shared by
    /// every reader of the process).
    pub fn backend_stats(&self) -> crate::transport::RequestStats {
        self.backend.stats()
    }

    pub fn prefetch(&self, calls: impl FnOnce(&Self)) -> Result<()> {
        const MSEARCH_BATCH: usize = 32;
        *self.recording.lock().unwrap_or_else(|p| p.into_inner()) = Some(Vec::new());
        calls(self);
        let recorded = self
            .recording
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_default();
        let mut uniq: Vec<(String, Value)> = Vec::new();
        let mut keys = std::collections::HashSet::new();
        for (kind, body) in recorded {
            let index = self.backend.index(kind);
            if keys.insert((index.clone(), body.to_string())) {
                uniq.push((index, body));
            }
        }
        if uniq.is_empty() {
            return Ok(());
        }
        // Bounded `_msearch` payloads (P06): at most MSEARCH_BATCH items
        // per request.
        let mut responses = Vec::with_capacity(uniq.len());
        for batch in uniq.chunks(MSEARCH_BATCH) {
            responses.extend(self.backend.msearch(batch).map_err(Self::err)?);
        }
        let mut cache = self.prefetched.lock().unwrap_or_else(|p| p.into_inner());
        for ((index, body), r) in uniq.into_iter().zip(responses) {
            // A failed search is not cached: sent alone later, it reports
            // its own error.
            if r.get("error").is_some() {
                continue;
            }
            let hits = r
                .pointer("/hits/hits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            cache.insert((index, body.to_string()), hits);
        }
        Ok(())
    }

    /// Raw hits of a `_search` against one index.
    fn hits(&self, kind: IndexKind, body: Value) -> Result<Vec<Value>> {
        if let Some(rec) = self
            .recording
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            rec.push((kind, body));
            return Ok(Vec::new());
        }
        let key = (self.backend.index(kind), body.to_string());
        if let Some(hits) = self
            .prefetched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key)
        {
            return Ok(hits);
        }
        let path = format!("/{}/_search", self.backend.index(kind));
        let v = self
            .backend
            .post_json(Method::Post, &path, &body)
            .map_err(Self::err)?;
        Ok(v.pointer("/hits/hits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// `filter` + `must` over one build, sorted, at most `size`.
    fn find(
        &self,
        kind: IndexKind,
        build_id: &str,
        mut filter: Vec<Value>,
        must: Option<Value>,
        sort: Value,
        size: usize,
    ) -> Result<Vec<Value>> {
        let mut f = self.scope(build_id)?;
        f.append(&mut filter);
        let mut query = json!({ "bool": { "filter": f } });
        if let Some(m) = must {
            query["bool"]["must"] = json!([m]);
        }
        self.hits(
            kind,
            json!({ "size": size.min(MAX_READ), "query": query, "sort": sort }),
        )
    }

    fn file_mtimes(&self, build_id: &str, ids: &[&str]) -> Result<Vec<(String, String, f64)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self.find(
            IndexKind::Files,
            build_id,
            vec![json!({ "terms": { "file_id": ids } })],
            None,
            json!([{ "file_id": "asc" }]),
            ids.len(),
        )?;
        Ok(hits
            .iter()
            .map(|h| {
                let s = &h["_source"];
                (
                    str_of(s, "file_id"),
                    str_of(s, "rel_path"),
                    s["mtime"].as_f64().unwrap_or(0.0),
                )
            })
            .collect())
    }

    fn entity_search_rows(&self, hits: &[Value]) -> Vec<EntitySearchRow> {
        hits.iter()
            .map(|h| {
                let s = &h["_source"];
                EntitySearchRow {
                    id: str_of(s, "entity_id"),
                    name: str_of(s, "name"),
                    qualified_name: str_of(s, "qualified_name"),
                    kind: str_of(s, "kind"),
                    signature: opt_str(s, "signature"),
                    start_line: s["start_line"].as_i64().unwrap_or(0),
                    end_line: s["end_line"].as_i64().unwrap_or(0),
                    rel_path: str_of(s, "rel_path"),
                    mtime: s["mtime"].as_f64().unwrap_or(0.0),
                    fts_rank: 0,
                    bm25: None,
                }
            })
            .collect()
    }

    fn attachment_of(s: &Value) -> Option<AttachmentProvenance> {
        let parent = opt_str(s, "parent_document_id")?;
        Some(AttachmentProvenance {
            name: opt_str(s, "attachment_name"),
            content_type: opt_str(s, "attachment_content_type"),
            index: s["attachment_index"].as_i64(),
            format: opt_str(s, "document_format")
                .or_else(|| opt_str(s, "format"))
                .unwrap_or_default(),
            parent_document_id: parent,
            parent_title: opt_str(s, "parent_title"),
        })
    }

    fn fts_query(expression: &str, fields: &[&str]) -> Option<Value> {
        let parsed = parse_expression(expression)?;
        let clauses: Vec<Value> = parsed
            .terms
            .iter()
            .map(|t| match t {
                FtsTerm::Phrase(p) => json!({ "multi_match": {
                    "query": p, "type": "phrase", "fields": fields } }),
                FtsTerm::Prefix(p) => json!({ "multi_match": {
                    "query": p, "type": "phrase_prefix", "fields": fields } }),
            })
            .collect();
        Some(if parsed.all {
            json!({ "bool": { "must": clauses } })
        } else {
            json!({ "bool": { "should": clauses, "minimum_should_match": parsed.min_should } })
        })
    }

    fn edges(
        &self,
        build_id: &str,
        filter: Vec<Value>,
        rtype: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        let mut filter = filter;
        filter.push(json!({ "term": { "record_kind": "edge" } }));
        if let Some(t) = rtype {
            filter.push(json!({ "term": { "relationship_type": t } }));
        }
        let sort = if rtype.is_some() {
            json!([{ "relationship_id": "asc" }])
        } else {
            json!([{ "relationship_type": "asc" }, { "relationship_id": "asc" }])
        };
        let Some(graph) = self.graph_generation(build_id)? else {
            return Ok(Vec::new());
        };
        let hits = self.find(
            IndexKind::Relationships,
            &graph,
            filter,
            None,
            sort,
            limit.max(0) as usize,
        )?;
        Ok(hits
            .iter()
            .map(|h| relationship_row(&h["_source"]))
            .collect())
    }

    /// Edges whose `field` is one of `keys`, in one search (falling back
    /// to one search per key when the result window would be exceeded),
    /// capped per key and type exactly like [`Self::edges`].
    #[allow(clippy::too_many_arguments)]
    fn edges_many(
        &self,
        build_id: &str,
        field: &str,
        keys: &[String],
        types: &[&str],
        extra: Vec<Value>,
        limit: i64,
        key_of: impl Fn(&RelationshipRow) -> String,
    ) -> Result<std::collections::HashMap<String, Vec<RelationshipRow>>> {
        let mut out: std::collections::HashMap<String, Vec<RelationshipRow>> = Default::default();
        let mut uniq: Vec<&str> = keys.iter().map(String::as_str).collect();
        uniq.sort_unstable();
        uniq.dedup();
        if uniq.is_empty() {
            return Ok(out);
        }
        let Some(graph) = self.graph_generation(build_id)? else {
            return Ok(out);
        };
        let mut filter = extra.clone();
        filter.push(json!({ "terms": { field: uniq } }));
        filter.push(json!({ "term": { "record_kind": "edge" } }));
        if !types.is_empty() {
            filter.push(json!({ "terms": { "relationship_type": types } }));
        }
        let hits = self.find(
            IndexKind::Relationships,
            &graph,
            filter,
            None,
            json!([{ "relationship_type": "asc" }, { "relationship_id": "asc" }]),
            MAX_READ,
        )?;
        if hits.len() >= MAX_READ {
            // Too many to cap reliably in one window: per key and type.
            for k in uniq {
                let mut f = extra.clone();
                f.push(json!({ "term": { field: k } }));
                let mut rows = Vec::new();
                if types.is_empty() {
                    rows = self.edges(build_id, f, None, limit)?;
                } else {
                    for t in types {
                        rows.extend(self.edges(build_id, f.clone(), Some(t), limit)?);
                    }
                }
                if !rows.is_empty() {
                    out.insert(k.to_owned(), rows);
                }
            }
            return Ok(out);
        }
        let cap = limit.max(0) as usize;
        let mut taken: std::collections::HashMap<(String, String), usize> = Default::default();
        for h in &hits {
            let r = relationship_row(&h["_source"]);
            let key = key_of(&r);
            // Per key and type with types; per key overall without.
            let bucket = if types.is_empty() {
                String::new()
            } else {
                r.relationship_type.clone()
            };
            let n = taken.entry((key.clone(), bucket)).or_default();
            if *n < cap {
                *n += 1;
                out.entry(key).or_default().push(r);
            }
        }
        Ok(out)
    }

    fn chunks_where(
        &self,
        build_id: &str,
        filter: Vec<Value>,
        sort: Value,
        size: usize,
    ) -> Result<Vec<ChunkRow>> {
        Ok(self
            .find(IndexKind::Chunks, build_id, filter, None, sort, size)?
            .iter()
            .map(|h| chunk_row(&h["_source"]))
            .collect())
    }

    fn passages_where(
        &self,
        build_id: &str,
        filter: Vec<Value>,
        sort: Value,
        size: usize,
    ) -> Result<Vec<PassageRow>> {
        Ok(self
            .find(IndexKind::Passages, build_id, filter, None, sort, size)?
            .iter()
            .map(|h| passage_row(&h["_source"]))
            .collect())
    }

    fn one_document(&self, build_id: &str, document_id: &str) -> Result<Option<Value>> {
        Ok(self
            .find(
                IndexKind::Documents,
                build_id,
                vec![json!({ "term": { "document_id": document_id } })],
                None,
                json!([{ "document_id": "asc" }]),
                1,
            )?
            .into_iter()
            .next()
            .map(|h| h["_source"].clone()))
    }

    /// `num_candidates` 0 means `max(10 * k, 100)` (Elasticsearch only;
    /// OpenSearch's HNSW search is exhaustive over `k` per segment).
    fn knn_body(
        &self,
        field_filter: Vec<Value>,
        query: &[f32],
        k: usize,
        num_candidates: usize,
    ) -> Value {
        let candidates = if num_candidates == 0 {
            (k * 10).max(100)
        } else {
            num_candidates.max(k)
        };
        match self.backend.engine() {
            Engine::OpenSearch => json!({
                "size": k,
                "_source": ["chunk_id", "entity_id", "passage_id"],
                "query": { "knn": { "embedding": {
                    "vector": query, "k": k,
                    "filter": { "bool": { "filter": field_filter } },
                } } },
            }),
            Engine::Elasticsearch => json!({
                "size": k,
                "_source": ["chunk_id", "entity_id", "passage_id"],
                "knn": {
                    "field": "embedding", "query_vector": query, "k": k,
                    "num_candidates": candidates.min(MAX_READ),
                    "filter": { "bool": { "filter": field_filter } },
                },
            }),
        }
    }

    /// Top-`k` vectors of build `b` inside `filter`, pre-filtered by the
    /// engine (exact under the filter, audit F06): the generation scope,
    /// the fingerprint and the hard filters all go into the kNN `filter`.
    fn knn_search(
        &self,
        b: &str,
        fingerprint: &str,
        query: &[f32],
        k: usize,
        num_candidates: usize,
        vf: &VectorFilter,
    ) -> Result<Option<Vec<VectorHit>>> {
        let Some(spec) = self.backend.vector_spec() else {
            return Ok(None);
        };
        if query.len() != spec.dims as usize {
            return Err(StorageError::Backend(format!(
                "query vector has {} dimensions but the server indexes were created for {} ({}); \
                 recreate the indexes for this model",
                query.len(),
                spec.dims,
                spec.model_id
            )));
        }
        if k == 0 {
            return Ok(Some(Vec::new()));
        }
        let mut filter = self.scope(b)?;
        filter.push(json!({ "term": { "embedding_fingerprint": fingerprint } }));
        filter.extend(vector_filter_clauses(vf));
        // A document filter excludes every entity.
        let entities = vf.entities && vf.document_ids.is_empty();
        // Body passages count as entity hits; plain-text passages as their
        // own kind.
        let passages = vf.document_ids.is_empty() && (vf.entities || vf.passages);
        let mut out = Vec::new();
        for (kind, subject, id_field, on) in [
            (IndexKind::Chunks, SUBJECT_CHUNK, "chunk_id", vf.documents),
            (IndexKind::Code, SUBJECT_ENTITY, "entity_id", entities),
            (IndexKind::Passages, SUBJECT_PASSAGE, "passage_id", passages),
        ] {
            if !on {
                continue;
            }
            let mut f = filter.clone();
            if kind == IndexKind::Chunks {
                f.extend(chunk_filter_clauses(vf));
            }
            if kind == IndexKind::Passages && !(vf.entities && vf.passages) {
                f.push(json!({ "term": { "kind": if vf.entities { "body" } else { "text" } } }));
            }
            for h in self.hits(kind, self.knn_body(f, query, k, num_candidates))? {
                out.push((
                    subject.to_owned(),
                    str_of(&h["_source"], id_field),
                    cosine(h["_score"].as_f64().unwrap_or(0.0)),
                ));
            }
        }
        out.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        out.truncate(k);
        Ok(Some(out))
    }
}

/// Hard-filter clauses common to every vector index (`rel_path` prefixes).
fn vector_filter_clauses(vf: &VectorFilter) -> Vec<Value> {
    path_filter(&ReadFilter {
        path_prefixes: vf.path_prefixes.clone(),
        document_ids: Vec::new(),
    })
}

/// Chunk-only hard-filter clauses: document ids (the chunk's document or
/// its parent) and the attachment exclusion.
fn chunk_filter_clauses(vf: &VectorFilter) -> Vec<Value> {
    let mut out = Vec::new();
    if !vf.document_ids.is_empty() {
        out.push(json!({ "terms": { "document_id": vf.document_ids } }));
    }
    if vf.exclude_attachments {
        out.push(
            json!({ "bool": { "must_not": [ { "exists": { "field": "attachment_index" } } ] } }),
        );
    }
    out
}

/// `rel_path` prefix clauses of `filter` (any of them), as a filter list.
fn path_filter(filter: &ReadFilter) -> Vec<Value> {
    if filter.path_prefixes.is_empty() {
        return Vec::new();
    }
    let should: Vec<Value> = filter
        .path_prefixes
        .iter()
        .map(|p| json!({ "prefix": { "rel_path": p } }))
        .collect();
    vec![json!({ "bool": { "should": should, "minimum_should_match": 1 } })]
}

/// A knowledge link as listed by `link list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerLink {
    pub id: String,
    pub link_type: String,
    pub entity_id: String,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub resolver: String,
    pub confidence: String,
    pub evidence: Option<String>,
    /// A user-defined (manual) link; `id` is what `link remove` takes.
    pub manual: bool,
}

/// A manual link to store on the server.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualLinkRecord {
    pub id: String,
    pub link_type: String,
    pub entity_id: String,
    pub entity_qualified_name: String,
    pub document_id: String,
    pub file_id: String,
    pub rel_path: String,
    pub chunk_id: Option<String>,
    pub chunk_ordinal: Option<i64>,
    pub note: Option<String>,
}

impl ServerReader {
    /// Automatic links of build `b` and the source's manual links.
    pub fn links(&self, b: &str) -> Result<Vec<ServerLink>> {
        // Without a visible graph only manual links are listed.
        let auto = match self.graph_generation(b)? {
            Some(g) => self.scope(&g)?.pop().unwrap_or(json!({ "match_none": {} })),
            None => json!({ "match_none": {} }),
        };
        let body = json!({
            "size": MAX_READ,
            "query": { "bool": { "filter": [
                { "term": { "source_id": self.source_id } },
                { "bool": { "should": [
                    { "bool": { "filter": [ auto, { "term": { "record_kind": "link" } } ] } },
                    { "bool": { "filter": [ { "term": { "build_id": MANUAL_BUILD } }, { "term": { "record_kind": MANUAL_LINK } } ] } },
                ], "minimum_should_match": 1 } },
            ] } },
            "sort": [{ "relationship_id": "asc" }],
        });
        Ok(self
            .hits(IndexKind::Relationships, body)?
            .iter()
            .map(|h| {
                let s = &h["_source"];
                ServerLink {
                    id: str_of(s, "relationship_id"),
                    link_type: opt_str(s, "link_type")
                        .unwrap_or_else(|| str_of(s, "relationship_type")),
                    entity_id: str_of(s, "entity_id"),
                    document_id: str_of(s, "document_id"),
                    chunk_id: opt_str(s, "chunk_id"),
                    resolver: str_of(s, "resolver"),
                    confidence: str_of(s, "confidence"),
                    evidence: opt_str(s, "evidence"),
                    manual: s["record_kind"].as_str() == Some(MANUAL_LINK),
                }
            })
            .collect())
    }

    /// Entities whose short name is exactly `name`.
    /// Entities of build `b` whose qualified name is exactly one of
    /// `names`, by name, in resolution order (`file_id`, `start_line`,
    /// `entity_id`, as the local store). Requests are batched and bounded;
    /// a batch reaching the read cap is an error, never a silent truncation.
    pub fn entities_by_qualified_names(
        &self,
        b: &str,
        names: &[&str],
    ) -> Result<std::collections::HashMap<String, Vec<EntityRow>>> {
        const BATCH: usize = 256;
        let mut out: std::collections::HashMap<String, Vec<EntityRow>> =
            std::collections::HashMap::new();
        for chunk in names.chunks(BATCH) {
            let hits = self.find(
                IndexKind::Code,
                b,
                vec![json!({ "terms": { "qualified_name.raw": chunk } })],
                None,
                json!([{ "file_id": "asc" }, { "start_line": "asc" }, { "entity_id": "asc" }]),
                MAX_READ,
            )?;
            if hits.len() >= MAX_READ {
                return Err(StorageError::Backend(format!(
                    "qualified-name lookup of {} name(s) reached the {MAX_READ} read cap",
                    chunk.len()
                )));
            }
            for h in &hits {
                let e = entity_row(&h["_source"]);
                out.entry(e.qualified_name.clone()).or_default().push(e);
            }
        }
        Ok(out)
    }

    pub fn entities_named(&self, b: &str, name: &str) -> Result<Vec<EntityRow>> {
        Ok(self
            .find(
                IndexKind::Code,
                b,
                vec![json!({ "term": { "name.raw": name } })],
                None,
                json!([{ "qualified_name.raw": "asc" }, { "entity_id": "asc" }]),
                MAX_READ,
            )?
            .iter()
            .map(|h| entity_row(&h["_source"]))
            .collect())
    }

    /// `(rel_path, file_id)` of a document of build `b`.
    pub fn document_location(
        &self,
        b: &str,
        document_id: &str,
    ) -> Result<Option<(String, String)>> {
        Ok(self
            .one_document(b, document_id)?
            .map(|d| (str_of(&d, "rel_path"), str_of(&d, "file_id"))))
    }

    /// Resolves a document reference (id, path, path suffix or file name of
    /// a top-level document) to `(rel_path, file_id, document_id)`; `None`
    /// when nothing or more than one document matches.
    pub fn resolve_document(&self, b: &str, r: &str) -> Result<Option<(String, String, String)>> {
        if let Some(d) = self.one_document(b, r)? {
            return Ok(Some((
                str_of(&d, "rel_path"),
                str_of(&d, "file_id"),
                str_of(&d, "document_id"),
            )));
        }
        let name = r.rsplit('/').next().unwrap_or(r);
        let hits = self.find(
            IndexKind::Documents,
            b,
            vec![
                json!({ "wildcard": { "rel_path": { "value": format!("*{}", wildcard_literal(name)) } } }),
                json!({ "bool": { "must_not": [ { "exists": { "field": "parent_document_id" } } ] } }),
            ],
            None,
            json!([{ "rel_path": "asc" }]),
            MAX_READ,
        )?;
        let matches: Vec<&Value> = hits
            .iter()
            .map(|h| &h["_source"])
            .filter(|d| {
                let p = str_of(d, "rel_path");
                p == r || p.ends_with(r) || p.rsplit('/').next() == Some(r)
            })
            .collect();
        Ok(match matches.as_slice() {
            [d] => Some((
                str_of(d, "rel_path"),
                str_of(d, "file_id"),
                str_of(d, "document_id"),
            )),
            _ => None,
        })
    }

    /// Every record of build `b` in `kind` matching `filter`, paged with
    /// `search_after` on `id_field` (no truncation).
    pub fn scan_all(
        &self,
        kind: IndexKind,
        b: &str,
        filter: Vec<Value>,
        id_field: &str,
    ) -> Result<Vec<Value>> {
        let mut f = self.scope(b)?;
        f.extend(filter);
        let mut out = Vec::new();
        let mut after: Option<Value> = None;
        loop {
            let mut body = json!({
                "size": 1000,
                "query": { "bool": { "filter": f } },
                "sort": [ { id_field: "asc" } ],
                "track_total_hits": false,
            });
            if let Some(a) = &after {
                body["search_after"] = a.clone();
            }
            let hits = self.hits(kind, body)?;
            let n = hits.len();
            after = hits.last().map(|h| h["sort"].clone());
            out.extend(hits.into_iter().map(|h| h["_source"].clone()));
            if n < 1000 {
                return Ok(out);
            }
        }
    }

    /// Every document of build `b`.
    pub fn documents(&self, b: &str) -> Result<Vec<Value>> {
        self.scan_all(IndexKind::Documents, b, vec![], "document_id")
    }

    /// Every file of build `b` (optionally of one kind).
    pub fn files(&self, b: &str, kind: Option<&str>) -> Result<Vec<Value>> {
        let filter = kind
            .map(|k| json!({ "term": { "kind": k } }))
            .into_iter()
            .collect();
        self.scan_all(IndexKind::Files, b, filter, "file_id")
    }

    /// Every file of build `b` as storage rows.
    pub fn file_rows(&self, b: &str) -> Result<Vec<docmonk_storage::knowledge::FileRow>> {
        Ok(self
            .files(b, None)?
            .iter()
            .map(|f| docmonk_storage::knowledge::FileRow {
                id: str_of(f, "file_id"),
                rel_path: str_of(f, "rel_path"),
                kind: str_of(f, "kind"),
                size: f["size"].as_i64().unwrap_or(0),
                mtime: f["mtime"].as_f64().unwrap_or(0.0),
                content_hash: opt_str(f, "content_hash"),
                status: opt_str(f, "status").unwrap_or_else(|| "indexed".into()),
                parser_version: opt_str(f, "parser_version"),
                chunker_version: opt_str(f, "chunker_version"),
                converter_version: opt_str(f, "converter_version"),
                embedding_model_id: opt_str(f, "embedding_model_id"),
                embedding_text_version: opt_str(f, "embedding_text_version"),
                last_error: opt_str(f, "last_error"),
                attempt_count: f["attempt_count"].as_i64().unwrap_or(0),
                next_attempt_at: opt_str(f, "next_attempt_at"),
                last_indexed_at: opt_str(f, "indexed_at"),
            })
            .collect())
    }

    /// Every document of build `b` as storage rows.
    pub fn document_rows(&self, b: &str) -> Result<Vec<docmonk_storage::knowledge::DocumentRow>> {
        Ok(self
            .documents(b)?
            .iter()
            .map(|d| docmonk_storage::knowledge::DocumentRow {
                id: str_of(d, "document_id"),
                file_id: str_of(d, "file_id"),
                format: str_of(d, "format"),
                title: opt_str(d, "title"),
                author: opt_str(d, "author"),
                page_count: d["page_count"].as_i64(),
                is_scanned: d["is_scanned"].as_bool().unwrap_or(false),
                content_hash: opt_str(d, "content_hash"),
                attachment: opt_str(d, "parent_document_id").map(|parent| {
                    docmonk_storage::knowledge::AttachmentProvenance {
                        parent_document_id: parent,
                        name: opt_str(d, "attachment_name"),
                        content_type: opt_str(d, "attachment_content_type"),
                        index: d["attachment_index"].as_i64().unwrap_or(0),
                        content_id: opt_str(d, "attachment_content_id"),
                    }
                }),
                language: opt_str(d, "language"),
                script: opt_str(d, "script"),
                pages: opt_str(d, "ocr").map(|ocr| docmonk_storage::knowledge::PageExtraction {
                    ocr,
                    ocr_pages: d["ocr_pages"].as_i64().unwrap_or(0),
                    empty_pages: d["empty_pages"].as_i64().unwrap_or(0),
                }),
            })
            .collect())
    }

    /// Chunks of one document of build `b`, in ordinal order.
    pub fn document_chunks(&self, b: &str, document_id: &str) -> Result<Vec<ChunkRow>> {
        let mut v: Vec<ChunkRow> = self
            .scan_all(
                IndexKind::Chunks,
                b,
                vec![json!({ "term": { "document_id": document_id } })],
                "chunk_id",
            )?
            .iter()
            .map(chunk_row)
            .collect();
        v.sort_by_key(|c| c.ordinal);
        Ok(v)
    }

    /// Every code entity of build `b`.
    pub fn all_entities(&self, b: &str) -> Result<Vec<EntityRow>> {
        Ok(self
            .scan_all(IndexKind::Code, b, vec![], "entity_id")?
            .iter()
            .map(entity_row)
            .collect())
    }

    /// `document_id -> (headings, paragraphs, tables)` chunk counts.
    pub fn chunk_kind_counts(
        &self,
        b: &str,
    ) -> Result<std::collections::HashMap<String, (i64, i64, i64)>> {
        let mut out: std::collections::HashMap<String, (i64, i64, i64)> = Default::default();
        let mut after: Option<Value> = None;
        loop {
            let mut composite = json!({
                "size": 1000,
                "sources": [
                    { "d": { "terms": { "field": "document_id" } } },
                    { "k": { "terms": { "field": "kind" } } },
                ],
            });
            if let Some(a) = &after {
                composite["after"] = a.clone();
            }
            let v = self
                .backend
                .post_json(
                    Method::Post,
                    &format!("/{}/_search", self.backend.index(IndexKind::Chunks)),
                    &json!({
                        "size": 0,
                        "query": { "bool": { "filter": self.scope(b)? } },
                        "aggs": { "c": { "composite": composite } },
                    }),
                )
                .map_err(Self::err)?;
            let buckets = v
                .pointer("/aggregations/c/buckets")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for bk in &buckets {
                let d = str_of(&bk["key"], "d");
                let n = bk["doc_count"].as_i64().unwrap_or(0);
                let e = out.entry(d).or_default();
                match bk["key"]["k"].as_str() {
                    Some("heading") => e.0 += n,
                    Some("paragraph") => e.1 += n,
                    Some("table") => e.2 += n,
                    _ => {}
                }
            }
            match v.pointer("/aggregations/c/after_key") {
                Some(k) if !buckets.is_empty() => after = Some(k.clone()),
                _ => return Ok(out),
            }
        }
    }

    fn manual_doc_id(&self, id: &str) -> String {
        format!("{MANUAL_BUILD}:{}:{id}", self.source_id)
    }

    /// Stores a manual link; `Ok(false)` when it already exists.
    pub fn put_manual_link(&self, l: &ManualLinkRecord) -> Result<bool> {
        let path = format!(
            "/{}/_create/{}?refresh=true",
            self.backend.index(IndexKind::Relationships),
            crate::backend::encode_id(&self.manual_doc_id(&l.id))
        );
        let doc = json!({
            "source_id": self.source_id,
            "build_id": MANUAL_BUILD,
            "record_kind": MANUAL_LINK,
            "relationship_id": l.id,
            "manual_link_id": l.id,
            "relationship_type": l.link_type,
            "link_type": l.link_type,
            "entity_id": l.entity_id,
            "target_symbol": l.entity_qualified_name,
            "document_id": l.document_id,
            "file_id": l.file_id,
            "rel_path": l.rel_path,
            "chunk_id": l.chunk_id,
            "chunk_ordinal": l.chunk_ordinal,
            "resolver": "user",
            "confidence": "exact",
            "evidence": l.note,
            "created_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
                .to_string(),
        });
        match self.backend.post_json(Method::Put, &path, &doc) {
            Ok(_) => Ok(true),
            Err(crate::BackendError::Http { status: 409, .. }) => Ok(false),
            Err(e) => Err(Self::err(e)),
        }
    }

    /// Deletes a manual link; `Ok(false)` when there is none with `id`.
    pub fn remove_manual_link(&self, id: &str) -> Result<bool> {
        let path = format!(
            "/{}/_doc/{}?refresh=true",
            self.backend.index(IndexKind::Relationships),
            crate::backend::encode_id(&self.manual_doc_id(id))
        );
        self.backend.delete_path(&path).map_err(Self::err)
    }
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn opt_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    match v.get(k) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn entity_row(s: &Value) -> EntityRow {
    EntityRow {
        id: str_of(s, "entity_id"),
        file_id: str_of(s, "file_id"),
        kind: str_of(s, "kind"),
        name: str_of(s, "name"),
        qualified_name: str_of(s, "qualified_name"),
        language: str_of(s, "language"),
        parent_id: opt_str(s, "parent_id"),
        signature: opt_str(s, "signature"),
        start_line: s["start_line"].as_i64().unwrap_or(0),
        end_line: s["end_line"].as_i64().unwrap_or(0),
        start_col: s["start_col"].as_i64().unwrap_or(0),
        end_col: s["end_col"].as_i64().unwrap_or(0),
    }
}

fn relationship_row(s: &Value) -> RelationshipRow {
    RelationshipRow {
        id: str_of(s, "relationship_id"),
        file_id: str_of(s, "file_id"),
        relationship_type: str_of(s, "relationship_type"),
        source_entity_id: str_of(s, "source_entity_id"),
        target_entity_id: opt_str(s, "target_entity_id"),
        target_symbol: opt_str(s, "target_symbol"),
        resolver: str_of(s, "resolver"),
        confidence: str_of(s, "confidence"),
        source_location: opt_str(s, "source_location"),
        evidence: opt_str(s, "evidence"),
        reference_text: opt_str(s, "reference_text"),
    }
}

fn passage_row(s: &Value) -> PassageRow {
    let int = |k: &str| s[k].as_i64().unwrap_or(0);
    PassageRow {
        id: str_of(s, "passage_id"),
        file_id: str_of(s, "file_id"),
        entity_id: opt_str(s, "entity_id"),
        kind: str_of(s, "kind"),
        extraction: str_of(s, "extraction"),
        language: opt_str(s, "language"),
        ordinal: int("ordinal"),
        part: int("part"),
        parts: int("parts"),
        start_line: int("start_line"),
        end_line: int("end_line"),
        header: str_of(s, "header"),
        text: str_of(s, "text"),
    }
}

fn chunk_row(s: &Value) -> ChunkRow {
    ChunkRow {
        id: str_of(s, "chunk_id"),
        document_id: str_of(s, "document_id"),
        file_id: str_of(s, "file_id"),
        kind: str_of(s, "kind"),
        ordinal: s["ordinal"].as_i64().unwrap_or(0),
        heading_path: strings(s, "heading_path"),
        heading_level: s["heading_level"].as_i64(),
        text: str_of(s, "text"),
        search_text: str_of(s, "search_text"),
        embedding_text: opt_str(s, "embedding_text"),
        token_count: s["token_count"].as_i64(),
        page_start: s["page_start"].as_i64(),
        page_end: s["page_end"].as_i64(),
        table_rows: serde_json::from_value(s["table_rows"].clone()).ok(),
        caption: opt_str(s, "caption"),
        parent_ordinal: s["parent_ordinal"].as_i64(),
        language: opt_str(s, "language"),
    }
}

/// Engine similarity score -> cosine similarity. Both engines report
/// `(1 + cos) / 2` for cosine HNSW fields.
fn cosine(score: f64) -> f32 {
    (2.0 * score - 1.0) as f32
}

/// SQL `LIKE`-style wildcard text with `*`/`?` escaped.
fn wildcard_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl KnowledgeRead for ServerReader {
    fn prefetch_lexical(
        &self,
        b: &str,
        q: &str,
        expressions: &[String],
        limit: i64,
        snippet_tokens: i64,
        filter: &ReadFilter,
    ) -> Result<()> {
        // Exactly the calls (and arguments) lexical retrieval makes.
        self.prefetch(|r| {
            let _ = r.search_entities_exact(b, q);
            if q.contains('.') {
                let _ = r.search_entities_alias(b, q, limit);
            }
            for e in expressions {
                let _ = r.search_entities_fts_in(b, e, limit, filter);
            }
            let _ = r.search_document_titles(b, q, limit);
            for e in expressions {
                let _ = r.search_chunks_fts_in(b, e, limit, snippet_tokens, filter);
            }
            for e in expressions {
                let _ = r.search_passages_fts_in(b, e, limit, snippet_tokens, filter);
            }
            let _ = r.search_paths_projection_in(b, q, limit, filter);
        })
    }

    fn as_sync(&self) -> Option<&(dyn KnowledgeRead + Sync)> {
        Some(self)
    }

    fn entity_edges_many(
        &self,
        b: &str,
        ids: &[String],
        types: &[&str],
        outgoing: bool,
        limit: i64,
    ) -> Result<std::collections::HashMap<String, Vec<RelationshipRow>>> {
        let field = if outgoing {
            "source_entity_id"
        } else {
            "target_entity_id"
        };
        self.edges_many(b, field, ids, types, vec![], limit, |r| {
            if outgoing {
                r.source_entity_id.clone()
            } else {
                r.target_entity_id.clone().unwrap_or_default()
            }
        })
    }

    fn unresolved_edges_many(
        &self,
        b: &str,
        symbols: &[String],
        types: &[&str],
        limit: i64,
    ) -> Result<std::collections::HashMap<String, Vec<RelationshipRow>>> {
        self.edges_many(
            b,
            "target_symbol",
            symbols,
            types,
            vec![json!({ "bool": { "must_not": [ { "exists": { "field": "target_entity_id" } } ] } })],
            limit,
            |r| r.target_symbol.clone().unwrap_or_default(),
        )
    }

    fn entities_by_ids(
        &self,
        b: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, EntityRow>> {
        let mut out = std::collections::HashMap::new();
        let mut uniq: Vec<&str> = ids.iter().map(String::as_str).collect();
        uniq.sort_unstable();
        uniq.dedup();
        for batch in uniq.chunks(MAX_READ) {
            for h in self.find(
                IndexKind::Code,
                b,
                vec![json!({ "terms": { "entity_id": batch } })],
                None,
                json!([{ "entity_id": "asc" }]),
                batch.len(),
            )? {
                let e = entity_row(&h["_source"]);
                out.entry(e.id.clone()).or_insert(e);
            }
        }
        Ok(out)
    }

    fn file_rel_paths(
        &self,
        b: &str,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut out = std::collections::HashMap::new();
        let mut uniq: Vec<&str> = ids.iter().map(String::as_str).collect();
        uniq.sort_unstable();
        uniq.dedup();
        for batch in uniq.chunks(MAX_READ) {
            for (id, path, _) in self.file_mtimes(b, batch)? {
                out.insert(id, path);
            }
        }
        Ok(out)
    }

    fn external_manifest(&self) -> Result<Option<docmonk_storage::graph::ExternalManifest>> {
        let Some(v) = self
            .backend
            .graph_external(&self.source_id)
            .map_err(|e| StorageError::Backend(e.to_string()))?
        else {
            return Ok(None);
        };
        serde_json::from_value(v)
            .map(Some)
            .map_err(|e| StorageError::Invalid(format!("dependency manifest: {e}")))
    }

    fn dependency_generation(&self, b: &str) -> Result<Option<String>> {
        // Server builds are immutable: the build id is the generation.
        Ok(Some(format!("server:{b}")))
    }

    fn graph_derivation(&self, b: &str) -> Result<Option<String>> {
        self.backend
            .served_graph_derivation(&self.source_id, b, self.require_current)
            .map_err(Self::err)
    }

    fn graph_freshness(&self, b: &str) -> Result<docmonk_storage::read::GraphFreshness> {
        let g = self.served_graph(b)?;
        let stale = g.as_ref().is_some_and(|(_, s)| *s);
        Ok(docmonk_storage::read::GraphFreshness {
            state: match &g {
                None => "unavailable",
                Some(_) if stale => "stale",
                Some(_) => "ready",
            }
            .into(),
            visible: g.is_some(),
            generation: g.map(|(g, _)| g),
            stale,
        })
    }
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn backend_kind(&self) -> &'static str {
        self.backend.engine().as_str()
    }

    fn search_entities_exact(&self, b: &str, q: &str) -> Result<Vec<EntitySearchRow>> {
        let hits = self.find(
            IndexKind::Code,
            b,
            vec![json!({ "bool": { "should": [
                { "term": { "name.raw": q } },
                { "term": { "qualified_name.raw": q } },
            ], "minimum_should_match": 1 } })],
            None,
            json!([{ "qualified_name.raw": "asc" }, { "file_id": "asc" }, { "start_line": "asc" }, { "entity_id": "asc" }]),
            MAX_READ,
        )?;
        Ok(self.entity_search_rows(&hits))
    }

    fn search_entities_alias(
        &self,
        b: &str,
        alias: &str,
        limit: i64,
    ) -> Result<Vec<EntitySearchRow>> {
        if alias.matches('.').count() != 1 {
            return Ok(Vec::new());
        }
        let hits = self.find(
            IndexKind::Code,
            b,
            vec![json!({ "wildcard": { "qualified_name.raw": {
                "value": format!("*.{}", wildcard_literal(alias)) } } })],
            None,
            json!([{ "qualified_name.raw": "asc" }, { "file_id": "asc" }, { "start_line": "asc" }, { "entity_id": "asc" }]),
            MAX_READ,
        )?;
        let mut rows = self.entity_search_rows(&hits);
        rows.retain(|r| r.qualified_name.matches('.').count() >= 2);
        rows.truncate(limit.max(0) as usize);
        Ok(rows)
    }

    fn search_entities_fts(
        &self,
        b: &str,
        expression: &str,
        limit: i64,
    ) -> Result<Vec<EntitySearchRow>> {
        self.search_entities_fts_in(b, expression, limit, &ReadFilter::default())
    }

    fn search_entities_fts_in(
        &self,
        b: &str,
        expression: &str,
        limit: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<EntitySearchRow>> {
        if !filter.document_ids.is_empty() {
            return Ok(Vec::new());
        }
        let Some(must) =
            Self::fts_query(expression, &["name", "qualified_name", "signature", "norm"])
        else {
            return Ok(Vec::new());
        };
        let hits = self.find(
            IndexKind::Code,
            b,
            path_filter(filter),
            Some(must),
            json!([{ "_score": "desc" }, { "rel_path": "asc" }, { "qualified_name.raw": "asc" }, { "start_line": "asc" }, { "entity_id": "asc" }]),
            limit.max(0) as usize,
        )?;
        let mut rows = self.entity_search_rows(&hits);
        for (i, (row, h)) in rows.iter_mut().zip(&hits).enumerate() {
            row.fts_rank = i as u32;
            row.bm25 = h["_score"].as_f64().map(|s| -s);
        }
        Ok(rows)
    }

    fn search_document_titles(
        &self,
        b: &str,
        title: &str,
        limit: i64,
    ) -> Result<Vec<DocumentSearchRow>> {
        let hits = self.find(
            IndexKind::Documents,
            b,
            vec![json!({ "term": { "title.raw": { "value": title, "case_insensitive": true } } })],
            None,
            json!([{ "rel_path": "asc" }, { "document_id": "asc" }]),
            limit.max(0) as usize,
        )?;
        Ok(hits
            .iter()
            .map(|h| {
                let s = &h["_source"];
                let t = str_of(s, "title");
                DocumentSearchRow {
                    id: str_of(s, "document_id"),
                    document_id: str_of(s, "document_id"),
                    snippet: Some(t.clone()),
                    title: t,
                    rel_path: str_of(s, "rel_path"),
                    mtime: s["mtime"].as_f64().unwrap_or(0.0),
                    heading: None,
                    heading_path: Vec::new(),
                    page_start: None,
                    page_end: None,
                    attachment: Self::attachment_of(s),
                    fts_rank: 0,
                    bm25: None,
                }
            })
            .collect())
    }

    fn search_chunks_fts(
        &self,
        b: &str,
        expression: &str,
        limit: i64,
        snippet_tokens: i64,
    ) -> Result<Vec<DocumentSearchRow>> {
        self.search_chunks_fts_in(b, expression, limit, snippet_tokens, &ReadFilter::default())
    }

    fn search_chunks_fts_in(
        &self,
        b: &str,
        expression: &str,
        limit: i64,
        snippet_tokens: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<DocumentSearchRow>> {
        let Some(must) = Self::fts_query(
            expression,
            &["fts_heading^5", "search_text", "title^8", "norm^0.5"],
        ) else {
            return Ok(Vec::new());
        };
        let mut f = self.scope(b)?;
        f.extend(path_filter(filter));
        if !filter.document_ids.is_empty() {
            f.push(json!({ "terms": { "document_id": filter.document_ids } }));
        }
        let tokens = snippet_tokens.clamp(1, 64) as usize;
        let body = json!({
            "size": (limit.max(0) as usize).min(MAX_READ),
            "query": { "bool": { "filter": f, "must": [must] } },
            "sort": [{ "_score": "desc" }, { "rel_path": "asc" }, { "ordinal": "asc" }, { "chunk_id": "asc" }],
            "track_scores": true,
            "highlight": {
                "fields": { "search_text": {
                    "type": "unified", "number_of_fragments": 1,
                    // About six characters per token.
                    "fragment_size": tokens * 6,
                    "pre_tags": [""], "post_tags": [""],
                } },
            },
        });
        let hits = self.hits(IndexKind::Chunks, body)?;
        Ok(hits
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let s = &h["_source"];
                let rel_path = str_of(s, "rel_path");
                let heading = opt_str(s, "fts_heading").filter(|x| !x.is_empty());
                let body = str_of(s, "search_text");
                let fallback: String = if body.is_empty() {
                    heading.clone().unwrap_or_default()
                } else {
                    body
                }
                .chars()
                .take(SNIPPET_CHARS)
                .collect();
                let snippet = h
                    .pointer("/highlight/search_text/0")
                    .and_then(Value::as_str)
                    .map(|x| x.trim().to_owned())
                    .filter(|x| !x.is_empty())
                    .or((!fallback.is_empty()).then_some(fallback));
                DocumentSearchRow {
                    id: str_of(s, "chunk_id"),
                    document_id: str_of(s, "document_id"),
                    title: opt_str(s, "title")
                        .filter(|t| !t.is_empty())
                        .unwrap_or_else(|| rel_path.clone()),
                    rel_path,
                    mtime: s["mtime"].as_f64().unwrap_or(0.0),
                    snippet,
                    heading,
                    heading_path: strings(s, "heading_path"),
                    page_start: s["page_start"].as_i64(),
                    page_end: s["page_end"].as_i64(),
                    attachment: Self::attachment_of(s),
                    fts_rank: i as u32,
                    bm25: h["_score"].as_f64().map(|x| -x),
                }
            })
            .collect())
    }

    fn search_paths_projection(&self, b: &str, q: &str, limit: i64) -> Result<Vec<PathSearchRow>> {
        self.search_paths_projection_in(b, q, limit, &ReadFilter::default())
    }

    fn search_paths_projection_in(
        &self,
        b: &str,
        q: &str,
        limit: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<PathSearchRow>> {
        if !filter.document_ids.is_empty() {
            return Ok(Vec::new());
        }
        let map = |hits: Vec<Value>| -> Vec<PathSearchRow> {
            hits.iter()
                .map(|h| {
                    let s = &h["_source"];
                    PathSearchRow {
                        id: str_of(s, "file_id"),
                        rel_path: str_of(s, "rel_path"),
                        mtime: s["mtime"].as_f64().unwrap_or(0.0),
                    }
                })
                .collect()
        };
        let size = limit.max(0) as usize;
        let tokens = docmonk_storage::search::word_tokens(q);
        if !tokens.is_empty() {
            let rows = map(self.find(
                IndexKind::Files,
                b,
                path_filter(filter),
                Some(json!({ "match": { "path_text": { "query": tokens.join(" "), "operator": "and" } } })),
                json!([{ "_score": "desc" }, { "rel_path": "asc" }]),
                size,
            )?);
            if !rows.is_empty() {
                return Ok(rows);
            }
        }
        let mut f = path_filter(filter);
        f.push(json!({ "wildcard": { "rel_path": {
            "value": format!("*{}*", wildcard_literal(q)), "case_insensitive": true } } }));
        Ok(map(self.find(
            IndexKind::Files,
            b,
            f,
            None,
            json!([{ "rel_path": "asc" }]),
            size,
        )?))
    }

    fn symbol_entities(&self, b: &str, q: &str) -> Result<Vec<EntityRow>> {
        Ok(self
            .find(
                IndexKind::Code,
                b,
                vec![json!({ "bool": { "should": [
                    { "term": { "name.raw": q } },
                    { "term": { "qualified_name.raw": q } },
                ], "minimum_should_match": 1 } })],
                None,
                json!([{ "qualified_name.raw": "asc" }, { "file_id": "asc" }, { "start_line": "asc" }, { "entity_id": "asc" }]),
                MAX_READ,
            )?
            .iter()
            .map(|h| entity_row(&h["_source"]))
            .collect())
    }

    fn entity(&self, b: &str, id: &str) -> Result<Option<EntityRow>> {
        Ok(self
            .find(
                IndexKind::Code,
                b,
                vec![json!({ "term": { "entity_id": id } })],
                None,
                json!([{ "entity_id": "asc" }]),
                1,
            )?
            .first()
            .map(|h| entity_row(&h["_source"])))
    }

    fn entity_edges(
        &self,
        b: &str,
        entity_id: &str,
        rtype: Option<&str>,
        outgoing: bool,
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        let field = if outgoing {
            "source_entity_id"
        } else {
            "target_entity_id"
        };
        self.edges(
            b,
            vec![json!({ "term": { field: entity_id } })],
            rtype,
            limit,
        )
    }

    fn unresolved_edges(
        &self,
        b: &str,
        symbol: &str,
        rtype: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RelationshipRow>> {
        self.edges(
            b,
            vec![
                json!({ "term": { "target_symbol": symbol } }),
                json!({ "bool": { "must_not": [ { "exists": { "field": "target_entity_id" } } ] } }),
            ],
            rtype,
            limit,
        )
    }

    fn file_rel_path(&self, b: &str, file_id: &str) -> Result<Option<String>> {
        Ok(self.file_mtimes(b, &[file_id])?.pop().map(|(_, p, _)| p))
    }

    fn entity_links(&self, b: &str, entity_id: &str) -> Result<Vec<EntityLinkRow>> {
        // Automatic links of the build's visible graph generation plus the
        // source's manual links; nothing without a visible graph.
        let Some(graph) = self.graph_generation(b)? else {
            return Ok(Vec::new());
        };
        let auto = self
            .scope(&graph)?
            .pop()
            .unwrap_or(json!({ "match_none": {} }));
        let path = format!("/{}/_search", self.backend.index(IndexKind::Relationships));
        let body = json!({
            "size": MAX_READ,
            "query": { "bool": { "filter": [
                { "term": { "source_id": self.source_id } },
                { "term": { "entity_id": entity_id } },
                { "bool": { "should": [
                    { "bool": { "filter": [ auto, { "term": { "record_kind": "link" } } ] } },
                    { "bool": { "filter": [ { "term": { "build_id": MANUAL_BUILD } }, { "term": { "record_kind": MANUAL_LINK } } ] } },
                ], "minimum_should_match": 1 } },
            ] } },
            "sort": [{ "relationship_id": "asc" }],
        });
        let v = self
            .backend
            .post_json(Method::Post, &path, &body)
            .map_err(Self::err)?;
        let mut out = Vec::new();
        for h in v
            .pointer("/hits/hits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let s = &h["_source"];
            let document_id = str_of(s, "document_id");
            // A link to a document that is not in this build is skipped,
            // as in the local store's join.
            let Some(doc) = self.one_document(b, &document_id)? else {
                continue;
            };
            let chunk_id = opt_str(s, "chunk_id");
            let chunk = match &chunk_id {
                Some(c) => self.chunk(b, c)?,
                None => None,
            };
            out.push((
                str_of(s, "relationship_id"),
                EntityLinkRow {
                    link_type: opt_str(s, "link_type")
                        .unwrap_or_else(|| str_of(s, "relationship_type")),
                    document_id,
                    chunk_id,
                    resolver: str_of(s, "resolver"),
                    confidence: str_of(s, "confidence"),
                    document_path: opt_str(&doc, "rel_path").unwrap_or_default(),
                    chunk_page_start: chunk.as_ref().and_then(|c| c.page_start),
                    chunk_heading_path: chunk.map(|c| c.heading_path).unwrap_or_default(),
                },
            ));
        }
        let conf = |c: &str| match c {
            "exact" => 0,
            "high" => 1,
            "medium" => 2,
            _ => 3,
        };
        let res = |r: &str| match r {
            "linker:exact_identifier" => 0,
            "linker:qualified_identifier" => 1,
            "linker:alias" => 2,
            "linker:filename" => 3,
            "linker:route_heuristic" => 4,
            _ => 5,
        };
        out.sort_by(|(ia, a), (ib, b)| {
            conf(&a.confidence)
                .cmp(&conf(&b.confidence))
                .then(res(&a.resolver).cmp(&res(&b.resolver)))
                .then(a.link_type.cmp(&b.link_type))
                .then(ia.cmp(ib))
        });
        Ok(out.into_iter().map(|(_, r)| r).collect())
    }

    fn document_attachment(
        &self,
        b: &str,
        document_id: &str,
    ) -> Result<Option<AttachmentProvenance>> {
        Ok(self
            .one_document(b, document_id)?
            .and_then(|d| Self::attachment_of(&d)))
    }

    fn chunk(&self, b: &str, id: &str) -> Result<Option<ChunkRow>> {
        Ok(self
            .chunks_where(
                b,
                vec![json!({ "term": { "chunk_id": id } })],
                json!([{ "chunk_id": "asc" }]),
                1,
            )?
            .pop())
    }

    fn chunk_at(&self, b: &str, document_id: &str, ordinal: i64) -> Result<Option<ChunkRow>> {
        Ok(self
            .chunks_where(
                b,
                vec![
                    json!({ "term": { "document_id": document_id } }),
                    json!({ "term": { "ordinal": ordinal } }),
                ],
                json!([{ "chunk_id": "asc" }]),
                1,
            )?
            .pop())
    }

    fn chunk_siblings(
        &self,
        b: &str,
        c: &ChunkRow,
        previous: i64,
        next: i64,
    ) -> Result<(Vec<ChunkRow>, Vec<ChunkRow>)> {
        let parent = match c.parent_ordinal {
            Some(p) => json!({ "term": { "parent_ordinal": p } }),
            None => {
                json!({ "bool": { "must_not": [ { "exists": { "field": "parent_ordinal" } } ] } })
            }
        };
        let doc = json!({ "term": { "document_id": c.document_id } });
        let mut before = if previous > 0 {
            self.chunks_where(
                b,
                vec![
                    doc.clone(),
                    parent.clone(),
                    json!({ "range": { "ordinal": { "lt": c.ordinal } } }),
                ],
                json!([{ "ordinal": "desc" }]),
                previous as usize,
            )?
        } else {
            Vec::new()
        };
        before.reverse();
        let after = if next > 0 {
            self.chunks_where(
                b,
                vec![
                    doc,
                    parent,
                    json!({ "range": { "ordinal": { "gt": c.ordinal } } }),
                ],
                json!([{ "ordinal": "asc" }]),
                next as usize,
            )?
        } else {
            Vec::new()
        };
        Ok((before, after))
    }

    fn vector_search(
        &self,
        b: &str,
        fingerprint: &str,
        query: &[f32],
        k: usize,
        num_candidates: usize,
    ) -> Result<Option<Vec<VectorHit>>> {
        self.knn_search(
            b,
            fingerprint,
            query,
            k,
            num_candidates,
            &VectorFilter {
                entities: true,
                documents: true,
                passages: true,
                ..VectorFilter::default()
            },
        )
    }

    fn search_passages_fts_in(
        &self,
        b: &str,
        expression: &str,
        limit: i64,
        snippet_tokens: i64,
        filter: &ReadFilter,
    ) -> Result<Vec<PassageSearchRow>> {
        if !filter.document_ids.is_empty() {
            return Ok(Vec::new());
        }
        let Some(must) = Self::fts_query(expression, &["header^3", "text", "norm^0.5"]) else {
            return Ok(Vec::new());
        };
        let mut f = self.scope(b)?;
        f.extend(path_filter(filter));
        let tokens = snippet_tokens.clamp(1, 64) as usize;
        let body = json!({
            "size": (limit.max(0) as usize).min(MAX_READ),
            "query": { "bool": { "filter": f, "must": [must] } },
            "sort": [{ "_score": "desc" }, { "rel_path": "asc" }, { "ordinal": "asc" }, { "passage_id": "asc" }],
            "track_scores": true,
            "highlight": {
                "fields": { "text": {
                    "type": "unified", "number_of_fragments": 1,
                    "fragment_size": tokens * 6,
                    "pre_tags": [""], "post_tags": [""],
                } },
            },
        });
        let hits = self.hits(IndexKind::Passages, body)?;
        Ok(hits
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let s = &h["_source"];
                let p = passage_row(s);
                let snippet = h
                    .pointer("/highlight/text/0")
                    .and_then(Value::as_str)
                    .map(|x| x.trim().to_owned())
                    .filter(|x| !x.is_empty())
                    .or_else(|| Some(p.text.chars().take(SNIPPET_CHARS).collect()));
                PassageSearchRow {
                    entity: p.entity_id.clone().map(|id| PassageOwner {
                        id,
                        name: str_of(s, "entity_name"),
                        qualified_name: str_of(s, "entity_qualified_name"),
                        kind: str_of(s, "entity_kind"),
                    }),
                    id: p.id,
                    file_id: p.file_id,
                    kind: p.kind,
                    extraction: p.extraction,
                    language: p.language,
                    start_line: p.start_line,
                    end_line: p.end_line,
                    header: p.header,
                    rel_path: str_of(s, "rel_path"),
                    mtime: s["mtime"].as_f64().unwrap_or(0.0),
                    snippet,
                    fts_rank: i as u32,
                    bm25: h["_score"].as_f64().map(|x| -x),
                }
            })
            .collect())
    }

    fn passage(&self, b: &str, id: &str) -> Result<Option<PassageRow>> {
        Ok(self
            .passages_where(
                b,
                vec![json!({ "term": { "passage_id": id } })],
                json!([{ "passage_id": "asc" }]),
                1,
            )?
            .pop())
    }

    fn entity_passages(&self, b: &str, entity_id: &str) -> Result<Vec<PassageRow>> {
        self.passages_where(
            b,
            vec![json!({ "term": { "entity_id": entity_id } })],
            json!([{ "ordinal": "asc" }]),
            MAX_READ,
        )
    }

    fn passage_siblings(
        &self,
        b: &str,
        p: &PassageRow,
        previous: i64,
        next: i64,
    ) -> Result<(Vec<PassageRow>, Vec<PassageRow>)> {
        let owner = match &p.entity_id {
            Some(e) => json!({ "term": { "entity_id": e } }),
            None => json!({ "bool": { "must_not": [ { "exists": { "field": "entity_id" } } ] } }),
        };
        let file = json!({ "term": { "file_id": p.file_id } });
        let side = |cmp: &str, order: &str, n: i64| -> Result<Vec<PassageRow>> {
            if n <= 0 {
                return Ok(Vec::new());
            }
            self.passages_where(
                b,
                vec![
                    file.clone(),
                    owner.clone(),
                    json!({ "range": { "ordinal": { cmp: p.ordinal } } }),
                ],
                json!([{ "ordinal": order }]),
                n as usize,
            )
        };
        let mut before = side("lt", "desc", previous)?;
        before.reverse();
        Ok((before, side("gt", "asc", next)?))
    }

    fn vector_search_filtered(
        &self,
        b: &str,
        fingerprint: &str,
        query: &[f32],
        k: usize,
        num_candidates: usize,
        filter: &VectorFilter,
    ) -> Result<Option<Vec<VectorHit>>> {
        self.knn_search(b, fingerprint, query, k, num_candidates, filter)
    }

    fn subject_meta(&self, b: &str, keys: &[(String, String)]) -> Result<Vec<SubjectMeta>> {
        // Three searches for the whole key set (entities, their files,
        // chunks), not one or two per key.
        let entity_ids: Vec<&str> = keys
            .iter()
            .filter(|(t, _)| t == SUBJECT_ENTITY)
            .map(|(_, id)| id.as_str())
            .collect();
        let chunk_ids: Vec<&str> = keys
            .iter()
            .filter(|(t, _)| t == SUBJECT_CHUNK)
            .map(|(_, id)| id.as_str())
            .collect();
        let passage_ids: Vec<&str> = keys
            .iter()
            .filter(|(t, _)| t == SUBJECT_PASSAGE)
            .map(|(_, id)| id.as_str())
            .collect();
        let mut passages: std::collections::HashMap<String, Value> = Default::default();
        for batch in passage_ids.chunks(MAX_READ) {
            for h in self.find(
                IndexKind::Passages,
                b,
                vec![json!({ "terms": { "passage_id": batch } })],
                None,
                json!([{ "passage_id": "asc" }]),
                batch.len(),
            )? {
                let id = str_of(&h["_source"], "passage_id");
                passages.entry(id).or_insert_with(|| h["_source"].clone());
            }
        }
        let mut entities: std::collections::HashMap<String, EntityRow> = Default::default();
        for batch in entity_ids.chunks(MAX_READ) {
            for h in self.find(
                IndexKind::Code,
                b,
                vec![json!({ "terms": { "entity_id": batch } })],
                None,
                json!([{ "entity_id": "asc" }]),
                batch.len(),
            )? {
                let e = entity_row(&h["_source"]);
                entities.entry(e.id.clone()).or_insert(e);
            }
        }
        let file_ids: Vec<String> = entities.values().map(|e| e.file_id.clone()).collect();
        let paths = self.file_rel_paths(b, &file_ids)?;
        let mut chunks: std::collections::HashMap<String, Value> = Default::default();
        for batch in chunk_ids.chunks(MAX_READ) {
            for h in self.find(
                IndexKind::Chunks,
                b,
                vec![json!({ "terms": { "chunk_id": batch } })],
                None,
                json!([{ "chunk_id": "asc" }]),
                batch.len(),
            )? {
                let id = str_of(&h["_source"], "chunk_id");
                chunks.entry(id).or_insert_with(|| h["_source"].clone());
            }
        }
        let mut out = Vec::with_capacity(keys.len());
        for (t, id) in keys {
            if t == SUBJECT_ENTITY {
                if let Some(e) = entities.get(id) {
                    let path = paths.get(&e.file_id).cloned().unwrap_or_default();
                    out.push(SubjectMeta {
                        subject_type: t.clone(),
                        subject_id: id.clone(),
                        kind: "entity".into(),
                        record_id: id.clone(),
                        rel_path: path,
                        title: e.qualified_name.clone(),
                        snippet: e
                            .signature
                            .clone()
                            .unwrap_or_else(|| e.qualified_name.clone()),
                        section: None,
                        position: e.start_line,
                        end_line: Some(e.end_line),
                        attachment_index: None,
                        document_id: None,
                    });
                }
                continue;
            }
            if t == SUBJECT_PASSAGE {
                if let Some(s) = passages.get(id) {
                    let p = passage_row(s);
                    out.push(passage_meta(
                        t,
                        id,
                        str_of(s, "rel_path"),
                        p.entity_id,
                        opt_str(s, "entity_qualified_name"),
                        &p.text,
                        p.start_line,
                        p.end_line,
                    ));
                }
                continue;
            }
            let Some(s) = chunks.get(id) else { continue };
            let c = chunk_row(s);
            let path = str_of(s, "rel_path");
            let title = opt_str(s, "title")
                .filter(|x| !x.is_empty())
                .unwrap_or_else(|| path.clone());
            let text = match (c.kind.as_str(), &c.table_rows) {
                ("table", Some(rows)) => rows
                    .iter()
                    .flatten()
                    .filter(|x| !x.is_empty())
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => c.text.clone(),
            };
            out.push(SubjectMeta {
                subject_type: t.clone(),
                subject_id: id.clone(),
                kind: "document".into(),
                record_id: id.clone(),
                title,
                rel_path: path,
                snippet: text.chars().take(SNIPPET_CHARS).collect(),
                section: (!c.heading_path.is_empty()).then(|| c.heading_path.join(" > ")),
                position: c.ordinal,
                end_line: None,
                attachment_index: s["attachment_index"].as_i64(),
                document_id: Some(c.document_id),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_translation_covers_the_generated_grammar() {
        let q = ServerReader::fts_query("\"settle bet\"", &["a"]).unwrap();
        assert_eq!(q["bool"]["must"][0]["multi_match"]["type"], "phrase");
        let q = ServerReader::fts_query("settle* AND \"in\"", &["a"]).unwrap();
        assert_eq!(q["bool"]["must"][0]["multi_match"]["type"], "phrase_prefix");
        assert_eq!(q["bool"]["must"].as_array().unwrap().len(), 2);
        let q = ServerReader::fts_query("\"a\" OR \"b\"", &["a"]).unwrap();
        assert_eq!(q["bool"]["minimum_should_match"], 1);
        assert!(ServerReader::fts_query("a AND b", &["a"]).is_none());
    }

    #[test]
    fn hard_filters_become_knn_prefilter_clauses() {
        let vf = VectorFilter {
            path_prefixes: vec!["src/".into()],
            document_ids: vec!["d1".into()],
            entities: true,
            documents: true,
            passages: false,
            exclude_attachments: true,
        };
        let common = vector_filter_clauses(&vf);
        assert_eq!(common[0]["bool"]["should"][0]["prefix"]["rel_path"], "src/");
        let chunk = chunk_filter_clauses(&vf);
        assert_eq!(chunk[0]["terms"]["document_id"][0], "d1");
        assert_eq!(
            chunk[1]["bool"]["must_not"][0]["exists"]["field"],
            "attachment_index"
        );
        assert!(vector_filter_clauses(&VectorFilter::default()).is_empty());
        assert!(chunk_filter_clauses(&VectorFilter::default()).is_empty());
    }

    #[test]
    fn scores_and_wildcards() {
        assert!((cosine(1.0) - 1.0).abs() < 1e-6);
        assert!((cosine(0.5)).abs() < 1e-6);
        assert_eq!(wildcard_literal("a*b?c\\"), "a\\*b\\?c\\\\");
    }
}
