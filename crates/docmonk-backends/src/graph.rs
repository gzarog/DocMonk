//! Server-side relationship graph generations.
//!
//! The base index and the relationship graph are published independently.
//! A graph generation is a generation of the source's graph lineage
//! ([`crate::generations`]): the `edge`/`link` records of the
//! relationships index visible at it (`build_id` names the generation id
//! that wrote a record), plus a record in the source-state document under
//! `versions.graph` (the opaque `versions` object, so the strict mappings
//! are unchanged):
//!
//! ```json
//! { "state": "ready", "generation": "<id>", "base_build_id": "<base>",
//!   "digest": "...", "input_digest": "...", "derivation": "...",
//!   "manifest_version": 1, "file_keys": {...}, "file_digests": {...},
//!   "published_at": "...", "retired": [...] }
//! ```
//!
//! `input_digest`, `derivation` and `file_keys` are the generation's
//! dependency manifest, stored on the authoritative backend so a writer
//! that lost its staging cache (or another host) plans from it and adopts
//! the generation's records ([`ServerBackend::graph_records`]) instead of
//! deriving everything. A new base with identical inputs is handled by
//! [`ServerBackend::rebind_graph`]: one fenced compare-and-swap, no record
//! written.
//!
//! [`ServerBackend::publish_graph`] writes a generation invisibly (files
//! whose graph records did not change since the previous generation are
//! not touched; only changed files' records are bulk-written and the
//! records they replace are closed), then
//! promotes it with one compare-and-swap of the state document that is
//! fenced by the writer lease **and** by the expected active base build: a
//! graph derived from a superseded base is never promoted. Readers
//! ([`ServerBackend::visible_graph`]) only use a generation that is `ready`
//! and whose base is the source's active build. Base publication never
//! writes graph records, and graph publication never touches the base build
//! pointer or base records.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use docmonk_storage::knowledge::ProjectStore;

use crate::backend::{check_lease, EdgeDoc, Lease, LinkDoc, ServerBackend, SourceState};
use crate::error::{BackendError, Result};
use crate::generations::{Line, Lineage};
use crate::schema::IndexKind;

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Key of the graph record inside the state document's `versions`.
pub const GRAPH_KEY: &str = "graph";
/// Key of the cross-source dependency manifest inside `versions`.
pub const EXTERNAL_KEY: &str = "graph_external";

/// The graph record of one source (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ServerGraph {
    pub state: String,
    pub generation: Option<String>,
    pub base_build_id: Option<String>,
    pub digest: Option<String>,
    /// Semantic digest of the inputs the promoted generation was derived
    /// from (see `docmonk_storage::graph::graph_input_digest`).
    pub input_digest: Option<String>,
    /// Graph derivation identity of the promoted generation.
    pub derivation: Option<String>,
    /// Layout version of the persisted manifest (`file_keys`).
    pub manifest_version: u64,
    pub pending: Option<String>,
    pub published_at: Option<String>,
    pub last_error: Option<String>,
    pub stale_reason: Option<String>,
    /// `file_id -> digest` of the promoted generation's records, used to
    /// copy unchanged files' records forward.
    #[serde(skip)]
    pub file_digests: BTreeMap<String, String>,
    /// The generation's dependency manifest: `file_id -> semantic file
    /// key` of the inputs it was derived from. Persisted with the
    /// generation so graph planning survives the loss of a host's staging
    /// cache or a change of writer host.
    #[serde(skip)]
    pub file_keys: BTreeMap<String, String>,
    /// Retired generations still inside the reader grace window.
    #[serde(skip)]
    pub retired: Vec<String>,
}

impl ServerGraph {
    pub fn from_state(state: &SourceState) -> Self {
        let g = state
            .doc
            .get("versions")
            .and_then(|v| v.get(GRAPH_KEY))
            .cloned()
            .unwrap_or(Value::Null);
        let s = |k: &str| g.get(k).and_then(Value::as_str).map(str::to_owned);
        Self {
            state: s("state").unwrap_or_else(|| "pending".into()),
            generation: s("generation"),
            base_build_id: s("base_build_id"),
            digest: s("digest"),
            input_digest: s("input_digest"),
            derivation: s("derivation"),
            manifest_version: g
                .get("manifest_version")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            pending: s("pending"),
            published_at: s("published_at"),
            last_error: s("last_error"),
            stale_reason: s("stale_reason"),
            file_digests: g
                .get("file_digests")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                        .collect()
                })
                .unwrap_or_default(),
            file_keys: g
                .get("file_keys")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                        .collect()
                })
                .unwrap_or_default(),
            retired: g
                .get("retired")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|r| r["generation"].as_str().map(str::to_owned))
                .collect(),
        }
    }

    /// The effective state against the source's active base build.
    pub fn effective_state(&self, active: Option<&str>) -> &str {
        if self.state == "ready" && self.base_build_id.as_deref() != active {
            "stale"
        } else {
            &self.state
        }
    }

    /// The generation readers may use for `active`, if any.
    pub fn visible_generation(&self, active: Option<&str>) -> Option<&str> {
        (self.state == "ready" && active.is_some() && self.base_build_id.as_deref() == active)
            .then_some(self.generation.as_deref())
            .flatten()
    }

    /// The generation graph readers of `active` are served and whether it
    /// is stale. Strict (`graph.require_current`): only the current one
    /// ([`Self::visible_generation`]). Otherwise the last promoted
    /// generation stays readable while the next is built (a base was
    /// republished, a graph build failed or is running), flagged stale; a
    /// disabled or never-built graph is not served.
    pub fn served_generation(
        &self,
        active: Option<&str>,
        require_current: bool,
    ) -> Option<(&str, bool)> {
        if let Some(g) = self.visible_generation(active) {
            return Some((g, false));
        }
        if require_current
            || active.is_none()
            || matches!(self.state.as_str(), "disabled" | "pending")
        {
            return None;
        }
        self.generation.as_deref().map(|g| (g, true))
    }
}

