//! Batched, bounded status reads and fenced run heartbeats.
//!
//! A full status snapshot of N sources costs a fixed number of requests
//! per [`PAIR_BATCH`] sources, never one per source:
//!
//! 1. the source-state catalog (paged by [`crate::backend::SCAN_PAGE`]):
//!    the frozen `source_id → active_build_id` pairs of this snapshot;
//! 2. file counters (by status, retry queue, last error time) grouped by
//!    source in one aggregation per batch;
//! 3. code/document/chunk/relationship counts of all four indexes grouped
//!    by index, source and record kind in one aggregation per batch;
//! 4. the newest file errors, globally sorted by `last_error_at`, and the
//!    edge/link counts of each source's visible relationship graph
//!    generation (graph records are never scoped to a base build);
//! 5. run heartbeats (`{prefix}-runtime`);
//! 6. cluster health of the RagMonk indexes;
//! 7. one cheap re-read of the active builds to detect a publication that
//!    happened while reading (changed sources are re-read once).
//!
//! Every aggregate is scoped to the frozen pairs (`source_id` and
//! `build_id` terms; build ids are unique per source), so a pending or
//! retired build is never counted and two builds are never mixed.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use crate::backend::{encode_id, ServerBackend, SCAN_PAGE};
use crate::error::{BackendError, Result};
use crate::schema::IndexKind;
use crate::transport::Method;

/// Sources per aggregation request (terms filters, bounded response size).
pub const PAIR_BATCH: usize = 1000;
/// Upper bound on error events per request.
pub const MAX_ERRORS: usize = 200;

/// Counters of one source's published build.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PublishedAgg {
    pub files: u64,
    pub indexed: u64,
    pub failed: u64,
    pub retry: u64,
    pub max_attempt_count: u64,
    pub next_retry_at: Option<String>,
    /// Epoch milliseconds of the newest file error.
    pub last_error_at: Option<String>,
    pub entities: u64,
    pub documents: u64,
    pub chunks: u64,
    pub relationships: u64,
    pub links: u64,
}

/// One file error of a published build.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileErrorEvent {
    pub source_id: String,
    pub rel_path: String,
    pub status: Option<String>,
    pub error: String,
    pub attempt_count: Option<u64>,
    /// Epoch milliseconds; `None` when the record carries no event time.
    pub last_error_at: Option<String>,
}

/// Cluster health of the RagMonk indexes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClusterStatus {
    pub status: Option<String>,
    pub unassigned_shards: Option<u64>,
    pub number_of_nodes: Option<u64>,
    pub indexes: Vec<String>,
}

/// What [`ServerBackend::status_snapshot`] reads.
#[derive(Debug, Clone)]
pub struct StatusQuery {
    pub error_limit: usize,
    pub error_source: Option<String>,
    /// Read the published aggregates and error feed (the heavy part);
    /// `false` for a watch refresh that reuses cached aggregates.
    pub published: bool,
}

/// A section that could not be read: its error, never a zero value.
pub type Section<T> = std::result::Result<T, String>;

/// One consistent status snapshot.
#[derive(Debug, Clone)]
pub struct ServerSnapshot {
    /// Raw source-state documents (catalog, build lifecycle, lease).
    pub states: Vec<Value>,
    /// The frozen `source_id → active_build_id` pairs.
    pub pairs: BTreeMap<String, String>,
    pub published: Option<Section<BTreeMap<String, PublishedAgg>>>,
    pub errors: Option<Section<Vec<FileErrorEvent>>>,
    pub runtime: Section<Vec<Value>>,
    pub cluster: Section<ClusterStatus>,
    /// Sources re-read because a publication changed them mid-snapshot.
    pub retried: Vec<String>,
    /// Sources that kept changing: their aggregates may be incomplete.
    pub inconsistent: Vec<String>,
    pub requests: u64,
    /// Round trip of the catalog read.
    pub latency_ms: u64,
}

/// A run's heartbeat document.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RuntimeDoc {
    pub source_id: String,
    pub run_id: String,
    pub host: String,
    pub owner: String,
    pub lease_token: i64,
    pub operation: Option<String>,
    pub stage: Option<String>,
    pub active: bool,
    pub outcome: Option<String>,
    pub scanned: i64,
    pub planned: Option<i64>,
    pub processed: i64,
    pub indexed: i64,
    pub failed: i64,
    pub retry: i64,
    pub error: Option<String>,
    /// Dates as epoch milliseconds.
    pub started_at: Option<String>,
    pub last_progress_at: Option<String>,
    pub heartbeat_at: Option<String>,
    pub expires_at: Option<String>,
    pub finished_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Painless: keep the stored document when it belongs to a newer lease
