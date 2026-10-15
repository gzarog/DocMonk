//! Queries over the indexed sources: symbol and graph lookups, impact,
//! explore, lexical/semantic/hybrid search and manual links. Retrieval
//! reports paths relative to each source root; results here carry
//! absolute paths.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_retrieval::graph::{self, Direction, SourceMatch, TraversalEdge};
use docmonk_retrieval::lexical::{self, SearchResult};
use docmonk_retrieval::{classify, context, explore, hybrid, Corpus};
use docmonk_storage::control::SourceRecord;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

use docmonk_backends::reader::ServerReader;
use docmonk_storage::read::KnowledgeRead;

use crate::backend::Backend;
use crate::sources::control_plane;
use crate::{db, load};

/// A source's knowledge reader: the local store, or a server reader pinned
/// to the request's snapshot.
pub enum Reader {
    Local(PooledStore),
    Server(ServerReader),
}

/// A local store borrowed from the process's connection pool; dropping it
/// returns the connection for the next request of the same source and
/// active build (so the schema check and pragmas run once per connection).
pub struct PooledStore {
    key: (String, String),
    store: Option<ProjectStore>,
}

type StorePool =
    std::sync::Mutex<HashMap<(String, String), Vec<(ProjectStore, std::time::Instant)>>>;

/// Idle connections kept per `(source, build)`.
const POOL_PER_KEY: usize = 4;
/// An idle connection older than this is closed instead of reused, so a
/// pooled handle never blocks another process from removing a source for
/// long (Windows refuses to delete open files).
const POOL_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