/// What to publish.
pub struct GraphPublishInput<'a> {
    pub source_id: &'a str,
    pub store: &'a ProjectStore,
    /// The staged build whose graph rows are published.
    pub local_build: &'a str,
    /// The server's active base build the graph was derived from.
    pub base_build_id: &'a str,
    pub lease: Option<&'a Lease>,
    /// The new graph generation id.
    pub generation: &'a str,
    /// Semantic digest of the staged graph's inputs, recorded with the
    /// generation so an unchanged source is recognized without staging.
    pub input_digest: Option<&'a str>,
    /// The staged graph's dependency manifest (see [`ServerGraph::file_keys`]).
    pub manifest: Option<GraphManifest<'a>>,
    /// Called when publication enters a phase (`staging`, `closing`,
    /// `writing`, `promoting`).
    pub phase: Option<&'a (dyn Fn(&'static str) + 'a)>,
}

/// The persisted part of a graph's dependency manifest.
#[derive(Clone, Copy)]
pub struct GraphManifest<'a> {
    pub derivation: &'a str,
    pub version: u64,
    pub file_keys: &'a BTreeMap<String, String>,
}

/// Records of one visible generation, as the staging store stores them.
#[derive(Debug, Default)]
pub struct GraphRecords {
    pub edges: Vec<docmonk_storage::knowledge::RelationshipRow>,
    pub links: Vec<docmonk_storage::knowledge::LinkRow>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct GraphPublishReport {
    pub generation: Option<String>,
    /// `false` when the server already had exactly this graph.
    pub published: bool,
    /// The visible generation was rebound to the new base without writing
    /// any record (identical graph inputs).
    pub rebound: bool,
    /// Edge and link records adopted from the server into a staging store
    /// that had lost its graph (cache deletion or a new writer host).
    pub records_hydrated: usize,
    pub edges_written: usize,
    pub links_written: usize,
    /// Files whose graph records were left untouched (the previous
    /// generation had exactly these records).
    pub files_kept: usize,
    /// Files whose graph records were written.
    pub files_written: usize,
    /// Previous records closed (changed or removed files only).
    pub records_closed: u64,
    pub seconds: f64,
    /// Per-phase wall-clock seconds and request counts.
    pub timings: PublishTimings,
    pub counters: PublishCounters,
}

/// Wall-clock seconds per phase of one graph publication.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PublishTimings {
    /// Reading the staged graph from the staging store and digesting it.
    pub staging_seconds: f64,
    /// Closing the replaced files' previous records.
    pub close_seconds: f64,
    /// Bulk-writing changed files' records.
    pub bulk_seconds: f64,
    pub refresh_seconds: f64,
    /// The fenced compare-and-swap that makes the generation visible.
    pub promotion_seconds: f64,
}

/// Work counted during one graph publication.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct PublishCounters {
    /// SQL statements staging issued against the staging store.
    pub staging_sql_statements: u64,
    /// HTTP requests this publication sent (and their payload bytes).
    pub http_requests: u64,
    pub http_bytes_sent: u64,
    /// `_update_by_query` requests closing replaced records.
    pub close_requests: u64,
    pub bulk_requests: u64,
    pub staged_files: usize,
    pub staged_edges: usize,
    pub staged_links: usize,
}

impl From<docmonk_storage::StorageError> for BackendError {
    fn from(e: docmonk_storage::StorageError) -> Self {
        BackendError::Invalid(format!("staging store: {e}"))
    }
}

/// One file's graph records (edges recorded by the file, links into its
/// documents) and their digest.
#[derive(Default)]
struct FileGraph {
    edges: Vec<EdgeDoc>,
    links: Vec<LinkDoc>,
    digest: String,
}

impl FileGraph {
    /// Orders the records by id and digests them (the digest the previous
    /// generation's `file_digests` are compared with).
    fn seal(&mut self) {
        self.edges
            .sort_by(|a, b| a.relationship_id.cmp(&b.relationship_id));
        self.links
            .sort_by(|a, b| a.relationship_id.cmp(&b.relationship_id));
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(&self.edges).unwrap_or_default());
        h.update([0]);
        h.update(serde_json::to_vec(&self.links).unwrap_or_default());
        self.digest = hex(h);
    }
}

/// One staged file: its record digest and counts.
struct StagedFile {
    digest: String,
    edges: usize,
    links: usize,
}

/// The staged graph by file, plus the overall digest (which also covers
/// the base build, so a new base always gets its own generation).
struct StagedGraph {
    files: BTreeMap<String, StagedFile>,
    digest: String,
    /// Records of the files to write, kept from the digest pass while they
    /// fit in [`STAGE_RETAIN_RECORDS`]; `None` means they are read from the
    /// staging store again when written.
    retained: Option<BTreeMap<String, FileGraph>>,
}

/// Most records kept in memory between digesting the staged graph and
/// writing its changed files. A larger write set (a first publication of a
/// large source) is streamed from the staging store a second time, so
/// memory stays bounded by this budget plus the largest file.
const STAGE_RETAIN_RECORDS: usize = 50_000;

/// What both passes over the staging store share: file paths and the
/// links grouped by the file of their document (links are few next to
/// edges; edges are streamed).
struct StageSource<'a> {
    store: &'a ProjectStore,
    build: &'a str,
    paths: HashMap<String, String>,
    links: BTreeMap<String, Vec<LinkDoc>>,
}

impl<'a> StageSource<'a> {
    fn load(store: &'a ProjectStore, build: &'a str) -> Result<Self> {
        let paths: HashMap<String, String> = store
            .files(build)?
            .into_iter()
            .map(|f| (f.id, f.rel_path))
            .collect();
        let doc_file: HashMap<String, String> = store
            .documents(build)?
            .into_iter()
            .map(|d| (d.id, d.file_id))
            .collect();
        let mut links: BTreeMap<String, Vec<LinkDoc>> = BTreeMap::new();
        for l in store.links(build)? {
            let file_id = doc_file.get(&l.document_id).cloned().unwrap_or_default();
            let rel_path = paths.get(&file_id).cloned().unwrap_or_default();
            links.entry(file_id.clone()).or_default().push(LinkDoc {
                relationship_id: l.id,
                relationship_type: l.link_type,
                entity_id: l.entity_id,
                document_id: l.document_id,
                chunk_id: l.chunk_id,
                file_id,
                rel_path,
                resolver: l.resolver,
                confidence: l.confidence,
                evidence: l.evidence,
            });
        }
        Ok(Self {
            store,
            build,
            paths,
            links,
        })
    }