/// (higher fencing token); otherwise replace it.
const FENCED_PUT: &str = "if (ctx._source.lease_token != null && ctx._source.lease_token > params.doc.lease_token) { ctx.op = 'none' } else { ctx._source.clear(); ctx._source.putAll(params.doc) }";

fn u(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
        .unwrap_or(0)
}

/// Date aggregations return epoch millis as a float `value`.
fn millis(v: &Value) -> Option<String> {
    v.as_f64().map(|f| (f as i64).to_string())
}

fn buckets<'a>(v: &'a Value, ptr: &str) -> impl Iterator<Item = &'a Value> {
    v.pointer(ptr)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

impl ServerBackend {
    fn requests(&self) -> u64 {
        self.stats().requests
    }

    /// Filter matching only the records of the given pairs' builds.
    fn pairs_filter(pairs: &[(&String, &String)]) -> Value {
        if pairs.is_empty() {
            return json!({ "match_none": {} });
        }
        let sources: Vec<&String> = pairs.iter().map(|p| p.0).collect();
        let builds: Vec<&String> = pairs.iter().map(|p| p.1).collect();
        json!({ "bool": { "filter": [
            { "terms": { "source_id": sources } },
            { "terms": { "build_id": builds } },
        ]}})
    }

    /// Every source-state document, paged (the catalog of a snapshot).
    pub fn status_states(&self) -> Result<Vec<Value>> {
        self.scan_state_docs(json!({ "match_all": {} }), None)
    }