static POOL_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Keeps read connections open between requests of this process (MCP
/// server, Admin UI). Off by default: a one-shot CLI process gains
/// nothing from it.
pub fn enable_store_pool() {
    POOL_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Closes every pooled connection (before an operation that deletes or
/// replaces knowledge databases, which open handles would block on
/// Windows).
pub fn release_pooled_stores() {
    store_pool()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

fn pool_enabled() -> bool {
    POOL_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

fn store_pool() -> &'static StorePool {
    static POOL: std::sync::OnceLock<StorePool> = std::sync::OnceLock::new();
    POOL.get_or_init(Default::default)
}

impl PooledStore {
    /// A pooled connection to `source_id`'s store for `build`, else a new
    /// one from `open`. Connections pooled for another build of the source
    /// are dropped (the active build changed).
    fn take(
        source_id: &str,
        build: &str,
        open: impl FnOnce() -> Result<ProjectStore, DocMonkError>,
    ) -> Result<Self, DocMonkError> {
        let key = (source_id.to_owned(), build.to_owned());
        let pooled = if !pool_enabled() {
            None
        } else {
            let mut pool = store_pool().lock().unwrap_or_else(|p| p.into_inner());
            pool.retain(|(s, b), _| s != source_id || b == build);
            for idle in pool.values_mut() {
                idle.retain(|(_, at)| at.elapsed() < POOL_IDLE);
            }
            pool.get_mut(&key).and_then(Vec::pop).map(|(s, _)| s)
        };
        let store = match pooled {
            Some(s) => s,
            None => open()?,
        };
        Ok(Self {
            key,
            store: Some(store),
        })
    }
}

impl std::ops::Deref for PooledStore {
    type Target = ProjectStore;
    fn deref(&self) -> &ProjectStore {
        self.store.as_ref().expect("pooled store")
    }
}

impl std::ops::DerefMut for PooledStore {
    fn deref_mut(&mut self) -> &mut ProjectStore {
        self.store.as_mut().expect("pooled store")
    }
}

impl Drop for PooledStore {
    fn drop(&mut self) {
        let Some(store) = self.store.take() else {
            return;
        };
        // Never pool a connection left inside a transaction.
        if !pool_enabled() || store.in_session() {
            return;
        }
        let mut pool = store_pool().lock().unwrap_or_else(|p| p.into_inner());
        let idle = pool.entry(self.key.clone()).or_default();
        if idle.len() < POOL_PER_KEY {
            idle.push((store, std::time::Instant::now()));
        }
    }
}

impl Reader {
    pub fn read(&self) -> &dyn KnowledgeRead {
        match self {
            Reader::Local(s) => &**s,
            Reader::Server(s) => s,
        }
    }

    /// The local store; in server mode a typed "local only" error.
    pub fn local(&self) -> Result<&ProjectStore, DocMonkError> {
        match self {
            Reader::Local(s) => Ok(s),
            Reader::Server(_) => Err(crate::backend::local_only("this operation")),
        }
    }

    pub fn local_mut(&mut self) -> Result<&mut ProjectStore, DocMonkError> {
        match self {
            Reader::Local(s) => Ok(s),
            Reader::Server(_) => Err(crate::backend::local_only("this operation")),
        }
    }

    pub fn server(&self) -> Option<&ServerReader> {
        match self {
            Reader::Server(s) => Some(s),
            Reader::Local(_) => None,
        }
    }
}

/// One indexed source: its record, reader and active build (the
/// request's snapshot of it).
pub struct Opened {
    pub source: SourceRecord,
    pub store: Reader,
    pub build: String,
}

/// Every source with a published build (optionally one source), read from
/// the configured backend. In server mode the catalog and the active
/// builds come from the server, read once: that map is the request's
/// snapshot, so every later read of this request sees one consistent set
/// of builds. Nothing local is opened.
pub fn open_sources(home: &Home, only: Option<&str>) -> Result<Vec<Opened>, DocMonkError> {
    match crate::backend::open(home)? {
        Backend::Local => open_local(home, only),
        Backend::Server(server) => open_server(server, only),
    }
}

fn open_local(home: &Home, only: Option<&str>) -> Result<Vec<Opened>, DocMonkError> {
    let cfg = load(home)?;
    let cp = control_plane(home)?;
    let sources = match only {
        Some(id) => vec![cp
            .get_source(id)
            .map_err(db)?
            .ok_or_else(|| DocMonkError::usage(format!("no such source: {id}")))?],
        None => cp.list_sources(false).map_err(db)?,
    };
    let layout = StorageLayout::new(home);
    let mut out = Vec::new();
    for source in sources {
        let Some(build) = cp.state(&source.id).map_err(db)?.active_build_id else {
            continue;
        };
        let store = PooledStore::take(&source.id, &build, || {
            ProjectStore::open(
                &layout,
                &project_id_for_canonical(&source.path),
                &source.id,
                cfg.runtime.sqlite_cache_size_mb,
            )
            .map_err(db)
        })?;
        out.push(Opened {
            source,
            store: Reader::Local(store),
            build,
        });
    }
    Ok(out)
}

fn open_server(
    server: std::sync::Arc<docmonk_backends::ServerBackend>,
    only: Option<&str>,
) -> Result<Vec<Opened>, DocMonkError> {
    let catalog = crate::sources::Catalog::Server(server.clone());
    let sources = match only {
        Some(id) => vec![catalog.get(id)?],
        None => catalog.list(false)?,
    };
    let active = server.active_builds().map_err(crate::backend::server_err)?;
    Ok(sources
        .into_iter()
        .filter_map(|source| {
            let build = active.get(&source.id)?.clone();
            Some(Opened {
                store: Reader::Server(ServerReader::new(server.clone(), &source.id)),
                source,
                build,
            })
        })
        .collect())
}

pub fn corpora(opened: &[Opened]) -> Vec<Corpus<'_>> {
    opened
        .iter()
        .map(|o| Corpus {
            store: o.store.read(),
            build_id: &o.build,
        })
        .collect()
}

pub fn search_err(e: docmonk_retrieval::SearchError) -> DocMonkError {
    DocMonkError::new(ErrorKind::Database, e.to_string())
}

fn root_of<'a>(opened: &'a [Opened], source_id: &str) -> Option<&'a str> {
    opened
        .iter()
        .find(|o| o.source.id == source_id)
        .map(|o| o.source.path.as_str())
}

fn join(root: &str, rel: &str) -> String {
    if rel.is_empty() || Path::new(rel).is_absolute() {
        return rel.to_owned();
    }
    Path::new(root).join(rel).to_string_lossy().into_owned()
}

/// `rel:line` (or a bare path) under `root`.
pub fn join_location(root: &str, loc: &str) -> String {
    match loc.rsplit_once(':') {
        Some((p, line)) if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() => {
            format!("{}:{line}", join(root, p))
        }
        _ => join(root, loc),
    }
}

pub fn absolutize_results(results: &mut [SearchResult], opened: &[Opened]) {
    for r in results {
        let Some(root) = root_of(opened, &r.source_id) else {
            continue;
        };
        if r.kind == "path" && r.title == r.path {
            r.title = join(root, &r.title);
        }
        r.path = join(root, &r.path);
    }
}

pub fn absolutize_hits(hits: &mut [hybrid::SourcedHit], opened: &[Opened]) {
    for h in hits {
        if let Some(root) = root_of(opened, &h.source_id) {
            h.hit.path = join(root, &h.hit.path);
        }
    }
}

/// Rewrites every `path` / `paths` / `source_location` string in an
/// `explore` or `impact` payload. An object with a `source_id` uses that
/// source. Otherwise the first source where the file exists is used,
/// falling back to the first source.
pub fn absolutize_json(v: &mut Value, opened: &[Opened], inherited: Option<&str>) {
    let pick = |rel: &str, sid: Option<&str>| -> Option<String> {
        if let Some(root) = sid.and_then(|s| root_of(opened, s)) {
            return Some(root.to_owned());
        }
        let file = rel.rsplit_once(':').map_or(rel, |(p, _)| p);
        opened
            .iter()
            .find(|o| Path::new(&o.source.path).join(file).exists())
            .or_else(|| opened.first())
            .map(|o| o.source.path.clone())
    };
    match v {
        Value::Object(map) => {
            let own = map
                .get("source_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let sid = own.as_deref().or(inherited);
            for (k, child) in map.iter_mut() {
                match (k.as_str(), &mut *child) {
                    ("path", Value::String(p)) => {
                        if let Some(root) = pick(p, sid) {
                            *p = join(&root, p);
                        }
                    }
                    ("source_location", Value::String(p)) => {
                        if let Some(root) = pick(p, sid) {
                            *p = join_location(&root, p);
                        }
                    }
                    ("paths", Value::Array(items)) => {
                        for it in items.iter_mut() {
                            if let Value::String(p) = it {
                                if let Some(root) = pick(p, sid) {
                                    *p = join(&root, p);
                                }
                            }
                        }
                    }
                    _ => absolutize_json(child, opened, sid),
                }
            }
        }
        Value::Array(items) => {
            for it in items {
                absolutize_json(it, opened, inherited);
            }
        }
        _ => {}
    }
}

pub fn match_json(m: &SourceMatch, opened: &[Opened]) -> Value {
    let e = &m.entity;
    json!({
        "entity_id": e.id,
        "kind": e.kind,
        "name": e.name,
        "qualified_name": e.qualified_name,
        "language": e.language,
        "file_id": e.file_id,
        "start_line": e.start_line,
        "end_line": e.end_line,
        "signature": e.signature,
        "source_id": m.source_id,
        "source_path": opened.get(m.corpus).map(|o| o.source.path.clone()),
    })
}

pub fn edge_json(e: &TraversalEdge, opened: &[Opened]) -> Value {
    let r = &e.relationship;
    let root = opened.get(e.corpus).map(|o| o.source.path.as_str());
    json!({
        "depth": e.depth,
        "relationship_type": r.relationship_type,
        "source_entity_id": r.source_entity_id,
        "target_entity_id": r.target_entity_id,
        "target_symbol": r.target_symbol,
        "confidence": r.confidence,
        "resolver": r.resolver,
        "source_location": match (root, &r.source_location) {
            (Some(root), Some(l)) => Some(join_location(root, l)),
            (_, l) => l.clone(),
        },
        "evidence": r.evidence,
    })
}

/// `symbol --json` data.
pub fn symbol_value(opened: &[Opened], name: &str) -> Result<Value, DocMonkError> {
    let matches = graph::find_symbol_matches(&corpora(opened), name).map_err(search_err)?;
    let m: Vec<Value> = matches.iter().map(|m| match_json(m, opened)).collect();
    Ok(json!({"query": name, "matches": m}))
}

/// `callers`/`callees --json` data.
pub fn calls_value(
    opened: &[Opened],
    name: &str,
    direction: Direction,
    max_depth: usize,
    limit: usize,
) -> Result<Value, DocMonkError> {
    traverse_value(
        opened,
        name,
        direction,
        &graph::CALL_TYPES,
        max_depth,
        limit,
    )
}

/// `{query, matches, edges}` of a traversal over `types`.
pub fn traverse_value(
    opened: &[Opened],
    name: &str,
    direction: Direction,
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<Value, DocMonkError> {
    let (matches, edges) =
        graph::traverse_symbol(&corpora(opened), name, direction, types, max_depth, limit)
            .map_err(search_err)?;
    Ok(json!({
        "query": name,
        "matches": matches.iter().map(|m| match_json(m, opened)).collect::<Vec<_>>(),
        "edges": edges.iter().map(|e| edge_json(e, opened)).collect::<Vec<_>>(),
    }))
}

/// `[{source_id, state, generation, visible, stale}]`: how current each
/// source's relationship graph is.
pub fn graph_json(opened: &[Opened]) -> Result<Value, DocMonkError> {
    let mut out = Vec::with_capacity(opened.len());
    for o in opened {
        let f = o.store.read().graph_freshness(&o.build).map_err(db)?;
        out.push(json!({
            "source_id": o.source.id,
            "state": f.state,
            "generation": f.generation,
            "visible": f.visible,
            "stale": f.stale,
        }));
    }
    Ok(Value::Array(out))
}

/// `impact --json` data.
pub fn impact_value(
    opened: &[Opened],
    name: &str,
    max_depth: usize,
    limit: usize,
) -> Result<Value, DocMonkError> {
    let mut payload =
        explore::impact(&corpora(opened), name, max_depth, limit).map_err(search_err)?;
    absolutize_json(&mut payload, opened, None);
    payload["graph"] = graph_json(opened)?;
    Ok(payload)
}

/// The process-wide query embedder (loaded on first use, then reused).
pub fn lazy_embedder(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
) -> std::sync::Arc<docmonk_ml::LazyEmbedder> {
    docmonk_ml::shared_embedder(
        docmonk_ml::embedder::models_root(Some(&home.root().join("models"))),
        docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL,
        cfg.indexing.embedding_batch_size,
    )
}

/// The process-wide cross-encoder, when it can be loaded.
pub fn cross_encoder(home: &Home) -> Option<std::sync::Arc<docmonk_ml::reranker::CrossEncoder>> {
    let root = docmonk_ml::embedder::models_root(Some(&home.models_dir()));
    let spec = docmonk_ml::manifest::DEFAULT_RERANKER_MODEL;
    docmonk_ml::shared_cross_encoder(&root.join(spec.slug), spec)
}

/// `search.ann` as query-time ANN parameters.
pub fn ann_params(cfg: &docmonk_config::DocMonkConfig) -> docmonk_ml::semantic::AnnParams {
    docmonk_ml::semantic::AnnParams {
        ef_search: cfg.search.ann.ef_search.max(1) as usize,
        num_candidates: cfg.search.ann.num_candidates.max(0) as usize,
    }
}

pub fn semantic_json(s: &hybrid::SemanticOutcome) -> Value {
    json!({
        "available": s.available,
        "reason": s.reason,
        "results": s.hits.iter().map(explore::semantic_hit_json).collect::<Vec<_>>(),
    })
}

/// The configured `context.*` budget.
pub fn config_budget(cfg: &docmonk_config::DocMonkConfig) -> explore::Budget {
    explore::Budget {
        max_chars: cfg.context.max_chars.max(0) as usize,
        max_files: cfg.context.max_files.max(0) as usize,
        max_graph_nodes: cfg.context.max_graph_nodes.max(0) as usize,
    }
}

/// `explore --json` data under `budget`.
pub fn explore_value(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    opened: &[Opened],
    query: &str,
    budget: explore::Budget,
) -> Result<Value, DocMonkError> {
    let corp = corpora(opened);
    let plan = explore::plan(query, cfg.search.semantic);
    let semantic = if plan.strategies.contains(&explore::Strategy::Semantic) {
        let lazy = lazy_embedder(home, cfg);
        let embedder = lazy.get().ok().map(|a| a.as_ref());
        Some(
            hybrid::semantic_search_with(
                &corp,
                embedder,
                query,
                lexical::DEFAULT_LIMIT,
                Some(cfg.search.semantic_top_k.max(1) as usize),
                ann_params(cfg),
            )
            .map_err(search_err)?,
        )
    } else {
        None
    };
    let opts = explore::ExploreOptions {
        budget,
        snippet_tokens: cfg.search.output.snippet_max_tokens,
    };
    let mut r = explore::explore(&corp, &plan, semantic.as_ref(), opts).map_err(search_err)?;
    absolutize_json(&mut r, opened, None);
    r["graph"] = graph_json(opened)?;
    Ok(r)
}

/// `{available, reason, results}` of a semantic search, absolute paths.
pub fn semantic_value(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    opened: &[Opened],
    query: &str,
    limit: usize,
) -> Result<Value, DocMonkError> {
    let lazy = lazy_embedder(home, cfg);
    let embedder = lazy.get().ok().map(|a| a.as_ref());
    let mut s = hybrid::semantic_search_with(
        &corpora(opened),
        embedder,
        query,
        limit,
        Some(cfg.search.semantic_top_k.max(1) as usize),
        ann_params(cfg),
    )
    .map_err(search_err)?;
    absolutize_hits(&mut s.hits, opened);
    Ok(semantic_json(&s))
}

/// The plain lexical ranking `search --json` reports as `results`, at
/// most `limit` hits, with absolute paths.
pub fn lexical_value(
    cfg: &docmonk_config::DocMonkConfig,
    opened: &[Opened],
    query: &str,
    limit: usize,
) -> Result<Value, DocMonkError> {
    let mut results = lexical::search(
        &corpora(opened),
        query,
        limit,
        cfg.search.output.snippet_max_tokens,
    )
    .map_err(search_err)?;
    absolutize_results(&mut results, opened);
    Ok(json!({
        "query": query,
        "results": results.iter().map(explore::search_result_json).collect::<Vec<_>>(),
    }))
}

/// `link list` rows for one source. A user link shows its manual link id,
/// which is what `link remove` takes.
pub fn link_rows(
    o: &Opened,
    entity: Option<&str>,
    document: Option<&str>,
) -> Result<Vec<Value>, DocMonkError> {
    if let Some(server) = o.store.server() {
        return server_link_rows(o, server, entity, document);
    }
    let store = o.store.local()?;
    let mut links = store.links(&o.build).map_err(db)?;
    if !store.graph_visible(&o.build).map_err(db)? {
        // Automatic links belong to the relationship graph, which does not
        // match this build; manual links are user data and stay listed.
        links.retain(|l| l.resolver == docmonk_knowledge::manual::USER_RESOLVER);
    }
    if let Some(r) = entity {
        let ids: std::collections::HashSet<String> = match store.entity(&o.build, r).map_err(db)? {
            Some(e) => [e.id].into(),
            None => store
                .entities_named(&o.build, r)
                .map_err(db)?
                .into_iter()
                .map(|e| e.id)
                .collect(),
        };
        links.retain(|l| ids.contains(&l.entity_id));
    } else if let Some(d) = document {
        links.retain(|l| l.document_id == d);
    }
    let manual_ids = docmonk_knowledge::manual::manual_ids(store, &o.build).map_err(db)?;
    let files: HashMap<String, String> = store
        .files(&o.build)
        .map_err(db)?
        .into_iter()
        .map(|f| (f.id, f.rel_path))
        .collect();
    let doc_files: HashMap<String, String> = store
        .documents(&o.build)
        .map_err(db)?
        .into_iter()
        .map(|d| (d.id, d.file_id))
        .collect();
    let mut out = Vec::new();
    for l in links {
        let entity_qn = store
            .entity(&o.build, &l.entity_id)
            .map_err(db)?
            .map(|e| e.qualified_name);
        let document_path = doc_files
            .get(&l.document_id)
            .and_then(|f| files.get(f))
            .map(|rel| {
                Path::new(&o.source.path)
                    .join(rel)
                    .to_string_lossy()
                    .into_owned()
            });
        out.push(json!({
            "link_id": manual_ids.get(&l.id).cloned().unwrap_or_else(|| l.id.clone()),
            "link_type": l.link_type,
            "source_id": o.source.id,
            "entity_id": l.entity_id,
            "entity": entity_qn,
            "document_id": l.document_id,
            "document_path": document_path,
            "section_id": l.chunk_id,
            "resolver": l.resolver,
            "confidence": l.confidence,
            "evidence": l.evidence,
        }));
    }
    Ok(out)
}

fn sdb(e: docmonk_storage::StorageError) -> DocMonkError {
    e.into()
}

/// Entity ids a `--entity` reference names (an id, or a short name).
fn server_entity_ids(
    server: &ServerReader,
    build: &str,
    r: &str,
) -> Result<std::collections::HashSet<String>, DocMonkError> {
    Ok(match server.entity(build, r).map_err(sdb)? {
        Some(e) => [e.id].into(),
        None => server
            .entities_named(build, r)
            .map_err(sdb)?
            .into_iter()
            .map(|e| e.id)
            .collect(),
    })
}

fn server_link_rows(
    o: &Opened,
    server: &ServerReader,
    entity: Option<&str>,
    document: Option<&str>,
) -> Result<Vec<Value>, DocMonkError> {
    let mut links = server.links(&o.build).map_err(sdb)?;
    if let Some(r) = entity {
        let ids = server_entity_ids(server, &o.build, r)?;
        links.retain(|l| ids.contains(&l.entity_id));
    } else if let Some(d) = document {
        links.retain(|l| l.document_id == d);
    }
    let mut out = Vec::new();
    for l in links {
        let entity_qn = server
            .entity(&o.build, &l.entity_id)
            .map_err(sdb)?
            .map(|e| e.qualified_name);
        let document_path = server
            .document_location(&o.build, &l.document_id)
            .map_err(sdb)?
            .map(|(rel, _)| {
                Path::new(&o.source.path)
                    .join(rel)
                    .to_string_lossy()
                    .into_owned()
            });
        out.push(json!({
            "link_id": l.id,
            "link_type": l.link_type,
            "source_id": o.source.id,
            "entity_id": l.entity_id,
            "entity": entity_qn,
            "document_id": l.document_id,
            "document_path": document_path,
            "section_id": l.chunk_id,
            "resolver": l.resolver,
            "confidence": l.confidence,
            "evidence": l.evidence,
        }));
    }
    Ok(out)
}

/// `references` data: every edge touching the symbol.
pub fn references_value(
    opened: &[Opened],
    name: &str,
    max_depth: usize,
    limit: usize,
) -> Result<Value, DocMonkError> {
    let (matches, edges) =
        graph::references(&corpora(opened), name, max_depth, limit).map_err(search_err)?;
    Ok(json!({
        "query": name,
        "matches": matches.iter().map(|m| match_json(m, opened)).collect::<Vec<_>>(),
        "edges": edges.iter().map(|e| edge_json(e, opened)).collect::<Vec<_>>(),
    }))
}

/// A `search` request.
#[derive(Debug, Clone)]
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub limit: usize,
    /// Also merge and rerank lexical and semantic results.
    pub hybrid: bool,
    /// Expand document hits with their surrounding context.
    pub with_context: bool,
    /// Hard filters, applied before ranking to every stage (ADR 0033).
    pub filters: docmonk_retrieval::route::SearchFilters,
}

/// Everything one `search` produced, for any renderer.
pub struct SearchRun {
    /// Lexical results (absolute paths).
    pub results: Vec<SearchResult>,
    /// The semantic stage, when it ran.
    pub semantic: Option<hybrid::SemanticOutcome>,
    /// The merged hybrid ranking, when requested.
    pub ranked: Option<Vec<hybrid::RankedHit>>,
    /// Context per document hit id.
    pub expanded: HashMap<String, Value>,
    /// `{stage, hits, duration_ms}` per stage.
    pub timings: Vec<Value>,
    /// Lexical confidence (`high`, `medium`, `low`).
    pub confidence: &'static str,
    /// Semantic search is enabled but was skipped for confident lexical hits.
    pub semantic_skipped: bool,
    /// Model and ANN index loads and cache reuse during this run.
    pub counters: docmonk_ml::counters::Counters,
}

impl SearchRun {
    pub fn total_ms(&self) -> f64 {
        self.timings
            .iter()
            .map(|t| t["duration_ms"].as_f64().unwrap_or(0.0))
            .sum()
    }
}

/// The pinned tokenizer's identity, as `search --explain` and `status`
/// report it.
pub fn tokenizer_json() -> Value {
    use docmonk_documents::tokenizer as t;
    json!({
        "model_id": t::EMBEDDING_MODEL_ID,
        "revision": t::TOKENIZER_REVISION,
        "fingerprint": t::tokenizer_fingerprint(),
        "max_sequence_tokens": t::MAX_SEQUENCE_TOKENS,
    })
}

/// Document chunks of one file kept on a plain `search` page.
pub const PLAIN_SEARCH_CHUNKS_PER_DOCUMENT: usize = 2;

/// Runs lexical search, then the semantic, hybrid and context stages the
/// configuration and request call for.
pub fn run_search(
    home: &Home,
    cfg: &docmonk_config::DocMonkConfig,
    opened: &[Opened],
    req: &SearchRequest<'_>,
) -> Result<SearchRun, DocMonkError> {
    let sc = &cfg.search;
    let f = &req.filters;
    let counters_before = docmonk_ml::counters::snapshot();
    // Hard source filter: other sources are never read.
    let corp: Vec<Corpus<'_>> = corpora(opened)
        .into_iter()
        .filter(|c| f.allows_source(c.store.source_id()))
        .collect();
    // Over-fetch so the limit still fills after collapsing sibling chunks
    // (and more when filtering).
    let fetch = if f.is_empty() {
        req.limit * 2
    } else {
        req.limit * 4
    };
    // The semantic stage: model (shared, loaded once per process), one
    // query embedding, every corpus.
    let semantic_job =
        |corp: &[Corpus<'_>]| -> Result<(hybrid::SemanticOutcome, f64, f64), DocMonkError> {
            let started = Instant::now();
            let lazy = lazy_embedder(home, cfg);
            let embedder = lazy.get().ok().map(|a| a.as_ref());
            let model_load_ms = started.elapsed().as_secs_f64() * 1000.0;
            let s = hybrid::semantic_search_with(
                corp,
                embedder,
                req.query,
                req.limit,
                Some(sc.semantic_top_k.max(1) as usize),
                ann_params(cfg),
            )
            .map_err(search_err)?;
            Ok((s, model_load_ms, started.elapsed().as_secs_f64() * 1000.0))
        };
    // With thread-safe readers (server mode) and no lexical-confidence
    // gate, semantic search runs concurrently with the lexical stages.
    let sync: Option<Vec<(&(dyn KnowledgeRead + Sync), &str)>> = corp
        .iter()
        .map(|c| c.store.as_sync().map(|s| (s, c.build_id)))
        .collect();
    let lexical_job = || {
        lexical::search_with_timings(&corp, req.query, fetch, sc.output.snippet_max_tokens)
            .map_err(search_err)
    };
    let (lexical_out, early_semantic) = match sync {
        Some(stores) if sc.semantic && !sc.lazy_semantic => std::thread::scope(|scope| {
            let sem = scope.spawn(move || {
                let corp: Vec<Corpus<'_>> = stores
                    .iter()
                    .map(|(store, build_id)| Corpus {
                        store: *store as &dyn KnowledgeRead,
                        build_id,
                    })
                    .collect();
                semantic_job(&corp)
            });
            let lex = lexical_job();
            let sem = sem.join().unwrap_or_else(|_| {
                Err(DocMonkError::new(
                    ErrorKind::Generic,
                    "semantic search thread panicked",
                ))
            });
            (lex, Some(sem))
        }),
        _ => (lexical_job(), None),
    };
    let (mut results, stage_timings) = lexical_out?;
    docmonk_retrieval::diversify::collapse_document_chunks(
        &mut results,
        PLAIN_SEARCH_CHUNKS_PER_DOCUMENT,
    );
    if !f.is_empty() {
        let mut kept = Vec::with_capacity(results.len());
        for r in results {
            let attachment = r.location.as_ref().is_some_and(|l| l.attachment.is_some());
            let doc = if r.kind == "document" && !f.document_ids.is_empty() {
                let c = corp.iter().find(|c| c.store.source_id() == r.source_id);
                match c {
                    Some(c) => c
                        .store
                        .chunk(c.build_id, &r.id)
                        .map_err(DocMonkError::from)?
                        .map(|ch| ch.document_id)
                        .or_else(|| Some(r.id.clone())),
                    None => None,
                }
            } else {
                None
            };
            if f.allows(&r.source_id, &r.path, r.kind, doc.as_deref(), attachment) {
                kept.push(r);
            }
        }
        results = kept;
    }
    results.truncate(req.limit);
    absolutize_results(&mut results, opened);
    let mut timings: Vec<Value> = stage_timings
        .iter()
        .map(|t| json!({"stage": t.stage, "hits": t.hits, "duration_ms": t.duration_ms}))
        .collect();
    let confidence = classify::estimate_confidence(&results);
    let run_semantic = sc.semantic && !(sc.lazy_semantic && confidence == "high");
    let semantic = if run_semantic {
        let (mut s, model_load_ms, duration_ms) = match early_semantic {
            Some(done) => done?,
            None => semantic_job(&corp)?,
        };
        s.hits.retain(|h| {
            let kind = if h.hit.kind == "entity" {
                "entity"
            } else {
                "document"
            };
            f.allows(
                &h.source_id,
                &h.hit.path,
                kind,
                h.hit.document_id.as_deref(),
                h.attachment.is_some(),
            )
        });
        absolutize_hits(&mut s.hits, opened);
        timings.push(json!({
            "stage": "semantic",
            "hits": s.hits.len(),
            "duration_ms": duration_ms,
            "substages": s.substages_json(model_load_ms),
        }));
        Some(s)
    } else {
        None
    };
    let ranked = if req.hybrid {
        let sem_hits = semantic
            .as_ref()
            .map(|s| s.hits.clone())
            .unwrap_or_default();
        let candidates = hybrid::merge(&results, &sem_hits);
        let collapse = |mut hits: Vec<hybrid::RankedHit>| {
            docmonk_retrieval::diversify::collapse_ranked_document_chunks(
                &mut hits,
                PLAIN_SEARCH_CHUNKS_PER_DOCUMENT,
            );
            hits
        };
        if sc.reranker.enabled {
            let top_n = sc.reranker.top_n.max(1) as usize;
            let pooled = collapse(hybrid::rerank(candidates, req.limit.max(top_n) * 2));
            let started = Instant::now();
            let encoder = cross_encoder(home);
            let mut hits = hybrid::neural_rerank(req.query, pooled, top_n, encoder.as_deref());
            timings.push(json!({
                "stage": "neural_rerank",
                "hits": hits.len().min(top_n),
                "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
            }));
            hits.truncate(req.limit);
            Some(hits)
        } else {
            let mut hits = collapse(hybrid::rerank(candidates, req.limit * 2));
            hits.truncate(req.limit);
            Some(hits)
        }
    } else {
        None
    };
    // Context for document hits, after ranking and the limit.
    let ctx_opts = context::ContextOptions::from_config(&sc.context);
    let mut expanded = HashMap::new();
    if req.with_context && !ctx_opts.disabled() {
        let started = Instant::now();
        for r in results.iter().filter(|r| r.kind == "document") {
            if let Some(o) = opened.iter().find(|o| o.source.id == r.source_id) {
                if let Some(c) =
                    context::expand_chunk_context(o.store.read(), &o.build, &r.id, &ctx_opts)
                        .map_err(search_err)?
                {
                    expanded.insert(r.id.clone(), c);
                }
            }
        }
        timings.push(json!({
            "stage": "context",
            "hits": expanded.len(),
            "duration_ms": started.elapsed().as_secs_f64() * 1000.0,
        }));
    }
    Ok(SearchRun {
        results,
        semantic,
        ranked,
        expanded,
        timings,
        confidence,
        semantic_skipped: sc.semantic && !run_semantic,
        counters: docmonk_ml::counters::snapshot().since(&counters_before),
    })
}

/// The result of `link add`.
pub enum LinkAdded {
    Exists,
    Added {
        entity: String,
        document_id: String,
        link_id: String,
    },
}

/// Links an entity to a document (or one of its sections) in the single
/// source where both resolve unambiguously.
pub fn link_add(
    home: &Home,
    entity: &str,
    document: &str,
    section: Option<&str>,
    source: Option<&str>,
) -> Result<LinkAdded, DocMonkError> {
    let mut opened = open_sources(home, source)?;
    if opened.iter().any(|o| o.store.server().is_some()) {
        return server_link_add(&opened, entity, document, section);
    }
    use docmonk_knowledge::manual;
    let mut hits = Vec::new();
    for (i, o) in opened.iter().enumerate() {
        let store = o.store.local()?;
        let e = manual::resolve_entity(store, &o.build, entity);
        let d = manual::resolve_document(store, &o.build, document);
        if let (Ok(e), Ok(d)) = (e, d) {
            hits.push((i, e, d));
        }
    }
    let (i, e, (_, _, document_id)) = pick_unique(hits, &opened, entity, document)?;
    let o = &mut opened[i];
    let ordinal = match section {
        None => None,
        Some(sid) => match o.store.local()?.chunk(&o.build, sid).map_err(db)? {
            Some(c) if c.document_id == document_id => Some(c.ordinal),
            _ => {
                return Err(DocMonkError::usage(format!(
                    "no such section '{sid}' in document {document_id}"
                )))
            }
        },
    };
    let now = docmonk_indexing::progress::now_iso();
    let build = o.build.clone();
    let added = manual::add(
        o.store.local_mut()?,
        &build,
        &e.id,
        &document_id,
        ordinal,
        None,
        &now,
    )
    .map_err(|err| DocMonkError::usage(err.to_string()))?;
    Ok(match added {
        None => LinkAdded::Exists,
        Some(l) => LinkAdded::Added {
            entity: e.qualified_name,
            document_id,
            link_id: l.id,
        },
    })
}

fn pick_unique<E, D>(
    mut hits: Vec<(usize, E, D)>,
    opened: &[Opened],
    entity: &str,
    document: &str,
) -> Result<(usize, E, D), DocMonkError> {
    match hits.len() {
        0 => Err(DocMonkError::usage(format!(
            "no unambiguous match for entity '{entity}' and document '{document}'"
        ))),
        1 => Ok(hits.remove(0)),
        _ => {
            let ids: Vec<&str> = hits
                .iter()
                .map(|(i, _, _)| opened[*i].source.id.as_str())
                .collect();
            Err(DocMonkError::usage(format!(
                "ambiguous match across sources ({}); pass --source to disambiguate",
                ids.join(", ")
            )))
        }
    }
}

fn server_link_add(
    opened: &[Opened],
    entity: &str,
    document: &str,
    section: Option<&str>,
) -> Result<LinkAdded, DocMonkError> {
    let mut hits = Vec::new();
    for (i, o) in opened.iter().enumerate() {
        let Some(server) = o.store.server() else {
            continue;
        };
        let ents: Vec<_> = match server.entity(&o.build, entity).map_err(sdb)? {
            Some(e) => vec![e],
            None => server.entities_named(&o.build, entity).map_err(sdb)?,
        };
        let doc = server.resolve_document(&o.build, document).map_err(sdb)?;
        if let ([e], Some(d)) = (ents.as_slice(), doc) {
            hits.push((i, e.clone(), d));
        }
    }
    let (i, e, (rel_path, file_id, document_id)) = pick_unique(hits, opened, entity, document)?;
    let o = &opened[i];
    let server = o
        .store
        .server()
        .ok_or_else(|| crate::backend::local_only("link add"))?;
    let (chunk_id, ordinal) = match section {
        None => (None, None),
        Some(sid) => match server.chunk(&o.build, sid).map_err(sdb)? {
            Some(c) if c.document_id == document_id => (Some(c.id), Some(c.ordinal)),
            _ => {
                return Err(DocMonkError::usage(format!(
                    "no such section '{sid}' in document {document_id}"
                )))
            }
        },
    };
    let link_type = docmonk_core::models::RelationshipType::DocumentedBy.as_str();
    let id = docmonk_core::ids::record::link_id(
        &e.qualified_name,
        &format!("{rel_path}#{}", -1),
        ordinal.map(|o| o.to_string()).as_deref(),
        link_type,
        docmonk_knowledge::manual::USER_RESOLVER,
    );
    let added = server
        .put_manual_link(&docmonk_backends::reader::ManualLinkRecord {
            id: id.clone(),
            link_type: link_type.into(),
            entity_id: e.id.clone(),
            entity_qualified_name: e.qualified_name.clone(),
            document_id: document_id.clone(),
            file_id,
            rel_path,
            chunk_id,
            chunk_ordinal: ordinal,
            note: None,
        })
        .map_err(sdb)?;
    Ok(if added {
        LinkAdded::Added {
            entity: e.qualified_name,
            document_id,
            link_id: id,
        }
    } else {
        LinkAdded::Exists
    })
}

/// Removes a manual link from whichever source holds it.
pub fn link_remove(home: &Home, link_id: &str, source: Option<&str>) -> Result<(), DocMonkError> {
    for mut o in open_sources(home, source)? {
        if let Some(server) = o.store.server() {
            if server.remove_manual_link(link_id).map_err(sdb)? {
                return Ok(());
            }
            continue;
        }
        let build = o.build.clone();
        if docmonk_knowledge::manual::remove(o.store.local_mut()?, &build, link_id)
            .map_err(|err| DocMonkError::usage(err.to_string()))?
        {
            return Ok(());
        }
    }
    Err(DocMonkError::usage(format!("no such link: {link_id}")))
}

/// `link list` rows across the selected sources.
pub fn link_list(
    home: &Home,
    entity: Option<&str>,
    document: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<Value>, DocMonkError> {
    let mut rows = Vec::new();
    for o in open_sources(home, source)? {
        rows.extend(link_rows(&o, entity, document)?);
    }
    Ok(rows)
}
