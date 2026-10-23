//! Semantic search over one project build: embed the
//! query, take the ANN (or exact) candidates, attach display metadata.

use std::path::Path;

use docmonk_storage::read::{KnowledgeRead, VectorFilter};

use crate::ann::{self, Engine};
use crate::embedder::Embedder;

/// Default number of results.
pub const DEFAULT_LIMIT: usize = 10;

/// Vector candidates fetched per wanted hit before hits are deduplicated
/// per record (an entity has a signature vector and body-passage vectors).
pub const PER_RECORD_OVERFETCH: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHit {
    /// `entity`, `document` or `passage`.
    pub kind: String,
    pub subject_type: String,
    pub id: String,
    pub title: String,
    pub path: String,
    pub score: f32,
    pub snippet: String,
    /// Heading path of a chunk (`A > B`), if any.
    pub section: Option<String>,
    /// Entity or passage start line, or chunk ordinal.
    pub position: i64,
    /// Entity or passage end line (`None` for chunks).
    pub end_line: Option<i64>,
    pub attachment_index: Option<i64>,
    /// Owning document of a chunk hit.
    pub document_id: Option<String>,
}

/// Where one semantic query spent its time (milliseconds). Model loading
/// happens before [`search`] and is timed by the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SemanticStages {
    pub query_embed_ms: f64,
    pub ann_load_ms: f64,
    pub ann_freshness_ms: f64,
    pub ann_search_ms: f64,
    pub meta_join_ms: f64,
}

impl SemanticStages {
    /// Adds `other` stage by stage (several corpora).
    pub fn add(&mut self, other: &SemanticStages) {
        self.query_embed_ms += other.query_embed_ms;
        self.ann_load_ms += other.ann_load_ms;
        self.ann_freshness_ms += other.ann_freshness_ms;
        self.ann_search_ms += other.ann_search_ms;
        self.meta_join_ms += other.meta_join_ms;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticResult {
    pub available: bool,
    pub reason: String,
    pub engine: Option<Engine>,
    pub hits: Vec<SemanticHit>,
    pub stages: SemanticStages,
}

/// `reason` of a build that has no vectors for the query's model.
pub const NO_VECTORS: &str = "no embeddings computed for this project yet";

impl SemanticResult {
    /// How the search was answered, for diagnostics: the engine
    /// (`hnsw`, `exact`, `server`), `no_vectors` or `unavailable`.
    pub fn engine_label(&self) -> &'static str {
        match self.engine {
            _ if !self.available => "unavailable",
            _ if self.reason == NO_VECTORS => "no_vectors",
            Some(e) => e.name(),
            None => "none",
        }
    }

    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: reason.into(),
            engine: None,
            hits: Vec::new(),
            stages: SemanticStages::default(),
        }
    }
}

fn ms(t: std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Query-time ANN parameters (`search.ann`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnParams {
    /// HNSW beam width (local index).
    pub ef_search: usize,
    /// Elasticsearch kNN `num_candidates`; 0 = `max(10 * k, 100)`.
    pub num_candidates: usize,
}

impl Default for AnnParams {
    fn default() -> Self {
        Self {
            ef_search: ann::DEFAULT_EF_SEARCH,
            num_candidates: 0,
        }
    }
}

/// A query embedded once, reused for every corpus of a request.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryVector {
    pub vector: Vec<f32>,
    /// Fingerprint of the model that produced it.
    pub fingerprint: String,
    /// Time spent embedding (milliseconds).
    pub embed_ms: f64,
}

/// Embeds `query` (trimmed); `None` for an empty query.
pub fn embed_query(embedder: &Embedder, query: &str) -> Result<Option<QueryVector>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(None);
    }
    let t = std::time::Instant::now();
    crate::counters::query_embedded();
    let vector = embedder
        .embed_queries(&[query])
        .map_err(|e| e.to_string())?
        .pop()
        .unwrap_or_default();
    Ok(Some(QueryVector {
        vector,
        fingerprint: embedder.fingerprint().to_owned(),
        embed_ms: ms(t),
    }))
}

/// Searches `build_id` for `query`. This never fails
/// because semantic search cannot run. An empty query, a missing model or
/// a build without vectors is reported in `reason` instead.
pub fn search(
    store: &dyn KnowledgeRead,
    project_dir: &Path,
    build_id: &str,
    embedder: Option<&Embedder>,
    query: &str,
    limit: usize,
    candidate_k: Option<usize>,
) -> Result<SemanticResult, String> {
    let Some(embedder) = embedder else {
        return Ok(SemanticResult::unavailable("embedding model unavailable"));
    };
    let Some(qv) = embed_query(embedder, query)? else {
        return Ok(SemanticResult {
            available: true,
            reason: "empty query".into(),
            engine: None,
            hits: Vec::new(),
            stages: SemanticStages::default(),
        });
    };
    let mut r = search_vector(
        store,
        project_dir,
        build_id,
        &qv,
        limit,
        candidate_k,
        AnnParams::default(),
    )?;
    r.stages.query_embed_ms = qv.embed_ms;
    Ok(r)
}

