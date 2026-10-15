//! Semantic search over one project build: embed the
//! query, take the ANN (or exact) candidates, attach display metadata.

use std::path::Path;

use docmonk_storage::read::KnowledgeRead;

use crate::ann::{self, Engine};
use crate::embedder::Embedder;

/// Default number of results.
pub const DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHit {
    /// `entity` or `document`.
    pub kind: String,
    pub subject_type: String,
    pub id: String,
    pub title: String,
    pub path: String,
    pub score: f32,
    pub snippet: String,
    /// Heading path of a chunk (`A > B`), if any.
    pub section: Option<String>,
    /// Entity start line, or chunk ordinal.
    pub position: i64,
    /// Entity end line (`None` for chunks).
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

impl SemanticResult {
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
    let query = query.trim();
    let Some(embedder) = embedder else {
        return Ok(SemanticResult::unavailable("embedding model unavailable"));
    };
    if query.is_empty() {
        return Ok(SemanticResult {
            available: true,
            reason: "empty query".into(),
            engine: None,
            hits: Vec::new(),
            stages: SemanticStages::default(),
        });
    }
    let mut stages = SemanticStages::default();
    let t = std::time::Instant::now();
    crate::counters::query_embedded();
    let qv = embedder
        .embed(&[query])
        .map_err(|e| e.to_string())?
        .pop()
        .unwrap_or_default();
    stages.query_embed_ms = ms(t);
    let k = candidate_k.unwrap_or(limit).max(limit);
    let (engine, raw) = match store.as_project_store() {
        Some(local) => {
            let (engine, hits, t) =
                ann::search_timed(local, project_dir, build_id, embedder.fingerprint(), &qv, k)?;
            stages.ann_load_ms = t.load_ms;
            stages.ann_freshness_ms = t.freshness_ms;
            stages.ann_search_ms = t.search_ms;
            (engine, hits)
        }
        None => {
            let t = std::time::Instant::now();
            let found = store
                .vector_search(build_id, embedder.fingerprint(), &qv, k)
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
            reason: "no embeddings computed for this project yet".into(),
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
    let mut hits: Vec<SemanticHit> = meta
        .into_iter()
        .filter_map(|m| {
            let score = raw
                .iter()
                .find(|(t, id, _)| *t == m.subject_type && *id == m.subject_id)?
                .2;
            Some(SemanticHit {
                kind: m.kind,
                subject_type: m.subject_type,
                id: m.subject_id,
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
