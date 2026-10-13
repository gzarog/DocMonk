//! Hybrid ranking: merge, fusion and optional neural reranking.
//!
//! * **Merge.** Lexical and semantic hits are deduplicated per `(kind, id)`.
//!   A candidate keeps every signal's rank and score. Lexical display
//!   fields win, and each input is capped at its fusion budget first.
//! * **Pinned tier.** A candidate whose lexical tier is exact or
//!   structural (exact/qualified/alias symbol, title or heading) always
//!   sorts first, in the lexical tie-break order. It is never fused.
//! * **Hybrid tier.** Everything else is ordered by
//!   `rrf_score(lexical_rank, semantic_rank)`, then the same tie-breaks.
//! * **Neural pass (optional).** A cross-encoder rescores the top `top_n`
//!   hits by snippet (else title). If the model is unavailable, the RRF
//!   order is kept.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

use docmonk_ml::embedder::Embedder;
use docmonk_ml::fusion::{
    rrf_score, MAX_FUSION_CANDIDATES, MAX_LEXICAL_CANDIDATES, MAX_SEMANTIC_CANDIDATES, RRF_K,
};
use docmonk_ml::reranker::CrossEncoder;
use docmonk_ml::semantic::{self, SemanticHit};

use crate::lexical::{LexicalTier, Location, RankTier, SearchResult};
use crate::{Corpus, SearchError};

/// A semantic hit tagged with the source it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcedHit {
    pub source_id: String,
    pub hit: SemanticHit,
    /// Provenance when the hit is a chunk of an email attachment.
    pub attachment: Option<docmonk_storage::search::AttachmentProvenance>,
}

/// Outcome of semantic search across corpora (never an error when the
/// model or vectors are missing; see `reason`).
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticOutcome {
    pub available: bool,
    pub reason: String,
    pub hits: Vec<SourcedHit>,
}

/// Semantic search over every corpus, merged best first
/// (`-score, path, id`) and capped at `limit`. `project_dir` maps a store
/// to its project directory (where the ANN index lives).
pub fn semantic_search(
    corpora: &[Corpus<'_>],
    embedder: Option<&Embedder>,
    query: &str,
    limit: usize,
    candidate_k: Option<usize>,
) -> Result<SemanticOutcome, SearchError> {
    let mut hits = Vec::new();
    for c in corpora {
        let dir = c
            .store
            .as_project_store()
            .and_then(|s| s.project_dir())
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let r = semantic::search(
            c.store,
            &dir,
            c.build_id,
            embedder,
            query,
            limit,
            candidate_k,
        )
        .map_err(SearchError)?;
        if !r.available || r.reason == "empty query" {
            return Ok(SemanticOutcome {
                available: r.available,
                reason: r.reason,
                hits: Vec::new(),
            });
        }
        for hit in r.hits {
            let attachment = match (&hit.document_id, hit.attachment_index) {
                (Some(doc), Some(_)) => c.store.document_attachment(c.build_id, doc)?,
                _ => None,
            };
            hits.push(SourcedHit {
                source_id: c.store.source_id().to_owned(),
                hit,
                attachment,
            });
        }
    }
    hits.sort_by(|a, b| semantic_order(&a.hit, &b.hit));
    hits.truncate(limit);
    let reason = if hits.is_empty() {
        "no embeddings computed for this project yet"
    } else {
        "ok"
    }
    .to_owned();
    Ok(SemanticOutcome {
        available: true,
        reason,
        hits,
    })
}

fn semantic_order(a: &SemanticHit, b: &SemanticHit) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.id.cmp(&b.id))
}

/// One `(kind, id)`'s combined evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub path: String,
    pub source_id: String,
    pub snippet: Option<String>,
    pub location: Option<Location>,
    pub lexical_tier: Option<RankTier>,
    pub lexical_fts_rank: u32,
    pub lexical_query_tier: LexicalTier,
    pub entity_kind_rank: u32,
    pub mtime: f64,
    pub lexical_rank: Option<u32>,
    pub bm25: Option<f64>,
    pub semantic_score: Option<f32>,
    pub semantic_rank: Option<u32>,
    pub exact_match: bool,
    pub rrf_score: Option<f64>,
}