    /// Every staged file's sealed records, in one ordered pass over the
    /// build's relationships (one statement, never one per file). Edges
    /// recorded under a file the build does not list are not published.
    fn each_file(&self, mut each: impl FnMut(String, FileGraph) -> Result<()>) -> Result<()> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        self.store
            .for_each_file_relationships(self.build, |file_id, rows| {
                let Some(rel_path) = self.paths.get(&file_id) else {
                    return Ok(());
                };
                let mut g = FileGraph {
                    edges: rows
                        .into_iter()
                        .map(|r| EdgeDoc {
                            relationship_id: r.id,
                            file_id: file_id.clone(),
                            rel_path: rel_path.clone(),
                            relationship_type: r.relationship_type,
                            source_entity_id: r.source_entity_id,
                            target_entity_id: r.target_entity_id,
                            target_symbol: r.target_symbol,
                            resolver: r.resolver,
                            confidence: r.confidence,
                            source_location: r.source_location,
                            evidence: r.evidence,
                            reference_text: r.reference_text,
                        })
                        .collect(),
                    links: self.links.get(&file_id).cloned().unwrap_or_default(),
                    digest: String::new(),
                };
                g.seal();
                seen.insert(file_id.clone());
                each(file_id, g)
            })?;
        for (file_id, links) in &self.links {
            if seen.contains(file_id) {
                continue;
            }
            let mut g = FileGraph {
                links: links.clone(),
                ..FileGraph::default()
            };
            g.seal();
            each(file_id.clone(), g)?;
        }
        Ok(())
    }
}

/// Digests the staged graph. `writes` says which files will be written
/// (their digest differs from the visible generation's); their records are
/// retained while they fit the budget.
fn stage(
    src: &StageSource<'_>,
    base: &str,
    writes: impl Fn(&str, &str) -> bool,
) -> Result<StagedGraph> {
    stage_within(src, base, writes, STAGE_RETAIN_RECORDS)
}

fn stage_within(
    src: &StageSource<'_>,
    base: &str,
    writes: impl Fn(&str, &str) -> bool,
    budget: usize,
) -> Result<StagedGraph> {
    let mut files: BTreeMap<String, StagedFile> = BTreeMap::new();
    let mut retained = Some(BTreeMap::new());
    let mut kept = 0;
    src.each_file(|file_id, g| {
        files.insert(
            file_id.clone(),
            StagedFile {
                digest: g.digest.clone(),
                edges: g.edges.len(),
                links: g.links.len(),
            },
        );
        if writes(&file_id, &g.digest) {
            kept += g.edges.len() + g.links.len();
            if kept > budget {
                retained = None;
            }
            if let Some(r) = retained.as_mut() {
                r.insert(file_id, g);
            }
        }
        Ok(())
    })?;
    let mut overall = Sha256::new();
    overall.update(base.as_bytes());
    for (id, f) in &files {
        overall.update(id.as_bytes());
        overall.update([0]);
        overall.update(f.digest.as_bytes());
    }
    Ok(StagedGraph {
        files,
        digest: hex(overall),
        retained,
    })
}

/// Whether the current generation holds exactly the staged records: the
/// same files, each with the same record digest, and nothing pending.
fn records_identical(current: &ServerGraph, staged: &StagedGraph) -> bool {
    current.generation.is_some()
        && current.pending.is_none()
        && matches!(current.state.as_str(), "ready" | "stale")
        && current.file_digests.len() == staged.files.len()
        && staged
            .files
            .iter()
            .all(|(id, f)| current.file_digests.get(id) == Some(&f.digest))
}

/// Whether a staged file's records equal the visible generation's, so they
/// are kept as they are instead of written.
fn copies_forward(current: &ServerGraph, file_id: &str, digest: &str) -> bool {
    current.generation.is_some()
        && current.file_digests.get(file_id).map(String::as_str) == Some(digest)
}

