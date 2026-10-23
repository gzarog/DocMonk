//! The shared ranking path (ADR 0038): one fusion, one optional neural
//! pass and one context expansion for every surface that ranks evidence.
//!
//! `search --hybrid`, `evidence` (per subquery), `explore` and `ask` (which
//! cites `evidence`) all rank through [`fuse`]:
//!
//! 1. lexical and semantic candidates, each fetched within its own
//!    candidate budget ([`candidate_budget`]), independent of how many hits
//!    the response shows;
//! 2. merge per `(kind, id)`, exact pins first, RRF for the rest
//!    ([`crate::hybrid`]);
//! 3. an optional cross-encoder pass over the hybrid tier, scored on each
//!    hit's canonical stored text ([`canonical_passage`]), never moving a
//!    pinned hit; its state is reported as a [`RerankerStatus`], so a
//!    skipped or unavailable reranker is never mistaken for a ranking
//!    decision;
//! 4. the final limit;
//! 5. for a fact-seeking route, the final hits (unpinned) ordered by how
//!    many of the query's content terms they contain
//!    ([`order_by_term_coverage`]; the fused rank breaks ties): per-source
//!    ranks are not comparable across sources.
//!
//! Without a reranker the result is deterministic. Context is expanded
//! around final hits of every kind ([`expand_context`]), including
//! semantic-only and code hits.

use std::time::Instant;

use docmonk_ml::fusion::MAX_FUSION_CANDIDATES;
use docmonk_ml::reranker::CrossEncoder;
use serde::Serialize;
use serde_json::{json, Value};

use crate::context::{self, ContextOptions};
use crate::hybrid::{self, RankedHit, SourcedHit};
use crate::lexical::SearchResult;
use crate::route::QueryIntent;
use crate::{Corpus, SearchError};

/// Smallest automatic candidate budget.
pub const MIN_CANDIDATES: usize = 40;

/// Candidates fetched per stage for a response of `limit` hits:
/// `configured` (`search.candidate_pool`) when set, else `max(4 * limit,
/// 40)`; never fewer than `limit` and never more than the fusion cap.
pub fn candidate_budget(configured: i64, limit: usize) -> usize {
    let auto = (limit.saturating_mul(4)).max(MIN_CANDIDATES);
    let want = if configured > 0 {
        configured as usize
    } else {
        auto
    };
    want.max(limit).min(MAX_FUSION_CANDIDATES.max(limit))
}

/// What the neural pass did for one ranking.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RerankerStatus {
    /// `search.reranker.enabled`.
    pub enabled: bool,
    /// `applied`, `disabled`, `unavailable` (no local model), `failed`
    /// (scoring returned no scores) or `nothing_to_rescore` (no hybrid-tier
    /// hit to reorder).
    pub state: &'static str,
    /// Hits the pass may rescore (`search.reranker.top_n`).
    pub top_n: usize,
    /// Hits actually rescored.
    pub scored: usize,
    pub duration_ms: f64,
}

impl RerankerStatus {
    fn disabled() -> Self {
        Self {
            enabled: false,
            state: "disabled",
            top_n: 0,
            scored: 0,
            duration_ms: 0.0,
        }
    }
}

/// Options of one [`fuse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseOptions {
    /// Hits returned.
    pub limit: usize,
    /// `Some(top_n)` when the neural pass is enabled.
    pub rerank_top_n: Option<usize>,
    /// At most this many chunks of one document before the neural pass
    /// (`search` shows a document twice at most), or `None`.
    pub per_document_cap: Option<usize>,
    /// Order unpinned hits by term coverage ([`orders_by_coverage`]).
    pub term_coverage: bool,
}

/// Whether a route of `intent` orders its ranking by term coverage: the
/// fact-seeking intents, whose answer must contain the question's terms.
pub fn orders_by_coverage(intent: QueryIntent) -> bool {
    matches!(
        intent,
        QueryIntent::DocumentFact | QueryIntent::Conceptual | QueryIntent::Ambiguous
    )
}

/// Stable reorder of `hits` (pinned hits first, untouched) by how many of
/// `query`'s content terms each hit's title, path and snippet contain.
pub fn order_by_term_coverage(hits: &mut [RankedHit], query: &str) {
    let terms = crate::grounding::content_terms(query);
    if terms.is_empty() {
        return;
    }
    let cover = |h: &RankedHit| {
        let c = &h.candidate;
        let text = format!(
            "{} {} {}",
            c.title,
            c.path,
            c.snippet.as_deref().unwrap_or_default()
        )
        .to_lowercase();
        terms.iter().filter(|t| text.contains(t.as_str())).count()
    };
    let pins = hits.iter().take_while(|h| h.candidate.exact_match).count();
    hits[pins..].sort_by_key(|h| std::cmp::Reverse(cover(h)));
}