/// Deduplicates lexical results (already ranked) and semantic hits
/// (re-sorted by score) into candidates, each input capped at its budget.
pub fn merge(lexical: &[SearchResult], semantic_hits: &[SourcedHit]) -> Vec<Candidate> {
    let mut sem: Vec<&SourcedHit> = semantic_hits.iter().collect();
    sem.sort_by(|a, b| semantic_order(&a.hit, &b.hit));
    sem.truncate(MAX_SEMANTIC_CANDIDATES);
    let mut order: Vec<(String, String)> = Vec::new();
    let mut by_key: HashMap<(String, String), Candidate> = HashMap::new();
    for (i, r) in lexical.iter().take(MAX_LEXICAL_CANDIDATES).enumerate() {
        let key = (r.kind.to_owned(), r.id.clone());
        if by_key.contains_key(&key) {
            continue;
        }
        order.push(key.clone());
        by_key.insert(
            key,
            Candidate {
                kind: r.kind.to_owned(),
                id: r.id.clone(),
                title: r.title.clone(),
                path: r.path.clone(),
                source_id: r.source_id.clone(),
                snippet: r.snippet.clone(),
                location: r.location.clone(),
                lexical_tier: Some(r.tier),
                lexical_fts_rank: r.fts_rank,
                lexical_query_tier: r.query_tier,
                entity_kind_rank: r.entity_kind_rank,
                mtime: r.mtime,
                lexical_rank: Some(i as u32 + 1),
                bm25: r.bm25,
                semantic_score: None,
                semantic_rank: None,
                exact_match: r.tier < RankTier::Fts,
                rrf_score: None,
            },
        );
    }
    for (i, s) in sem.iter().enumerate() {
        let h = &s.hit;
        let key = (h.kind.clone(), h.id.clone());
        if let Some(c) = by_key.get_mut(&key) {
            c.semantic_score = Some(h.score);
            c.semantic_rank = Some(i as u32 + 1);
            continue;
        }
        order.push(key.clone());
        let location = if h.kind == "entity" {
            Location {
                line_start: Some(h.position),
                line_end: h.end_line,
                ..Default::default()
            }
        } else {
            Location {
                section: h.section.clone(),
                attachment: s.attachment.clone(),
                ..Default::default()
            }
        };
        by_key.insert(
            key,
            Candidate {
                kind: h.kind.clone(),
                id: h.id.clone(),
                title: h.title.clone(),
                path: h.path.clone(),
                source_id: s.source_id.clone(),
                snippet: Some(h.snippet.clone()),
                location: Some(location),
                lexical_tier: None,
                lexical_fts_rank: 0,
                lexical_query_tier: LexicalTier::Phrase,
                entity_kind_rank: 99,
                mtime: 0.0,
                lexical_rank: None,
                bm25: None,
                semantic_score: Some(h.score),
                semantic_rank: Some(i as u32 + 1),
                exact_match: false,
                rrf_score: None,
            },
        );
    }
    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .collect()
}

/// A ranked hybrid hit.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedHit {
    pub candidate: Candidate,
    /// The lexical tier label for pinned hits, else `hybrid`.
    pub tier_label: &'static str,
    /// Cross-encoder logit when the neural pass rescored this hit.
    pub neural_score: Option<f32>,
}

impl RankedHit {
    /// The ranked hit's JSON shape.
    pub fn to_json(&self) -> serde_json::Value {
        let c = &self.candidate;
        serde_json::json!({
            "kind": c.kind,
            "id": c.id,
            "title": c.title,
            "path": c.path,
            "source_id": c.source_id,
            "snippet": c.snippet,
            "location": c.location,
            "tier": self.tier_label,
            "semantic_score": c.semantic_score.map(|s| (f64::from(s) * 1e4).round() / 1e4),
            "rrf_score": c.rrf_score.map(|s| (s * 1e6).round() / 1e6),
        })
    }
}

/// Lower-priority tie-breaks shared by both tiers.
fn tail_cmp(a: &Candidate, b: &Candidate) -> Ordering {
    let sem = |c: &Candidate| c.semantic_score.map_or(-1.0, f64::from);
    a.lexical_query_tier
        .cmp(&b.lexical_query_tier)
        .then(a.lexical_fts_rank.cmp(&b.lexical_fts_rank))
        .then(a.entity_kind_rank.cmp(&b.entity_kind_rank))
        .then(sem(b).total_cmp(&sem(a)))
        .then(b.mtime.total_cmp(&a.mtime))
        .then_with(|| a.source_id.cmp(&b.source_id))
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.id.cmp(&b.id))
}

fn tier_label(c: &Candidate) -> &'static str {
    match (c.exact_match, c.lexical_tier) {
        (true, Some(t)) => t.label(),
        _ => "hybrid",
    }
}

