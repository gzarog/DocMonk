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
//!   hits of the hybrid tier by snippet (else title); pinned hits keep
//!   their place. If the model is unavailable, the RRF order is kept.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

use docmonk_ml::embedder::Embedder;
use docmonk_ml::fusion::{
    rrf_score, MAX_FUSION_CANDIDATES, MAX_LEXICAL_CANDIDATES, MAX_SEMANTIC_CANDIDATES, RRF_K,
};
use docmonk_ml::reranker::CrossEncoder;
use docmonk_ml::semantic::{self, AnnParams, SemanticHit, SemanticStages};

use crate::lexical::{LexicalTier, Location, RankTier, SearchResult};
use crate::route::SearchFilters;
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
    /// `"<source_id>: <reason>"` for every corpus semantic search could not
    /// read; the other corpora's hits are still returned.
    pub degraded: Vec<String>,
    /// Sub-stage timings summed over corpora (`--explain`).
    pub stages: SemanticStages,
    /// Attachment provenance lookups for chunk hits (milliseconds).
    pub attachments_ms: f64,
}

impl SemanticOutcome {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: reason.into(),
            hits: Vec::new(),
            degraded: Vec::new(),
            stages: SemanticStages::default(),
            attachments_ms: 0.0,
        }
    }

    /// `[{stage, duration_ms}]` of every sub-stage, for `--explain`.
    pub fn substages_json(&self, model_load_ms: f64) -> serde_json::Value {
        let s = &self.stages;
        let row = |stage: &str, ms: f64| serde_json::json!({"stage": stage, "duration_ms": ms});
        serde_json::json!([
            row("model_load", model_load_ms),
            row("query_embed", s.query_embed_ms),
            row("ann_load", s.ann_load_ms),
            row("ann_freshness", s.ann_freshness_ms),
            row("ann_search", s.ann_search_ms),
            row("meta_join", s.meta_join_ms),
            row("attachments", self.attachments_ms),
        ])
    }
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
    semantic_search_with(
        corpora,
        embedder,
        query,
        limit,
        candidate_k,
        AnnParams::default(),
    )
}