/// One fused ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub hits: Vec<RankedHit>,
    pub reranker: RerankerStatus,
}

/// Merges `lexical` and `semantic` (both already inside the request's hard
/// filters), pins exact matches, orders the rest by RRF, when enabled and
/// `encoder` is loaded rescores the hybrid tier on canonical text, and for
/// `opts.term_coverage` orders unpinned hits by term coverage.
pub fn fuse(
    corpora: &[Corpus<'_>],
    query: &str,
    lexical: &[SearchResult],
    semantic: &[SourcedHit],
    opts: FuseOptions,
    encoder: Option<&CrossEncoder>,
) -> Result<Fused, SearchError> {
    let candidates = hybrid::merge(lexical, semantic);
    let collapse = |mut hits: Vec<RankedHit>| {
        if let Some(cap) = opts.per_document_cap {
            crate::diversify::collapse_ranked_document_chunks(&mut hits, cap);
        }
        hits
    };
    // Coverage reorders the final hits only: a candidate the ranking cut
    // does not come back because it repeats more of the question's words.
    let finish = |mut hits: Vec<RankedHit>, reranker: RerankerStatus| {
        hits.truncate(opts.limit);
        if opts.term_coverage {
            order_by_term_coverage(&mut hits, query);
        }
        Fused { hits, reranker }
    };
    let Some(top_n) = opts.rerank_top_n.map(|n| n.max(1)) else {
        let hits = collapse(hybrid::rerank(candidates, opts.limit.saturating_mul(2)));
        return Ok(finish(hits, RerankerStatus::disabled()));
    };
    let pool = collapse(hybrid::rerank(
        candidates,
        opts.limit.max(top_n).saturating_mul(2),
    ));
    let mut status = RerankerStatus {
        enabled: true,
        state: "unavailable",
        top_n,
        scored: 0,
        duration_ms: 0.0,
    };
    let Some(encoder) = encoder else {
        return Ok(finish(pool, status));
    };
    let started = Instant::now();
    let rescorable = pool.iter().filter(|h| !h.candidate.exact_match).count();
    if rescorable < 2 {
        status.state = "nothing_to_rescore";
        return Ok(finish(pool, status));
    }
    let mut texts = Vec::with_capacity(pool.len());
    for h in &pool {
        texts.push(if h.candidate.exact_match {
            String::new()
        } else {
            canonical_passage(corpora, h)?.unwrap_or_else(|| hybrid::snippet_passage(h))
        });
    }
    let by_key: std::collections::HashMap<(String, String, String), String> = pool
        .iter()
        .zip(texts)
        .map(|(h, t)| {
            let c = &h.candidate;
            ((c.source_id.clone(), c.kind.clone(), c.id.clone()), t)
        })
        .collect();
    let hits = hybrid::neural_rerank_with(
        pool,
        top_n,
        |h| {
            let c = &h.candidate;
            by_key
                .get(&(c.source_id.clone(), c.kind.clone(), c.id.clone()))
                .cloned()
                .unwrap_or_else(|| hybrid::snippet_passage(h))
        },
        |texts| encoder.score(query, texts),
    );
    status.scored = hits.iter().filter(|h| h.neural_score.is_some()).count();
    status.state = if status.scored > 0 {
        "applied"
    } else {
        "failed"
    };
    status.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(finish(hits, status))
}

/// The corpus a hit was read from.
fn corpus_of<'c, 'a>(corpora: &'c [Corpus<'a>], source_id: &str) -> Option<&'c Corpus<'a>> {
    corpora.iter().find(|c| c.store.source_id() == source_id)
}

/// The cited lines of an entity hit, when it carries them.
fn cited_lines(h: &RankedHit) -> Option<(i64, i64)> {
    let l = h.candidate.location.as_ref()?;
    Some((l.line_start?, l.line_end.or(l.line_start)?))
}