/// Pinned candidates first (lexical tie-break order), then the hybrid tier
/// by RRF (capped at `MAX_FUSION_CANDIDATES`), then `limit`.
pub fn rerank(candidates: Vec<Candidate>, limit: usize) -> Vec<RankedHit> {
    let (mut pinned, mut hybrid): (Vec<Candidate>, Vec<Candidate>) =
        candidates.into_iter().partition(|c| c.exact_match);
    pinned.sort_by(|a, b| {
        a.lexical_tier
            .cmp(&b.lexical_tier)
            .then_with(|| tail_cmp(a, b))
    });
    for c in &mut hybrid {
        c.rrf_score = Some(rrf_score(c.lexical_rank, c.semantic_rank, RRF_K));
    }
    hybrid.sort_by(|a, b| {
        b.rrf_score
            .unwrap_or(0.0)
            .total_cmp(&a.rrf_score.unwrap_or(0.0))
            .then_with(|| tail_cmp(a, b))
    });
    hybrid.truncate(MAX_FUSION_CANDIDATES);
    pinned
        .into_iter()
        .chain(hybrid)
        .take(limit)
        .map(|c| RankedHit {
            tier_label: tier_label(&c),
            candidate: c,
            neural_score: None,
        })
        .collect()
}

/// Rescores `hits[..top_n]` with `encoder` (snippet, else title); the rest
/// keep their order. Without an encoder, or if scoring fails, `hits` is
/// returned unchanged.
pub fn neural_rerank(
    query: &str,
    hits: Vec<RankedHit>,
    top_n: usize,
    encoder: Option<&CrossEncoder>,
) -> Vec<RankedHit> {
    let Some(encoder) = encoder else {
        tracing::info!(
            component = "reranker",
            event = "unavailable",
            reason = "model not loaded"
        );
        return hits;
    };
    let text = |h: &RankedHit| -> String {
        match &h.candidate.snippet {
            Some(s) if !s.is_empty() => s.clone(),
            _ => h.candidate.title.clone(),
        }
    };
    let items: Vec<(RankedHit, String)> = hits
        .into_iter()
        .map(|h| {
            let t = text(&h);
            (h, t)
        })
        .collect();
    let (items, scores) = docmonk_ml::reranker::rerank(
        items,
        top_n,
        |(_, t)| t.as_str(),
        |t| encoder.score(query, t),
    );
    let mut out: Vec<RankedHit> = items.into_iter().map(|(h, _)| h).collect();
    if let Some(scores) = scores {
        for (h, s) in out.iter_mut().zip(scores) {
            h.neural_score = Some(s);
        }
    }
    out
}

/// Options for [`hybrid_search`].
#[derive(Debug, Clone, Copy)]
pub struct HybridOptions {
    pub limit: usize,
    pub lexical_k: usize,
    pub semantic_k: usize,
    pub snippet_tokens: i64,
    /// `search.reranker.top_n` when the neural pass is enabled.
    pub rerank_top_n: Option<usize>,
}

impl Default for HybridOptions {
    fn default() -> Self {
        Self {
            limit: crate::lexical::DEFAULT_LIMIT,
            lexical_k: MAX_LEXICAL_CANDIDATES,
            semantic_k: MAX_SEMANTIC_CANDIDATES,
            snippet_tokens: 32,
            rerank_top_n: None,
        }
    }
}

/// Everything one hybrid query produced.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridResult {
    pub lexical: Vec<SearchResult>,
    pub semantic: SemanticOutcome,
    pub hits: Vec<RankedHit>,
}

/// Lexical + semantic → merge → pinned/RRF rerank → optional neural pass.
/// Semantic search being unavailable degrades to lexical-only fusion.
pub fn hybrid_search(
    corpora: &[Corpus<'_>],
    query: &str,
    embedder: Option<&Embedder>,
    encoder: Option<&CrossEncoder>,
    opts: HybridOptions,
) -> Result<HybridResult, SearchError> {
    let lexical = crate::lexical::search(corpora, query, opts.lexical_k, opts.snippet_tokens)?;
    let semantic = semantic_search(
        corpora,
        embedder,
        query,
        opts.semantic_k,
        Some(opts.semantic_k),
    )?;
    let candidates = merge(&lexical, &semantic.hits);
    let hits = match opts.rerank_top_n {
        Some(top_n) => {
            let pool = rerank(candidates, opts.limit.max(top_n));
            let mut hits = neural_rerank(query, pool, top_n, encoder);
            hits.truncate(opts.limit);
            hits
        }
        None => rerank(candidates, opts.limit),
    };
    Ok(HybridResult {
        lexical,
        semantic,
        hits,
    })
}