/// Semantic search of one build for an already embedded query.
pub fn search_vector(
    store: &dyn KnowledgeRead,
    project_dir: &Path,
    build_id: &str,
    qv: &QueryVector,
    limit: usize,
    candidate_k: Option<usize>,
    params: AnnParams,
) -> Result<SemanticResult, String> {
    search_vector_filtered(
        store,
        project_dir,
        build_id,
        qv,
        limit,
        candidate_k,
        params,
        None,
    )
}

/// [`search_vector`] with hard filters the server applies inside its kNN
/// query (exact top-`k` under the filter). The local ANN index ignores
/// `filter`: its caller widens and post-filters instead.
#[allow(clippy::too_many_arguments)]
pub fn search_vector_filtered(
    store: &dyn KnowledgeRead,
    project_dir: &Path,
    build_id: &str,
    qv: &QueryVector,
    limit: usize,
    candidate_k: Option<usize>,
    params: AnnParams,
    filter: Option<&VectorFilter>,
) -> Result<SemanticResult, String> {
    docmonk_core::deadline::check()?;
    let mut stages = SemanticStages::default();
    // Twice the candidates: an entity's signature and body-passage vectors
    // collapse into one hit below, which must still leave `limit` hits.
    let k = candidate_k
        .unwrap_or(limit)
        .max(limit)
        .saturating_mul(PER_RECORD_OVERFETCH);
    let (engine, raw) = match store.as_project_store() {
        Some(local) => {
            let (engine, hits, t) = ann::search_timed(
                local,
                project_dir,
                build_id,
                &qv.fingerprint,
                &qv.vector,
                k,
                params.ef_search,
            )?;
            stages.ann_load_ms = t.load_ms;
            stages.ann_freshness_ms = t.freshness_ms;
            stages.ann_search_ms = t.search_ms;
            (engine, hits)
        }
        None => {
            let t = std::time::Instant::now();
            let found = match filter {
                Some(f) => store.vector_search_filtered(
                    build_id,
                    &qv.fingerprint,
                    &qv.vector,
                    k,
                    params.num_candidates,
                    f,
                ),
                None => store.vector_search(
                    build_id,
                    &qv.fingerprint,
                    &qv.vector,
                    k,
                    params.num_candidates,
                ),
            }
            .map_err(|e| e.to_string())?;
            stages.ann_search_ms = ms(t);
            match found {
                Some(hits) => (Engine::Server, hits),
                None => return Ok(SemanticResult::unavailable("vector search unavailable")),
            }
        }
    };
    if raw.is_empty() {
        return Ok(SemanticResult {
            available: true,
            reason: NO_VECTORS.into(),
            engine: Some(engine),
            hits: Vec::new(),
            stages,
        });
    }
    let t = std::time::Instant::now();
    let keys: Vec<(String, String)> = raw
        .iter()
        .map(|(t, id, _)| (t.clone(), id.clone()))
        .collect();
    let meta = store
        .subject_meta(build_id, &keys)
        .map_err(|e| e.to_string())?;
    let scores: std::collections::HashMap<(&str, &str), f32> = raw
        .iter()
        .map(|(t, id, s)| ((t.as_str(), id.as_str()), *s))
        .collect();
    let mut hits: Vec<SemanticHit> = meta
        .into_iter()
        .filter_map(|m| {
            let score = *scores.get(&(m.subject_type.as_str(), m.subject_id.as_str()))?;
            Some(SemanticHit {
                kind: m.kind,
                subject_type: m.subject_type,
                // A body passage names its owning entity.
                id: m.record_id,
                title: m.title,
                path: m.rel_path,
                score,
                snippet: m.snippet,
                section: m.section,
                position: m.position,
                end_line: m.end_line,
                attachment_index: m.attachment_index,
                document_id: m.document_id,
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.id.cmp(&b.id))
    });
    // One hit per record: an entity's signature vector and its body
    // passages keep only the best-scoring one (with its location).
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert((h.kind.clone(), h.id.clone())));
    hits.truncate(limit);
    stages.meta_join_ms = ms(t);
    Ok(SemanticResult {
        available: true,
        reason: "ok".into(),
        engine: Some(engine),
        hits,
        stages,
    })
}