/// The stored text a cross-encoder scores a hit on: a chunk's text under
/// its title and headings; for a code hit at a passage's lines, that
/// passage under its owner; otherwise `None` (the caller uses the snippet).
pub fn canonical_passage(
    corpora: &[Corpus<'_>],
    h: &RankedHit,
) -> Result<Option<String>, SearchError> {
    let c = &h.candidate;
    let Some(corpus) = corpus_of(corpora, &c.source_id) else {
        return Ok(None);
    };
    Ok(match c.kind.as_str() {
        "document" => corpus
            .store
            .chunk(corpus.build_id, &c.id)?
            .map(|ch| hybrid::passage(&c.title, &ch.heading_path, &context::text_of(&ch))),
        "passage" => corpus
            .store
            .passage(corpus.build_id, &c.id)?
            .map(|p| hybrid::passage(&p.header, &[], &p.text)),
        "entity" => {
            let lines = cited_lines(h);
            let passages = corpus.store.entity_passages(corpus.build_id, &c.id)?;
            let body: Vec<&str> = passages
                .iter()
                .filter(|p| lines.is_none_or(|(s, e)| p.start_line <= e && s <= p.end_line))
                .map(|p| p.text.as_str())
                .collect();
            if body.is_empty() {
                None
            } else {
                Some(hybrid::passage(&c.title, &[], &body.join("\n")))
            }
        }
        _ => None,
    })
}

/// Context around one final hit: a document chunk's heading and sibling
/// chunks; a code hit's passage with its neighbouring passages of the same
/// owner (within `opts.previous_chunks` / `opts.next_chunks`). `None` for a
/// path hit, or a record that is gone.
pub fn expand_context(
    corpora: &[Corpus<'_>],
    h: &RankedHit,
    opts: &ContextOptions,
) -> Result<Option<Value>, SearchError> {
    let c = &h.candidate;
    let Some(corpus) = corpus_of(corpora, &c.source_id) else {
        return Ok(None);
    };
    match c.kind.as_str() {
        "document" => context::expand_chunk_context(corpus.store, corpus.build_id, &c.id, opts),
        "passage" => match corpus.store.passage(corpus.build_id, &c.id)? {
            Some(p) => code_context(corpus, &p, opts).map(Some),
            None => Ok(None),
        },
        "entity" => {
            let lines = cited_lines(h);
            let passages = corpus.store.entity_passages(corpus.build_id, &c.id)?;
            let matched = passages
                .iter()
                .find(|p| lines.is_some_and(|(s, e)| p.start_line <= e && s <= p.end_line))
                .or_else(|| passages.first());
            match matched {
                Some(p) => code_context(corpus, p, opts).map(Some),
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

fn passage_piece(p: &docmonk_storage::knowledge::PassageRow) -> Value {
    json!({
        "id": p.id,
        "kind": p.kind,
        "text": p.text,
        "line_start": p.start_line,
        "line_end": p.end_line,
    })
}

fn code_context(
    corpus: &Corpus<'_>,
    p: &docmonk_storage::knowledge::PassageRow,
    opts: &ContextOptions,
) -> Result<Value, SearchError> {
    let (before, after) = corpus.store.passage_siblings(
        corpus.build_id,
        p,
        opts.previous_chunks,
        opts.next_chunks,
    )?;
    // Passages are bounded (40 lines): the character budget is the same
    // ~4 characters per token the chunker assumes.
    let budget = (opts.max_tokens.max(0) as usize).saturating_mul(4);
    let mut used = p.text.chars().count();
    let mut truncated = false;
    let mut take = |side: Vec<&docmonk_storage::knowledge::PassageRow>| -> Vec<Value> {
        let mut out = Vec::new();
        for s in side {
            let n = s.text.chars().count();
            if used + n > budget {
                truncated = true;
                break;
            }
            used += n;
            out.push(passage_piece(s));
        }
        out
    };
    let mut previous = take(before.iter().rev().collect());
    previous.reverse();
    let next = take(after.iter().collect());
    Ok(json!({
        "matched": passage_piece(p),
        "owner": p.header,
        "previous": previous,
        "next": next,
        "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_budgets_are_independent_of_the_limit_and_bounded() {
        assert_eq!(candidate_budget(0, 10), 40);
        assert_eq!(candidate_budget(0, 20), 80);
        assert_eq!(candidate_budget(0, 50), MAX_FUSION_CANDIDATES);
        assert_eq!(candidate_budget(25, 10), 25);
        assert_eq!(candidate_budget(5, 10), 10, "never below the limit");
        assert_eq!(candidate_budget(10_000, 10), MAX_FUSION_CANDIDATES);
    }
}