/// [`semantic_search`] with explicit ANN parameters. The query is embedded
/// once and searched in every corpus. A corpus that cannot answer (no
/// vectors, no server kNN) is listed in `degraded` while the others'
/// hits are kept; the outcome is unavailable only when no corpus answered.
pub fn semantic_search_with(
    corpora: &[Corpus<'_>],
    embedder: Option<&Embedder>,
    query: &str,
    limit: usize,
    candidate_k: Option<usize>,
    ann: AnnParams,
) -> Result<SemanticOutcome, SearchError> {
    semantic_search_filtered(
        corpora,
        embedder,
        query,
        limit,
        candidate_k,
        ann,
        &SearchFilters::default(),
    )
}

/// Most candidates one corpus is asked for while widening a filtered
/// search (AD05: bounded adaptive expansion, never a silent widening of
/// the requested scope).
pub const MAX_FILTERED_CANDIDATES: usize = 4096;

/// Whether a semantic hit of `source_id` is inside `filter`.
pub fn semantic_hit_allowed(filter: &SearchFilters, source_id: &str, h: &SemanticHit) -> bool {
    let kind = if h.kind == "entity" {
        "entity"
    } else {
        "document"
    };
    filter.allows(
        source_id,
        &h.path,
        kind,
        h.document_id.as_deref(),
        h.attachment_index.is_some(),
    )
}

/// Asks `fetch(k)` for the top `k` candidates, keeping those `keep`
/// accepts, and widens `k` (x4, up to `cap`) until `limit` are kept or the
/// candidates are exhausted: hard filters apply at candidate generation, so
/// out-of-scope hits never crowd in-scope ones out of the top-k. Returns
/// the kept hits (`None`: unavailable) and whether the cap stopped it
/// short.
pub fn filtered_candidates<E>(
    limit: usize,
    base_k: usize,
    cap: usize,
    mut fetch: impl FnMut(usize) -> Result<Option<Vec<SemanticHit>>, E>,
    keep: impl Fn(&SemanticHit) -> bool,
) -> Result<(Option<Vec<SemanticHit>>, bool), E> {
    let mut k = base_k.max(limit).max(1);
    loop {
        let Some(raw) = fetch(k)? else {
            return Ok((None, false));
        };
        let exhausted = raw.len() < k;
        let kept: Vec<SemanticHit> = raw.into_iter().filter(|h| keep(h)).collect();
        if kept.len() >= limit || exhausted || k >= cap {
            let capped = kept.len() < limit && !exhausted;
            return Ok((Some(kept), capped));
        }
        k = (k * 4).min(cap);
    }
}

/// [`semantic_search_with`] restricted to `filter` at candidate generation.
pub fn semantic_search_filtered(
    corpora: &[Corpus<'_>],
    embedder: Option<&Embedder>,
    query: &str,
    limit: usize,
    candidate_k: Option<usize>,
    ann: AnnParams,
    filter: &SearchFilters,
) -> Result<SemanticOutcome, SearchError> {
    let Some(embedder) = embedder else {
        return Ok(SemanticOutcome::unavailable("embedding model unavailable"));
    };
    let Some(qv) = semantic::embed_query(embedder, query).map_err(SearchError)? else {
        return Ok(SemanticOutcome {
            available: true,
            reason: "empty query".into(),
            ..SemanticOutcome::unavailable("")
        });
    };
    let mut hits = Vec::new();
    let mut stages = SemanticStages {
        query_embed_ms: qv.embed_ms,
        ..Default::default()
    };
    let mut attachments_ms = 0.0;
    let mut degraded = Vec::new();
    let mut first_reason = None;
    let mut answered = 0;
    for c in corpora {
        let dir = c
            .store
            .as_project_store()
            .and_then(|s| s.project_dir())
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let sid = c.store.source_id().to_owned();
        if !filter.allows_source(&sid) {
            continue;
        }
        let base_k = candidate_k.unwrap_or(limit).max(limit);
        let mut unavailable = None;
        let (kept, capped) = if filter.is_empty() {
            let r =
                semantic::search_vector(c.store, &dir, c.build_id, &qv, limit, candidate_k, ann)
                    .map_err(SearchError)?;
            stages.add(&r.stages);
            if r.available {
                (Some(r.hits), false)
            } else {
                unavailable = Some(r.reason);
                (None, false)
            }
        } else {
            filtered_candidates::<SearchError>(
                limit,
                base_k,
                MAX_FILTERED_CANDIDATES,
                |k| {
                    let r =
                        semantic::search_vector(c.store, &dir, c.build_id, &qv, k, Some(k), ann)
                            .map_err(SearchError)?;
                    stages.add(&r.stages);
                    if r.available {
                        Ok(Some(r.hits))
                    } else {
                        unavailable = Some(r.reason);
                        Ok(None)
                    }
                },
                |h| semantic_hit_allowed(filter, &sid, h),
            )?
        };
        if capped {
            degraded.push(format!(
                "{sid}: filtered semantic search stopped at {MAX_FILTERED_CANDIDATES} candidates"
            ));
        }
        let Some(raw_hits) = kept else {
            let reason = unavailable.unwrap_or_default();
            degraded.push(format!("{sid}: {reason}"));
            first_reason.get_or_insert(reason);
            continue;
        };

        answered += 1;
        let t = std::time::Instant::now();
        let docs: Vec<String> = raw_hits
            .iter()
            .filter(|h| h.attachment_index.is_some())
            .filter_map(|h| h.document_id.clone())
            .collect();
        let attachments = c.store.document_attachments(c.build_id, &docs)?;
        attachments_ms += t.elapsed().as_secs_f64() * 1000.0;
        for hit in raw_hits {
            let attachment = match (&hit.document_id, hit.attachment_index) {
                (Some(doc), Some(_)) => attachments.get(doc).cloned().flatten(),
                _ => None,
            };
            hits.push(SourcedHit {
                source_id: c.store.source_id().to_owned(),
                hit,
                attachment,
            });
        }
    }
    if answered == 0 && !corpora.is_empty() {
        return Ok(SemanticOutcome {
            degraded,
            stages,
            ..SemanticOutcome::unavailable(first_reason.unwrap_or_default())
        });
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
        degraded,
        stages,
        attachments_ms,
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

/// Rescores the first `top_n` hits of the hybrid tier with `encoder`
/// (snippet, else title). Pinned hits (exact symbol, title, heading) keep
/// their place at the head of the list: the cross-encoder never moves an
/// exact match below a fused one. Without an encoder, or if scoring fails,
/// `hits` is returned unchanged.
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
    neural_rerank_with(hits, top_n, snippet_passage, |texts| {
        encoder.score(query, texts)
    })
}

/// The passage a hit is rescored by without stored text: its snippet,
/// else its title.
pub fn snippet_passage(h: &RankedHit) -> String {
    match &h.candidate.snippet {
        Some(s) if !s.is_empty() => s.clone(),
        _ => h.candidate.title.clone(),
    }
}

/// Words of chunk text a reranker passage keeps (the cross-encoder
/// truncates to its own token budget anyway).
pub const PASSAGE_WORDS: usize = 256;

/// A reranker passage: title, heading path and the first
/// [`PASSAGE_WORDS`] words of the text (for an entity: qualified name and
/// signature), so the cross-encoder sees what the hit is about rather than
/// a short match-centred snippet.
pub fn passage(title: &str, headings: &[String], text: &str) -> String {
    let mut out = title.to_owned();
    if !headings.is_empty() {
        out.push_str(" — ");
        out.push_str(&headings.join(" > "));
    }
    let body: Vec<&str> = text.split_whitespace().take(PASSAGE_WORDS).collect();
    if !body.is_empty() {
        out.push_str(". ");
        out.push_str(&body.join(" "));
    }
    out
}

/// [`neural_rerank`] with any passage builder and scorer.
pub fn neural_rerank_with(
    hits: Vec<RankedHit>,
    top_n: usize,
    text: impl Fn(&RankedHit) -> String,
    score: impl FnOnce(&[&str]) -> Result<Vec<f32>, docmonk_ml::embedder::MlError>,
) -> Vec<RankedHit> {
    let pinned = hits.iter().take_while(|h| h.candidate.exact_match).count();
    let mut hits = hits;
    let tail = hits.split_off(pinned);
    let items: Vec<(RankedHit, String)> = tail
        .into_iter()
        .map(|h| {
            let t = text(&h);
            (h, t)
        })
        .collect();
    let (items, scores) = docmonk_ml::reranker::rerank(items, top_n, |(_, t)| t.as_str(), score);
    let mut out: Vec<RankedHit> = items.into_iter().map(|(h, _)| h).collect();
    if let Some(scores) = scores {
        for (h, s) in out.iter_mut().zip(scores) {
            h.neural_score = Some(s);
        }
    }
    hits.extend(out);
    hits
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sem(id: usize, path: &str) -> SemanticHit {
        SemanticHit {
            kind: "document".into(),
            subject_type: "chunk".into(),
            id: format!("h{id}"),
            title: String::new(),
            path: path.into(),
            score: 1.0 - id as f32 / 1000.0,
            snippet: String::new(),
            section: None,
            position: 0,
            end_line: None,
            attachment_index: None,
            document_id: None,
        }
    }

    /// F06: 40 out-of-scope hits rank above 3 in-scope ones. A post-filter
    /// of the top 10 finds none; candidate generation under the filter
    /// finds all 3.
    #[test]
    fn f06_hard_filters_apply_before_top_k_truncation() {
        let corpus: Vec<SemanticHit> = (0..43)
            .map(|i| sem(i, if i < 40 { "vendor/x.md" } else { "docs/a.md" }))
            .collect();
        let filter = SearchFilters {
            path_prefixes: vec!["docs/".into()],
            ..Default::default()
        };
        let keep = |h: &SemanticHit| semantic_hit_allowed(&filter, "s", h);
        // The old behavior: truncate, then filter.
        let post: Vec<_> = corpus.iter().take(10).filter(|h| keep(h)).collect();
        assert!(post.is_empty());

        let mut asked = Vec::new();
        let (kept, capped) = filtered_candidates::<()>(
            3,
            10,
            MAX_FILTERED_CANDIDATES,
            |k| {
                asked.push(k);
                Ok(Some(corpus.iter().take(k).cloned().collect()))
            },
            keep,
        )
        .unwrap();
        let kept = kept.unwrap();
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().all(|h| h.path.starts_with("docs/")));
        assert!(!capped);
        assert_eq!(asked, [10, 40, 160], "bounded geometric widening");

        // A cap that stops short is reported, never silent.
        let (kept, capped) = filtered_candidates::<()>(
            3,
            10,
            20,
            |k| Ok(Some(corpus.iter().take(k).cloned().collect())),
            keep,
        )
        .unwrap();
        assert!(kept.unwrap().is_empty() && capped);
        // Unfiltered requests keep their single read.
        let mut n = 0;
        filtered_candidates::<()>(
            3,
            10,
            4096,
            |k| {
                n += 1;
                Ok(Some(corpus.iter().take(k).cloned().collect()))
            },
            |_| true,
        )
        .unwrap();
        assert_eq!(n, 1);
    }

    fn hit(id: &str, exact: bool) -> RankedHit {
        RankedHit {
            candidate: Candidate {
                kind: "entity".into(),
                id: id.into(),
                title: id.into(),
                path: format!("{id}.rs"),
                source_id: "s".into(),
                snippet: Some(format!("text of {id}")),
                location: None,
                lexical_tier: Some(if exact {
                    RankTier::ExactSymbol
                } else {
                    RankTier::Fts
                }),
                lexical_fts_rank: 0,
                lexical_query_tier: LexicalTier::Phrase,
                entity_kind_rank: 99,
                mtime: 0.0,
                lexical_rank: Some(1),
                bm25: None,
                semantic_score: None,
                semantic_rank: None,
                exact_match: exact,
                rrf_score: None,
            },
            tier_label: if exact { "exact_symbol" } else { "hybrid" },
            neural_score: None,
        }
    }

    #[test]
    fn pinned_hits_keep_their_place_through_the_neural_pass() {
        let hits = vec![
            hit("exact", true),
            hit("a", false),
            hit("b", false),
            hit("c", false),
        ];
        // The cross-encoder scores only the hybrid tier: the exact match
        // would be scored lowest, but it is never rescored.
        let out = neural_rerank_with(hits, 3, snippet_passage, |texts| {
            assert_eq!(texts.len(), 3, "only the hybrid tier is rescored");
            Ok(vec![0.1, 0.5, 0.9])
        });
        let ids: Vec<&str> = out.iter().map(|h| h.candidate.id.as_str()).collect();
        assert_eq!(ids, ["exact", "c", "b", "a"]);
        assert_eq!(out[0].neural_score, None);
        assert_eq!(out[1].neural_score, Some(0.9));
    }

    #[test]
    fn a_failed_neural_pass_keeps_the_order() {
        let hits = vec![hit("exact", true), hit("a", false), hit("b", false)];
        let out = neural_rerank_with(hits.clone(), 5, snippet_passage, |_| {
            Err(docmonk_ml::embedder::MlError::Load("no model".into()))
        });
        assert_eq!(out, hits);
    }
}