    pub(crate) fn scan_state_docs(
        &self,
        query: Value,
        fields: Option<&[&str]>,
    ) -> Result<Vec<Value>> {
        let path = format!("/{}/_search", self.index(IndexKind::SourceState));
        let mut out = Vec::new();
        let mut after: Option<Value> = None;
        loop {
            let mut body = json!({
                "size": SCAN_PAGE,
                "query": query,
                "sort": [ { "source_id": "asc" } ],
                "track_total_hits": false,
            });
            if let Some(f) = fields {
                body["_source"] = json!(f);
            }
            if let Some(a) = &after {
                body["search_after"] = a.clone();
            }
            let v = self.post_json(Method::Post, &path, &body)?;
            let hits = v
                .pointer("/hits/hits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            out.extend(hits.iter().map(|h| h["_source"].clone()));
            if hits.len() < SCAN_PAGE {
                break;
            }
            after = hits.last().map(|h| h["sort"].clone());
        }
        Ok(out)
    }

    /// `source_id → active_build_id` of the given state documents.
    pub fn pairs_of(states: &[Value]) -> BTreeMap<String, String> {
        states
            .iter()
            .filter_map(|d| {
                Some((
                    d["source_id"].as_str()?.to_owned(),
                    d["active_build_id"].as_str()?.to_owned(),
                ))
            })
            .collect()
    }

    /// `source_id → visible graph generation` of the given state documents
    /// against the frozen base `pairs`.
    pub fn graph_pairs_of(
        states: &[Value],
        pairs: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        states
            .iter()
            .filter_map(|d| {
                let source = d["source_id"].as_str()?;
                let g = &d["versions"][crate::graph::GRAPH_KEY];
                let base = pairs.get(source)?;
                (g["state"].as_str() == Some("ready")
                    && g["base_build_id"].as_str() == Some(base.as_str()))
                .then(|| Some((source.to_owned(), g["generation"].as_str()?.to_owned())))
                .flatten()
            })
            .collect()
    }

    /// `(edges, links)` of every graph generation pair.
    pub fn graph_aggregates(
        &self,
        graphs: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, (u64, u64)>> {
        let all: Vec<(&String, &String)> = graphs.iter().collect();
        let mut out = BTreeMap::new();
        for chunk in all.chunks(PAIR_BATCH) {
            let v = self.post_json(
                Method::Post,
                &format!("/{}/_search", self.index(IndexKind::Relationships)),
                &json!({
                    "size": 0,
                    "track_total_hits": false,
                    "query": { "bool": { "filter": [ Self::pairs_filter(chunk) ] } },
                    "aggs": { "by_source": {
                        "terms": { "field": "source_id", "size": chunk.len() },
                        "aggs": { "kind": { "terms": { "field": "record_kind", "size": 8 } } },
                    } },
                }),
            )?;
            for b in buckets(&v, "/aggregations/by_source/buckets") {
                let Some(source) = b["key"].as_str() else {
                    continue;
                };
                let mut counts = (0, 0);
                for k in buckets(b, "/kind/buckets") {
                    match k["key"].as_str() {
                        Some("edge") => counts.0 = u(&k["doc_count"]),
                        Some("link") => counts.1 = u(&k["doc_count"]),
                        _ => {}
                    }
                }
                out.insert(source.to_owned(), counts);
            }
        }
        Ok(out)
    }

    /// Published counters of every pair, [`PAIR_BATCH`] sources per
    /// request pair (files + records).
    pub fn published_aggregates(
        &self,
        pairs: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, PublishedAgg>> {
        let all: Vec<(&String, &String)> = pairs.iter().collect();
        let mut out: BTreeMap<String, PublishedAgg> = pairs
            .keys()
            .map(|k| (k.clone(), PublishedAgg::default()))
            .collect();
        for chunk in all.chunks(PAIR_BATCH) {
            let filter = Self::pairs_filter(chunk);
            let files = self.post_json(
                Method::Post,
                &format!("/{}/_search", self.index(IndexKind::Files)),
                &json!({
                    "size": 0,
                    "track_total_hits": false,
                    "query": { "bool": { "filter": [ filter ] } },
                    "aggs": { "by_source": {
                        "terms": { "field": "source_id", "size": chunk.len() },
                        "aggs": {
                            "by_status": { "terms": { "field": "status", "size": 32, "missing": "indexed" } },
                            "max_attempt": { "max": { "field": "attempt_count" } },
                            "last_error_at": { "max": { "field": "last_error_at" } },
                            "retry": { "filter": { "term": { "status": "retry" } }, "aggs": {
                                "next": { "terms": { "field": "next_attempt_at", "size": 1, "order": { "_key": "asc" } } },
                            } },
                        },
                    } },
                }),
            )?;
            for b in buckets(&files, "/aggregations/by_source/buckets") {
                let Some(agg) = b["key"].as_str().and_then(|k| out.get_mut(k)) else {
                    continue;
                };
                agg.files = u(&b["doc_count"]);
                for s in buckets(b, "/by_status/buckets") {
                    let n = u(&s["doc_count"]);
                    match s["key"].as_str() {
                        Some("indexed") => agg.indexed = n,
                        Some("failed") => agg.failed = n,
                        Some("retry") => agg.retry = n,
                        _ => {}
                    }
                }
                agg.max_attempt_count = u(&b["max_attempt"]["value"]);
                agg.last_error_at = millis(&b["last_error_at"]["value"]);
                agg.next_retry_at = b
                    .pointer("/retry/next/buckets/0/key")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            let names = [
                IndexKind::Code,
                IndexKind::Documents,
                IndexKind::Chunks,
                IndexKind::Relationships,
            ]
            .map(|k| self.index(k));
            let records = self.post_json(
                Method::Post,
                &format!("/{}/_search", names.join(",")),
                &json!({
                    "size": 0,
                    "track_total_hits": false,
                    "query": { "bool": { "filter": [ Self::pairs_filter(chunk) ] } },
                    "aggs": { "by_index": {
                        "terms": { "field": "_index", "size": 4 },
                        "aggs": { "by_source": {
                            "terms": { "field": "source_id", "size": chunk.len() },
                            "aggs": { "kind": { "terms": { "field": "record_kind", "size": 8 } } },
                        } },
                    } },
                }),
            )?;
            for ix in buckets(&records, "/aggregations/by_index/buckets") {
                let index = ix["key"].as_str().unwrap_or_default();
                for b in buckets(ix, "/by_source/buckets") {
                    let Some(agg) = b["key"].as_str().and_then(|k| out.get_mut(k)) else {
                        continue;
                    };
                    let n = u(&b["doc_count"]);
                    if index == names[0] {
                        agg.entities = n;
                    } else if index == names[1] {
                        agg.documents = n;
                    } else if index == names[2] {
                        agg.chunks = n;
                    } else if index == names[3] {
                        for k in buckets(b, "/kind/buckets") {
                            match k["key"].as_str() {
                                Some("edge") => agg.relationships = u(&k["doc_count"]),
                                Some("link") => agg.links = u(&k["doc_count"]),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// The newest file errors of the published builds, globally sorted by
    /// `last_error_at` (newest first), then source id and path.
    pub fn recent_file_errors(
        &self,
        pairs: &BTreeMap<String, String>,
        limit: usize,
        source: Option<&str>,
    ) -> Result<Vec<FileErrorEvent>> {
        let limit = limit.min(MAX_ERRORS);
        let all: Vec<(&String, &String)> = pairs
            .iter()
            .filter(|(s, _)| source.is_none_or(|f| f == s.as_str()))
            .collect();
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        for chunk in all.chunks(PAIR_BATCH) {
            let v = self.post_json(
                Method::Post,
                &format!("/{}/_search", self.index(IndexKind::Files)),
                &json!({
                    "size": limit,
                    "track_total_hits": false,
                    "_source": ["source_id", "rel_path", "status", "last_error", "attempt_count", "last_error_at"],
                    "query": { "bool": {
                        "filter": [ Self::pairs_filter(chunk) ],
                        "should": [
                            { "exists": { "field": "last_error_at" } },
                            { "terms": { "status": ["failed", "retry"] } },
                        ],
                        "minimum_should_match": 1,
                    } },
                    "sort": [
                        { "last_error_at": { "order": "desc", "missing": "_last" } },
                        { "source_id": "asc" },
                        { "rel_path": "asc" },
                    ],
                }),
            )?;
            for h in buckets(&v, "/hits/hits") {
                let s = &h["_source"];
                out.push(FileErrorEvent {
                    source_id: s["source_id"].as_str().unwrap_or_default().to_owned(),
                    rel_path: s["rel_path"].as_str().unwrap_or_default().to_owned(),
                    status: s["status"].as_str().map(str::to_owned),
                    error: s["last_error"].as_str().unwrap_or_default().to_owned(),
                    attempt_count: s["attempt_count"].as_u64(),
                    last_error_at: match &s["last_error_at"] {
                        Value::String(v) => Some(v.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    },
                });
            }
        }
        Ok(out)
    }

    /// Every run heartbeat document (one per source), paged.
    pub fn runtime_docs(&self) -> Result<Vec<Value>> {
        let path = format!("/{}/_search", self.index(IndexKind::Runtime));
        let mut out = Vec::new();
        let mut after: Option<Value> = None;
        loop {
            let mut body = json!({
                "size": SCAN_PAGE,
                "query": { "match_all": {} },
                "sort": [ { "source_id": "asc" } ],
                "track_total_hits": false,
            });
            if let Some(a) = &after {
                body["search_after"] = a.clone();
            }
            let v = self.post_json(Method::Post, &path, &body)?;
            let hits: Vec<Value> = buckets(&v, "/hits/hits").cloned().collect();
            out.extend(hits.iter().map(|h| h["_source"].clone()));
            if hits.len() < SCAN_PAGE {
                break;
            }
            after = hits.last().map(|h| h["sort"].clone());
        }
        Ok(out)
    }

    /// Health of the cluster as it affects the RagMonk indexes.
    pub fn cluster_status(&self) -> Result<ClusterStatus> {
        let names: Vec<String> = IndexKind::ALL.iter().map(|k| self.index(*k)).collect();
        let v = self.get_json(&format!(
            "/_cluster/health/{}?level=cluster&timeout=2s",
            names.join(",")
        ));
        let v = match v {
            Ok(v) => v,
            // Health of missing indexes times out with 408: report what it says.
            Err(BackendError::Http { status: 408, .. }) => json!({ "status": "red" }),
            Err(e) => return Err(e),
        };
        Ok(ClusterStatus {
            status: v["status"].as_str().map(str::to_owned),
            unassigned_shards: v["unassigned_shards"].as_u64(),
            number_of_nodes: v["number_of_nodes"].as_u64(),
            indexes: names,
        })
    }

    /// Writes (upserts) a run heartbeat, fenced by the lease token: a
    /// document of a newer lease is never overwritten. Returns `false`
    /// when the write was fenced off.
    pub fn write_runtime(&self, doc: &RuntimeDoc) -> Result<bool> {
        let v = serde_json::to_value(doc).map_err(|e| BackendError::Invalid(e.to_string()))?;
        let path = format!(
            "/{}/_update/{}?retry_on_conflict=3",
            self.index(IndexKind::Runtime),
            encode_id(&doc.source_id)
        );
        let resp = self.post_json(
            Method::Post,
            &path,
            &json!({
                "script": { "lang": "painless", "source": FENCED_PUT, "params": { "doc": v } },
                "upsert": v,
            }),
        )?;
        Ok(resp["result"].as_str() != Some("noop"))
    }

    /// One status snapshot (see the module docs for the request plan).
    pub fn status_snapshot(&self, q: &StatusQuery) -> Result<ServerSnapshot> {
        let before = self.requests();
        let started = Instant::now();
        let states = self.status_states()?;
        let latency_ms = started.elapsed().as_millis() as u64;
        let mut pairs = Self::pairs_of(&states);
        let mut retried = Vec::new();
        let mut inconsistent = Vec::new();
        let (published, errors) = if q.published {
            let mut published = self.published_aggregates(&pairs).map_err(|e| e.to_string());
            let mut errors = self
                .recent_file_errors(&pairs, q.error_limit, q.error_source.as_deref())
                .map_err(|e| e.to_string());
            // A publication while reading: re-read the changed sources once.
            if published.is_ok() {
                let now_pairs = Self::pairs_of(&self.scan_state_docs(
                    json!({ "exists": { "field": "active_build_id" } }),
                    Some(&["source_id", "active_build_id"]),
                )?);
                let changed: BTreeSet<String> = now_pairs
                    .iter()
                    .filter(|(s, b)| pairs.get(*s).is_some_and(|old| old != *b))
                    .map(|(s, _)| s.clone())
                    .collect();
                if !changed.is_empty() {
                    let sub: BTreeMap<String, String> = now_pairs
                        .iter()
                        .filter(|(s, _)| changed.contains(*s))
                        .map(|(s, b)| (s.clone(), b.clone()))
                        .collect();
                    let again = self.published_aggregates(&sub);
                    let errs =
                        self.recent_file_errors(&sub, q.error_limit, q.error_source.as_deref());
                    let confirm = Self::pairs_of(&self.scan_state_docs(
                        json!({ "terms": { "source_id": changed.iter().collect::<Vec<_>>() } }),
                        Some(&["source_id", "active_build_id"]),
                    )?);
                    for (s, b) in &sub {
                        if confirm.get(s) == Some(b) {
                            retried.push(s.clone());
                        } else {
                            inconsistent.push(s.clone());
                        }
                    }
                    match (&mut published, again) {
                        (Ok(p), Ok(a)) => {
                            for (s, agg) in a {
                                p.insert(s, agg);
                            }
                        }
                        (_, Err(e)) => published = Err(e.to_string()),
                        _ => {}
                    }
                    if let (Ok(list), Ok(e2)) = (&mut errors, errs) {
                        list.retain(|e| !changed.contains(&e.source_id));
                        list.extend(e2);
                    }
                    for (s, b) in sub {
                        pairs.insert(s, b);
                    }
                }
            }
            // Relationship counts come from each source's visible graph
            // generation, never from a base build (graph records are not
            // base-scoped); a missing or stale graph counts zero.
            if let Ok(p) = &mut published {
                let graphs = Self::graph_pairs_of(&states, &pairs);
                for agg in p.values_mut() {
                    agg.relationships = 0;
                    agg.links = 0;
                }
                match self.graph_aggregates(&graphs) {
                    Ok(counts) => {
                        for (s, (edges, links)) in counts {
                            if let Some(agg) = p.get_mut(&s) {
                                agg.relationships = edges;
                                agg.links = links;
                            }
                        }
                    }
                    Err(e) => published = Err(e.to_string()),
                }
            }
            (Some(published), Some(errors))
        } else {
            (None, None)
        };
        let runtime = self.runtime_docs().map_err(|e| e.to_string());
        let cluster = self.cluster_status().map_err(|e| e.to_string());
        Ok(ServerSnapshot {
            states,
            pairs,
            published,
            errors,
            runtime,
            cluster,
            retried,
            inconsistent,
            requests: self.requests() - before,
            latency_ms,
        })
    }
}