fn hex(h: Sha256) -> String {
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn put_graph(doc: &mut serde_json::Map<String, Value>, graph: Value) {
    let versions = doc.entry("versions").or_insert_with(|| json!({}));
    if !versions.is_object() {
        *versions = json!({});
    }
    if let Some(v) = versions.as_object_mut() {
        v.insert(GRAPH_KEY.into(), graph);
    }
}

fn graph_value(doc: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    doc.get("versions")
        .and_then(|v| v.get(GRAPH_KEY))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

impl ServerBackend {
    /// The source's graph record (`None` for an unknown source).
    pub fn graph(&self, source_id: &str) -> Result<Option<(ServerGraph, Option<String>)>> {
        Ok(self
            .source_state(source_id)?
            .map(|s| (ServerGraph::from_state(&s), s.active_build_id.clone())))
    }

    /// The graph generation readers may use for `base_build_id`.
    /// [`ServerGraph::served_generation`] of the source's graph against
    /// `base_build_id`: `(generation, stale)`.
    pub fn served_graph(
        &self,
        source_id: &str,
        base_build_id: &str,
        require_current: bool,
    ) -> Result<Option<(String, bool)>> {
        Ok(self.source_state(source_id)?.and_then(|s| {
            ServerGraph::from_state(&s)
                .served_generation(Some(base_build_id), require_current)
                .map(|(g, stale)| (g.to_owned(), stale))
        }))
    }

    /// The derivation identity of the graph generation readers of
    /// `base_build_id` are served, when it is the promoted generation
    /// (`None`: no graph served, or an older generation whose derivation
    /// is not recorded).
    pub fn served_graph_derivation(
        &self,
        source_id: &str,
        base_build_id: &str,
        require_current: bool,
    ) -> Result<Option<String>> {
        Ok(self.source_state(source_id)?.and_then(|s| {
            let g = ServerGraph::from_state(&s);
            let promoted = g
                .served_generation(Some(base_build_id), require_current)
                .is_some_and(|(served, _)| g.generation.as_deref() == Some(served));
            if promoted {
                g.derivation
            } else {
                None
            }
        }))
    }

    pub fn visible_graph(&self, source_id: &str, base_build_id: &str) -> Result<Option<String>> {
        Ok(self.source_state(source_id)?.and_then(|s| {
            ServerGraph::from_state(&s)
                .visible_generation(Some(base_build_id))
                .map(str::to_owned)
        }))
    }

    /// Records a graph lifecycle change (`disabled`, `failed`, `stale`)
    /// without touching any record.
    pub fn set_graph_state(
        &self,
        source_id: &str,
        lease: Option<&Lease>,
        state: &str,
        error: Option<&str>,
        stale_reason: Option<&str>,
    ) -> Result<()> {
        self.patch_state(source_id, lease, None, |doc, _| {
            let mut g = graph_value(doc);
            g.insert("state".into(), json!(state));
            if error.is_some() || state != "failed" {
                g.insert("last_error".into(), json!(error));
            }
            g.insert("stale_reason".into(), json!(stale_reason));
            put_graph(doc, Value::Object(g));
            Ok(())
        })
    }

    /// Rebinds the visible generation to a new active base whose graph
    /// inputs are identical (`input_digest`), without writing any record:
    /// one compare-and-swap fenced by the lease, the expected active base
    /// and the expected generation. Returns `false` when the server's graph
    /// is not exactly that generation with that digest (the caller then
    /// publishes normally).
    pub fn rebind_graph(
        &self,
        source_id: &str,
        lease: Option<&Lease>,
        generation: &str,
        base_build_id: &str,
        input_digest: &str,
    ) -> Result<bool> {
        let mut rebound = false;
        self.patch_state(source_id, lease, None, |doc, p| {
            let active = p.and_then(|p| p.active_build_id.as_deref());
            if active != Some(base_build_id) {
                return Err(BackendError::Conflict(format!(
                    "base build {base_build_id} of {source_id} was superseded; the graph was not rebound"
                )));
            }
            let mut g = graph_value(doc);
            let matches = |k: &str, v: &str| g.get(k).and_then(Value::as_str) == Some(v);
            if !(matches("generation", generation)
                && matches("input_digest", input_digest)
                && g.get("pending").is_none_or(Value::is_null)
                && matches!(
                    g.get("state").and_then(Value::as_str),
                    Some("ready" | "stale")
                ))
            {
                return Ok(());
            }
            g.insert("state".into(), json!("ready"));
            g.insert("base_build_id".into(), json!(base_build_id));
            g.insert("last_error".into(), Value::Null);
            g.insert("stale_reason".into(), Value::Null);
            g.insert("published_at".into(), json!(crate::backend::now()));
            put_graph(doc, Value::Object(g));
            rebound = true;
            Ok(())
        })?;
        Ok(rebound)
    }

    /// The source's cross-source dependency manifest as last persisted on
    /// the server (`versions.graph_external`; opaque JSON).
    pub fn graph_external(&self, source_id: &str) -> Result<Option<Value>> {
        Ok(self.source_state(source_id)?.and_then(|s| {
            s.doc
                .get("versions")
                .and_then(|v| v.get(EXTERNAL_KEY))
                .filter(|v| !v.is_null())
                .cloned()
        }))
    }

    /// Every source whose authoritative dependency manifest still records
    /// producers or bindings, in one scan of the state index (never one
    /// round trip per source): a consumer whose last dependency was removed
    /// is found here and reconciled.
    pub fn graph_external_owners(&self) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for doc in self.scan_states(json!({ "match_all": {} }))? {
            let m = &doc["versions"][EXTERNAL_KEY];
            let non_empty = |k: &str| match &m[k] {
                Value::Object(o) => !o.is_empty(),
                Value::Array(a) => !a.is_empty(),
                _ => false,
            };
            if let (Some(id), true) = (
                doc["source_id"].as_str(),
                non_empty("producers") || non_empty("bindings"),
            ) {
                out.insert(id.to_owned());
            }
        }
        Ok(out)
    }

    /// Persists the source's cross-source dependency manifest on the
    /// authoritative backend (fenced by the writer lease).
    pub fn set_graph_external(
        &self,
        source_id: &str,
        lease: Option<&Lease>,
        manifest: &Value,
    ) -> Result<()> {
        self.patch_state(source_id, lease, None, |doc, _| {
            let versions = doc.entry("versions").or_insert_with(|| json!({}));
            if !versions.is_object() {
                *versions = json!({});
            }
            if let Some(v) = versions.as_object_mut() {
                v.insert(EXTERNAL_KEY.into(), manifest.clone());
            }
            Ok(())
        })
    }

    /// Every edge and link record of a graph generation of `source_id`.
    pub fn graph_records(&self, source_id: &str, generation: &str) -> Result<GraphRecords> {
        self.graph_records_where(source_id, generation, Vec::new())
    }

    /// The unresolved cross-file references (with their reference text)
    /// of a graph generation of `source_id`, by relationship id: the
    /// authoritative inputs of cross-source dependency resolution (F04).
    pub fn graph_unresolved_references(
        &self,
        source_id: &str,
        generation: &str,
    ) -> Result<Vec<docmonk_storage::knowledge::RelationshipRow>> {
        // `reference_text` is stored, not indexed: it is checked here.
        Ok(self
            .graph_records_where(
                source_id,
                generation,
                vec![json!({ "term": { "resolver": "unresolved" } })],
            )?
            .edges
            .into_iter()
            .filter(|r| r.reference_text.is_some())
            .collect())
    }

    fn graph_records_where(
        &self,
        source_id: &str,
        generation: &str,
        extra: Vec<Value>,
    ) -> Result<GraphRecords> {
        let path = format!("/{}/_search", self.index(IndexKind::Relationships));
        let scope = self.scope_of(source_id, generation)?;
        let mut filter = vec![
            json!({ "term": { "source_id": source_id } }),
            scope,
            json!({ "terms": { "record_kind": ["edge", "link"] } }),
        ];
        filter.extend(extra);
        let mut out = GraphRecords::default();
        let mut after: Option<Value> = None;
        let s = |d: &Value, k: &str| d.get(k).and_then(Value::as_str).map(str::to_owned);
        loop {
            let mut body = json!({
                "size": 1000,
                "query": { "bool": { "filter": filter } },
                "sort": [ { "relationship_id": "asc" } ],
                "track_total_hits": false,
            });
            if let Some(a) = &after {
                body["search_after"] = a.clone();
            }
            let v = self.post_json(crate::transport::Method::Post, &path, &body)?;
            let hits = v
                .pointer("/hits/hits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for h in &hits {
                let d = &h["_source"];
                let id = s(d, "relationship_id").unwrap_or_default();
                match d.get("record_kind").and_then(Value::as_str) {
                    Some("edge") => out.edges.push(docmonk_storage::knowledge::RelationshipRow {
                        id,
                        file_id: s(d, "file_id").unwrap_or_default(),
                        relationship_type: s(d, "relationship_type").unwrap_or_default(),
                        source_entity_id: s(d, "source_entity_id").unwrap_or_default(),
                        target_entity_id: s(d, "target_entity_id"),
                        target_symbol: s(d, "target_symbol"),
                        resolver: s(d, "resolver").unwrap_or_default(),
                        confidence: s(d, "confidence").unwrap_or_default(),
                        source_location: s(d, "source_location"),
                        evidence: s(d, "evidence"),
                        reference_text: s(d, "reference_text"),
                    }),
                    Some("link") => out.links.push(docmonk_storage::knowledge::LinkRow {
                        id,
                        link_type: s(d, "relationship_type").unwrap_or_default(),
                        entity_id: s(d, "entity_id").unwrap_or_default(),
                        document_id: s(d, "document_id").unwrap_or_default(),
                        chunk_id: s(d, "chunk_id"),
                        resolver: s(d, "resolver").unwrap_or_default(),
                        confidence: s(d, "confidence").unwrap_or_default(),
                        evidence: s(d, "evidence"),
                    }),
                    _ => {}
                }
            }
            after = hits.last().map(|h| h["sort"].clone());
            if hits.len() < 1000 {
                return Ok(out);
            }
        }
    }

    /// Publishes the staged graph as a new generation (see the module docs).
    pub fn publish_graph(&self, input: &GraphPublishInput<'_>) -> Result<GraphPublishReport> {
        let started = Instant::now();
        let before = self.stats();
        let mut report = GraphPublishReport::default();
        let result = self.publish_graph_inner(input, started, &mut report);
        let after = self.stats();
        report.counters.http_requests = after.requests - before.requests;
        report.counters.http_bytes_sent = after.bytes_sent - before.bytes_sent;
        report.counters.bulk_requests = after.bulk_requests - before.bulk_requests;
        let ubq = |s: &crate::transport::RequestStats| {
            s.by_endpoint
                .get("POST _update_by_query")
                .copied()
                .unwrap_or(0)
        };
        report.counters.close_requests = ubq(&after) - ubq(&before);
        result.map(|()| report)
    }

    fn publish_graph_inner(
        &self,
        input: &GraphPublishInput<'_>,
        started: Instant,
        report: &mut GraphPublishReport,
    ) -> Result<()> {
        let phase = |name: &'static str| {
            if let Some(f) = input.phase {
                f(name);
            }
        };
        let prev = self.source_state(input.source_id)?.ok_or_else(|| {
            BackendError::NotFound(format!("no such source: {}", input.source_id))
        })?;
        check_lease(&prev, input.lease)?;
        if prev.active_build_id.as_deref() != Some(input.base_build_id) {
            return Err(BackendError::Conflict(format!(
                "base build {} of {} was superseded; the graph was not published",
                input.base_build_id, input.source_id
            )));
        }
        let current = ServerGraph::from_state(&prev);
        phase("staging");
        let staging = Instant::now();
        let statements = docmonk_storage::stats::sql_statements();
        let src = StageSource::load(input.store, input.local_build)?;
        let staged = stage(&src, input.base_build_id, |id, digest| {
            !copies_forward(&current, id, digest)
        })?;
        report.counters.staging_sql_statements =
            docmonk_storage::stats::sql_statements() - statements;
        report.timings.staging_seconds = staging.elapsed().as_secs_f64();
        report.counters.staged_files = staged.files.len();
        report.counters.staged_edges = staged.files.values().map(|f| f.edges).sum();
        report.counters.staged_links = staged.files.values().map(|f| f.links).sum();
        let digest = staged.digest.clone();
        if current
            .visible_generation(Some(input.base_build_id))
            .is_some()
            && current.digest.as_deref() == Some(digest.as_str())
        {
            report.generation = current.generation;
            report.seconds = started.elapsed().as_secs_f64();
            return Ok(());
        }
        // The current generation already holds exactly these records (a
        // new base whose changes did not change any graph record, e.g. a
        // body edit that moves no line): point it at the new base instead
        // of writing a new generation.
        if let Some(generation) = current.generation.as_deref() {
            if records_identical(&current, &staged) {
                phase("promoting");
                let promoting = Instant::now();
                let repointed = self.repoint_graph(input, &staged, generation)?;
                report.timings.promotion_seconds = promoting.elapsed().as_secs_f64();
                if repointed {
                    report.generation = Some(generation.to_owned());
                    report.published = true;
                    report.rebound = true;
                    report.seconds = started.elapsed().as_secs_f64();
                    return Ok(());
                }
            }
        }
        // Allocate the attempt's generation; an abandoned pending attempt
        // is aborted (its records stay invisible).
        let mut generation = 0;
        let mut aborted = Vec::new();
        let mut abandoned = None;
        self.patch_state(input.source_id, input.lease, None, |doc, _| {
            let mut l = Lineage::of(&Value::Object(doc.clone()), Line::Graph);
            abandoned = l.pending;
            generation = l.begin(input.generation, now_millis());
            aborted = l.aborted_generations();
            l.put(doc, Line::Graph);
            let mut g = graph_value(doc);
            g.insert("state".into(), json!("building"));
            g.insert("pending".into(), json!(input.generation));
            put_graph(doc, Value::Object(g));
            Ok(())
        })?;
        if let Some(g) = abandoned {
            // Housekeeping: the abandoned attempt is already invisible.
            let _ = self.sweep_generation(Line::Graph, input.source_id, g);
        }
        let outcome = self
            .write_graph(input, &src, &staged, &current, generation, &aborted, report)
            .and_then(|()| {
                phase("promoting");
                let promoting = Instant::now();
                let r = self.promote_graph(input, &staged);
                report.timings.promotion_seconds = promoting.elapsed().as_secs_f64();
                r
            });
        match outcome {
            Ok(lineage) => {
                // A failure only leaves unreachable records behind; the
                // next promotion collects them.
                let _ = self.collect_garbage(Line::Graph, input.source_id, input.lease, &lineage);
                report.generation = Some(input.generation.to_owned());
                report.published = true;
                report.seconds = started.elapsed().as_secs_f64();
                Ok(())
            }
            Err(e) => {
                let message = docmonk_telemetry::redact::redact_urls_in_text(&e.to_string());
                let _ = self.patch_state(input.source_id, input.lease, None, |doc, _| {
                    let mut l = Lineage::of(&Value::Object(doc.clone()), Line::Graph);
                    l.abort_pending(generation, now_millis());
                    l.put(doc, Line::Graph);
                    let mut g = graph_value(doc);
                    if g.get("pending").and_then(Value::as_str) == Some(input.generation) {
                        g.insert("pending".into(), Value::Null);
                    }
                    g.insert("state".into(), json!("failed"));
                    g.insert("last_error".into(), json!(message));
                    put_graph(doc, Value::Object(g));
                    Ok(())
                });
                // Housekeeping only (the attempt is already invisible).
                let _ = self.sweep_generation(Line::Graph, input.source_id, generation);
                Err(e)
            }
        }
    }

    /// Writes the pending generation `generation`: files whose records
    /// equal the previous generation's are left untouched; the others are
    /// written and the records they replace (and removed files' records)
    /// are closed.
    #[allow(clippy::too_many_arguments)]
    fn write_graph(
        &self,
        input: &GraphPublishInput<'_>,
        src: &StageSource<'_>,
        staged: &StagedGraph,
        current: &ServerGraph,
        generation: i64,
        aborted: &[i64],
        report: &mut GraphPublishReport,
    ) -> Result<()> {
        let (keep, write): (Vec<_>, Vec<_>) = staged
            .files
            .iter()
            .partition(|(id, f)| copies_forward(current, id, &f.digest));
        report.files_kept = keep.len();
        report.files_written = write.len();
        if let Some(f) = input.phase {
            f("writing");
        }
        let writing = Instant::now();
        let mut w = self.build_writer(input.source_id, input.generation, generation);
        let mut put = |g: &FileGraph, report: &mut GraphPublishReport| -> Result<()> {
            for e in &g.edges {
                w.edge(e)?;
                report.edges_written += 1;
            }
            for l in &g.links {
                w.link(l)?;
                report.links_written += 1;
            }
            Ok(())
        };
        match &staged.retained {
            Some(retained) => {
                for (id, _) in &write {
                    if let Some(g) = retained.get(*id) {
                        put(g, report)?;
                    }
                }
            }
            None => {
                // Too many changed records to keep: stream them again. The
                // staging store is this writer's own (held under the
                // source lock), but a changed digest is still refused
                // rather than published under the old one.
                let wanted: BTreeMap<&str, &str> = write
                    .iter()
                    .map(|(id, f)| (id.as_str(), f.digest.as_str()))
                    .collect();
                src.each_file(|file_id, g| {
                    let Some(digest) = wanted.get(file_id.as_str()) else {
                        return Ok(());
                    };
                    if *digest != g.digest {
                        return Err(BackendError::Invalid(format!(
                            "the staged graph of {file_id} changed during publication"
                        )));
                    }
                    put(&g, report)
                })?;
            }
        }
        w.finish()?;
        report.timings.bulk_seconds = writing.elapsed().as_secs_f64();
        if let Some(f) = input.phase {
            f("closing");
        }
        let closing = Instant::now();
        report.records_closed = if current.generation.is_none() {
            self.close_files(Line::Graph, input.source_id, None, generation, aborted)?
        } else {
            let mut replaced: Vec<String> = current
                .file_digests
                .keys()
                .filter(|id| {
                    staged
                        .files
                        .get(*id)
                        .is_none_or(|f| !copies_forward(current, id, &f.digest))
                })
                .cloned()
                .collect();
            replaced.sort();
            if replaced.is_empty() {
                0
            } else {
                self.close_files(
                    Line::Graph,
                    input.source_id,
                    Some(&replaced),
                    generation,
                    aborted,
                )?
            }
        };
        report.timings.close_seconds = closing.elapsed().as_secs_f64();
        let refreshing = Instant::now();
        let r = self.refresh_build_indexes();
        report.timings.refresh_seconds = refreshing.elapsed().as_secs_f64();
        r
    }

    /// Points the visible-or-stale `generation`, whose records equal the
    /// staged graph file for file, at the new base: one compare-and-swap
    /// fenced by the lease, the expected active base and the expected
    /// generation, with no record written. Returns `false` when the
    /// server's graph is no longer that generation (the caller publishes
    /// normally).
    fn repoint_graph(
        &self,
        input: &GraphPublishInput<'_>,
        staged: &StagedGraph,
        generation: &str,
    ) -> Result<bool> {
        let mut repointed = false;
        self.patch_state(input.source_id, input.lease, None, |doc, p| {
            let active = p.and_then(|p| p.active_build_id.as_deref());
            if active != Some(input.base_build_id) {
                return Err(BackendError::Conflict(format!(
                    "base build {} of {} was superseded; the graph was not published",
                    input.base_build_id, input.source_id
                )));
            }
            let mut g = graph_value(doc);
            let unchanged = g.get("generation").and_then(Value::as_str) == Some(generation)
                && g.get("pending").is_none_or(Value::is_null)
                && matches!(
                    g.get("state").and_then(Value::as_str),
                    Some("ready" | "stale")
                );
            if !unchanged {
                return Ok(());
            }
            g.insert("state".into(), json!("ready"));
            g.insert("base_build_id".into(), json!(input.base_build_id));
            g.insert("digest".into(), json!(staged.digest));
            g.insert("input_digest".into(), json!(input.input_digest));
            g.insert(
                "derivation".into(),
                json!(input.manifest.map(|m| m.derivation)),
            );
            g.insert(
                "manifest_version".into(),
                json!(input.manifest.map(|m| m.version)),
            );
            g.insert(
                "file_keys".into(),
                json!(input.manifest.map(|m| m.file_keys)),
            );
            g.insert("published_at".into(), json!(crate::backend::now()));
            g.insert("last_error".into(), Value::Null);
            g.insert("stale_reason".into(), Value::Null);
            put_graph(doc, Value::Object(g));
            repointed = true;
            Ok(())
        })?;
        Ok(repointed)
    }

    /// The fenced compare-and-swap that makes the pending generation the
    /// visible one (the only visibility switch). Returns the lineage after
    /// it, for garbage collection.
    fn promote_graph(
        &self,
        input: &GraphPublishInput<'_>,
        staged: &StagedGraph,
    ) -> Result<Lineage> {
        let digest = staged.digest.as_str();
        let file_digests: serde_json::Map<String, Value> = staged
            .files
            .iter()
            .map(|(id, g)| (id.clone(), json!(g.digest)))
            .collect();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let grace = self.gc_grace().as_millis();
        let mut lineage = Lineage::default();
        self.patch_state(input.source_id, input.lease, None, |doc, p| {
            let active = p.and_then(|p| p.active_build_id.as_deref());
            if active != Some(input.base_build_id) {
                return Err(BackendError::Conflict(format!(
                    "base build {} of {} was superseded during the graph build",
                    input.base_build_id, input.source_id
                )));
            }
            let g = graph_value(doc);
            if g.get("pending").and_then(Value::as_str) != Some(input.generation) {
                return Err(BackendError::Conflict(format!(
                    "graph generation {} is not the pending graph of {}",
                    input.generation, input.source_id
                )));
            }
            let mut retired: Vec<(String, u128)> = g
                .get("retired")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|r| {
                    Some((
                        r["generation"].as_str()?.to_owned(),
                        r["retired_at"].as_str()?.parse().ok()?,
                    ))
                })
                .collect();
            if let Some(old) = g
                .get("generation")
                .and_then(Value::as_str)
                .filter(|o| *o != input.generation)
            {
                retired.push((old.to_owned(), now_ms));
            }
            retired.retain(|(_, at)| grace > 0 && now_ms.saturating_sub(*at) < grace);
            let keep = retired;
            let mut l = Lineage::of(&Value::Object(doc.clone()), Line::Graph);
            if l.publish(input.generation).is_none() {
                return Err(BackendError::Conflict(format!(
                    "graph generation {} of {} is not the pending generation",
                    input.generation, input.source_id
                )));
            }
            let kept: Vec<&str> = keep.iter().map(|(g, _)| g.as_str()).collect();
            l.retain_ids(&kept);
            l.put(doc, Line::Graph);
            lineage = l;
            put_graph(
                doc,
                json!({
                    "state": "ready",
                    "generation": input.generation,
                    "base_build_id": input.base_build_id,
                    "digest": digest,
                    "input_digest": input.input_digest,
                    "derivation": input.manifest.map(|m| m.derivation),
                    "manifest_version": input.manifest.map(|m| m.version),
                    "file_keys": input.manifest.map(|m| m.file_keys),
                    "file_digests": file_digests,
                    "pending": null,
                    "published_at": crate::backend::now(),
                    "last_error": null,
                    "stale_reason": null,
                    "retired": keep.iter().map(|(g, at)| json!({
                        "generation": g, "retired_at": at.to_string()
                    })).collect::<Vec<_>>(),
                }),
            );
            Ok(())
        })?;
        Ok(lineage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(graph: Value, active: Option<&str>) -> SourceState {
        SourceState {
            source_id: "s".into(),
            path: None,
            state: "ready".into(),
            active_build_id: active.map(str::to_owned),
            pending_build_id: None,
            versions: Value::Null,
            doc: json!({ "versions": { "graph": graph } }),
            seq_no: 0,
            primary_term: 1,
        }
    }

    #[test]
    fn only_a_ready_graph_of_the_active_base_is_visible() {
        let st = state(
            json!({"state": "ready", "generation": "g1", "base_build_id": "b1"}),
            Some("b1"),
        );
        let g = ServerGraph::from_state(&st);
        assert_eq!(g.visible_generation(Some("b1")), Some("g1"));
        assert_eq!(g.effective_state(Some("b1")), "ready");
        assert_eq!(g.visible_generation(Some("b2")), None);
        assert_eq!(g.effective_state(Some("b2")), "stale");
        let failed = ServerGraph::from_state(&state(
            json!({"state": "failed", "generation": "g1", "base_build_id": "b1"}),
            Some("b1"),
        ));
        assert_eq!(failed.visible_generation(Some("b1")), None);
        let missing = ServerGraph::from_state(&state(Value::Null, Some("b1")));
        assert_eq!(missing.state, "pending");
        assert_eq!(missing.visible_generation(Some("b1")), None);
    }

    /// A staging store: three files (one without edges), edges recorded
    /// under an unlisted file, a link into a document and a link whose
    /// document is gone.
    fn store(dir: &std::path::Path) -> ProjectStore {
        use docmonk_storage::knowledge::{
            DocumentRow, FileKnowledge, FileRow, LinkRow, RelationshipRow,
        };
        let mut s =
            ProjectStore::open(&docmonk_storage::StorageLayout::at(dir), "p", "s", 8).unwrap();
        let v = docmonk_storage::control::IndexVersions {
            parser_version: "p".into(),
            chunker_version: "c".into(),
            converter_version: "v".into(),
            embedding_model_id: None,
            embedding_text_version: None,
        };
        s.create_build("b", true, &v).unwrap();
        let file = |id: &str, rel: &str| FileRow {
            id: id.into(),
            rel_path: rel.into(),
            kind: "code".into(),
            status: "indexed".into(),
            ..FileRow::default()
        };
        let (a, b, d) = (file("fa", "a.py"), file("fb", "b.py"), file("fd", "d.md"));
        let doc = FileKnowledge {
            documents: vec![DocumentRow {
                id: "doc1".into(),
                file_id: "fd".into(),
                format: "markdown".into(),
                title: None,
                author: None,
                page_count: None,
                is_scanned: false,
                content_hash: None,
                attachment: None,
            }],
            ..FileKnowledge::default()
        };
        let none = FileKnowledge::default();
        s.put_files("b", &[(&a, &none), (&b, &none), (&d, &doc)])
            .unwrap();
        let rows: Vec<RelationshipRow> = (0..40)
            .map(|i| RelationshipRow {
                id: format!("r{:03}", (i * 13) % 40),
                file_id: ["fa", "fb", "zz-unlisted"][i % 3].into(),
                relationship_type: "calls".into(),
                source_entity_id: format!("e{i}"),
                target_symbol: Some(format!("t{i}")),
                resolver: "pending".into(),
                confidence: "medium".into(),
                reference_text: (i % 2 == 0).then(|| format!("t{i}")),
                ..RelationshipRow::default()
            })
            .collect();
        s.put_relationships("b", &rows).unwrap();
        let link = |id: &str, document: &str| LinkRow {
            id: id.into(),
            link_type: "mentioned_in".into(),
            entity_id: "e1".into(),
            document_id: document.into(),
            chunk_id: None,
            resolver: "linker:exact_identifier".into(),
            confidence: "high".into(),
            evidence: Some("x".into()),
        };
        s.put_links("b", &[link("l2", "doc1"), link("l1", "gone")])
            .unwrap();
        s
    }

    /// The staging algorithm before the single pass (one relationship
    /// query per file): the digests every published generation recorded.
    fn per_file_reference(
        s: &ProjectStore,
        build: &str,
        base: &str,
    ) -> (BTreeMap<String, String>, String) {
        let files = s.files(build).unwrap();
        let paths: HashMap<&str, &str> = files
            .iter()
            .map(|f| (f.id.as_str(), f.rel_path.as_str()))
            .collect();
        let docs = s.documents(build).unwrap();
        let doc_file: HashMap<&str, &str> = docs
            .iter()
            .map(|d| (d.id.as_str(), d.file_id.as_str()))
            .collect();
        let mut out: BTreeMap<String, FileGraph> = BTreeMap::new();
        for f in &files {
            for r in s.file_relationships(build, &f.id).unwrap() {
                out.entry(f.id.clone()).or_default().edges.push(EdgeDoc {
                    relationship_id: r.id,
                    file_id: f.id.clone(),
                    rel_path: f.rel_path.clone(),
                    relationship_type: r.relationship_type,
                    source_entity_id: r.source_entity_id,
                    target_entity_id: r.target_entity_id,
                    target_symbol: r.target_symbol,
                    resolver: r.resolver,
                    confidence: r.confidence,
                    source_location: r.source_location,
                    evidence: r.evidence,
                    reference_text: r.reference_text,
                });
            }
        }
        for l in s.links(build).unwrap() {
            let file_id = doc_file
                .get(l.document_id.as_str())
                .copied()
                .unwrap_or_default();
            out.entry(file_id.to_owned())
                .or_default()
                .links
                .push(LinkDoc {
                    relationship_id: l.id,
                    relationship_type: l.link_type,
                    entity_id: l.entity_id,
                    document_id: l.document_id,
                    chunk_id: l.chunk_id,
                    file_id: file_id.to_owned(),
                    rel_path: paths.get(file_id).copied().unwrap_or_default().to_owned(),
                    resolver: l.resolver,
                    confidence: l.confidence,
                    evidence: l.evidence,
                });
        }
        let mut overall = Sha256::new();
        overall.update(base.as_bytes());
        let mut digests = BTreeMap::new();
        for (id, g) in &mut out {
            g.seal();
            overall.update(id.as_bytes());
            overall.update([0]);
            overall.update(g.digest.as_bytes());
            digests.insert(id.clone(), g.digest.clone());
        }
        (digests, hex(overall))
    }

    fn records(g: &BTreeMap<String, FileGraph>) -> Vec<String> {
        g.values()
            .flat_map(|f| {
                f.edges
                    .iter()
                    .map(|e| serde_json::to_string(e).unwrap())
                    .chain(f.links.iter().map(|l| serde_json::to_string(l).unwrap()))
            })
            .collect()
    }

    #[test]
    fn single_pass_staging_matches_the_per_file_digests_in_four_statements() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let (want, want_overall) = per_file_reference(&s, "b", "base-1");
        let before = docmonk_storage::stats::sql_statements();
        let src = StageSource::load(&s, "b").unwrap();
        let staged = stage(&src, "base-1", |_, _| true).unwrap();
        assert_eq!(
            docmonk_storage::stats::sql_statements() - before,
            4,
            "files, documents, links and one relationship pass, however many files"
        );
        let got: BTreeMap<String, String> = staged
            .files
            .iter()
            .map(|(id, f)| (id.clone(), f.digest.clone()))
            .collect();
        assert_eq!(got, want);
        assert_eq!(staged.digest, want_overall);
        assert!(
            !got.contains_key("zz-unlisted"),
            "unlisted files are not published"
        );
        assert!(
            got.contains_key("fd") && got.contains_key(""),
            "links-only files are staged"
        );
        let edges: usize = staged.files.values().map(|f| f.edges).sum();
        assert_eq!(edges, 27, "{:?}", staged.files.keys());
    }

    #[test]
    fn a_write_set_over_the_budget_is_streamed_again_with_the_same_records() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let src = StageSource::load(&s, "b").unwrap();
        let kept = stage_within(&src, "base", |_, _| true, usize::MAX).unwrap();
        let retained = kept.retained.expect("fits the budget");
        let only_fb = stage_within(&src, "base", |id, _| id == "fb", usize::MAX).unwrap();
        assert_eq!(
            only_fb.retained.unwrap().keys().collect::<Vec<_>>(),
            ["fb"],
            "only files to write are kept"
        );
        let over = stage_within(&src, "base", |_, _| true, 5).unwrap();
        assert!(over.retained.is_none());
        assert_eq!(over.digest, kept.digest);
        let mut streamed = BTreeMap::new();
        src.each_file(|id, g| {
            streamed.insert(id, g);
            Ok(())
        })
        .unwrap();
        assert_eq!(records(&streamed), records(&retained));
    }

    #[test]
    fn graph_record_lives_in_versions() {
        let mut doc = serde_json::Map::new();
        put_graph(&mut doc, json!({"state": "disabled"}));
        assert_eq!(doc["versions"]["graph"]["state"], "disabled");
        assert_eq!(graph_value(&doc)["state"], "disabled");
    }
}
