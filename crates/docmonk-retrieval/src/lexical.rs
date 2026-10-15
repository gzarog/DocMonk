//! Lexical search.
//!
//! Ranking order, best first: exact symbol > qualified symbol > alias >
//! title/heading > FTS > path ([`RankTier`]). Within the FTS signal, a hit
//! found by a more precise query structure wins ([`LexicalTier`]): exact
//! phrase, then all terms, then a prefix fallback, then OR of every token.
//! Remaining ties: FTS ordinal, entity kind, recency, source, path, id.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use docmonk_storage::search::{word_tokens, AttachmentProvenance};
use serde::Serialize;

use crate::{Corpus, SearchError};

/// Default number of results.
pub const DEFAULT_LIMIT: usize = 20;
/// Tokens shorter than this are never prefix-wildcarded.
const PREFIX_MIN_TOKEN_LEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RankTier {
    ExactSymbol = 0,
    QualifiedSymbol = 1,
    AliasSymbol = 2,
    TitleOrHeading = 3,
    Fts = 4,
    Path = 5,
}

impl RankTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::ExactSymbol => "exact_symbol",
            Self::QualifiedSymbol => "qualified_symbol",
            Self::AliasSymbol => "alias_symbol",
            Self::TitleOrHeading => "title_or_heading",
            Self::Fts => "fts",
            Self::Path => "path",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LexicalTier {
    Phrase = 0,
    AllTerms = 1,
    Prefix = 2,
    OrFallback = 3,
}

/// Where in a document or file a hit sits.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Location {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_end: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub heading_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentProvenance>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// `entity`, `document` or `path`.
    pub kind: &'static str,
    pub tier: RankTier,
    pub id: String,
    pub title: String,
    pub path: String,
    pub source_id: String,
    pub snippet: Option<String>,
    pub location: Option<Location>,
    pub fts_rank: u32,
    pub query_tier: LexicalTier,
    pub entity_kind_rank: u32,
    pub mtime: f64,
    /// Raw BM25 value behind `fts_rank` (used by hybrid fusion only).
    pub bm25: Option<f64>,
}

/// Fixed structural-to-narrow ordering of entity kinds (99 = other).
pub fn entity_kind_rank(kind: &str) -> u32 {
    match kind {
        "namespace" => 0,
        "class" => 1,
        "interface" => 2,
        "struct" => 3,
        "enum" => 4,
        "function" => 5,
        "method" => 6,
        "property" => 7,
        "field" => 8,
        _ => 99,
    }
}

/// One FTS5 MATCH expression and the precision tier it represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryVariant {
    pub tier: LexicalTier,
    pub expression: String,
}

/// Most precise first: phrase, all terms, prefix (only when it narrows
/// anything), OR fallback. `None` when the query has no word tokens.
pub fn query_plan(text: &str) -> Option<Vec<QueryVariant>> {
    let tokens = word_tokens(text);
    if tokens.is_empty() {
        return None;
    }
    let quoted: Vec<String> = tokens.iter().map(|t| format!("\"{t}\"")).collect();
    let fallback = QueryVariant {
        tier: LexicalTier::OrFallback,
        expression: quoted.join(" OR "),
    };
    if tokens.len() == 1 {
        return Some(vec![
            QueryVariant {
                tier: LexicalTier::Phrase,
                expression: quoted[0].clone(),
            },
            fallback,
        ]);
    }
    let all_terms = quoted.join(" AND ");
    let mut plan = vec![
        QueryVariant {
            tier: LexicalTier::Phrase,
            expression: format!("\"{}\"", tokens.join(" ")),
        },
        QueryVariant {
            tier: LexicalTier::AllTerms,
            expression: all_terms.clone(),
        },
    ];
    if tokens
        .iter()
        .any(|t| t.chars().count() >= PREFIX_MIN_TOKEN_LEN)
    {
        let prefix = tokens
            .iter()
            .map(|t| {
                if t.chars().count() >= PREFIX_MIN_TOKEN_LEN {
                    format!("{t}*")
                } else {
                    format!("\"{t}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        if prefix != all_terms {
            plan.push(QueryVariant {
                tier: LexicalTier::Prefix,
                expression: prefix,
            });
        }
    }
    plan.push(fallback);
    Some(plan)
}

/// Runs `plan` most precise first, skipping repeated expressions and
/// stopping once `limit` distinct rows were seen. Rows stay tagged with
/// the tier that found them; cross-tier duplicates are left to [`merge`].
fn run_plan<R>(
    plan: &[QueryVariant],
    limit: usize,
    mut execute: impl FnMut(&str) -> Result<Vec<R>, SearchError>,
    row_id: impl Fn(&R) -> &str,
) -> Result<Vec<(R, LexicalTier)>, SearchError> {
    let mut seen_expr = HashSet::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for v in plan {
        if !seen_expr.insert(v.expression.as_str()) {
            continue;
        }
        let rows = execute(&v.expression)?;
        for r in rows {
            seen_ids.insert(row_id(&r).to_owned());
            out.push((r, v.tier));
        }
        if seen_ids.len() >= limit {
            break;
        }
    }
    Ok(out)
}

fn sort_key_cmp(a: &SearchResult, b: &SearchResult) -> Ordering {
    a.tier
        .cmp(&b.tier)
        .then(a.query_tier.cmp(&b.query_tier))
        .then(a.fts_rank.cmp(&b.fts_rank))
        .then(a.entity_kind_rank.cmp(&b.entity_kind_rank))
        .then(b.mtime.total_cmp(&a.mtime))
        .then_with(|| a.source_id.cmp(&b.source_id))
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.id.cmp(&b.id))
}

fn entity_result(
    row: docmonk_storage::search::EntitySearchRow,
    tier: RankTier,
    query_tier: LexicalTier,
    source_id: &str,
) -> SearchResult {
    SearchResult {
        kind: "entity",
        tier,
        entity_kind_rank: entity_kind_rank(&row.kind),
        id: row.id,
        title: row.qualified_name,
        path: row.rel_path,
        source_id: source_id.to_owned(),
        snippet: row.signature,
        location: Some(Location {
            line_start: Some(row.start_line),
            line_end: Some(row.end_line),
            ..Default::default()
        }),
        fts_rank: row.fts_rank,
        query_tier,
        mtime: row.mtime,
        bm25: row.bm25,
    }
}

fn search_entities(c: Corpus<'_>, q: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
    let src = c.store.source_id();
    let mut out = Vec::new();
    let mut exact_ids = HashSet::new();
    for row in c.store.search_entities_exact(c.build_id, q)? {
        exact_ids.insert(row.id.clone());
        let tier = if row.name == q {
            RankTier::ExactSymbol
        } else {
            RankTier::QualifiedSymbol
        };
        out.push(entity_result(row, tier, LexicalTier::Phrase, src));
    }
    if q.contains('.') {
        for row in c.store.search_entities_alias(c.build_id, q, limit as i64)? {
            if !exact_ids.contains(&row.id) {
                out.push(entity_result(
                    row,
                    RankTier::AliasSymbol,
                    LexicalTier::Phrase,
                    src,
                ));
            }
        }
    }
    if let Some(plan) = query_plan(q) {
        let rows = run_plan(
            &plan,
            limit,
            |e| Ok(c.store.search_entities_fts(c.build_id, e, limit as i64)?),
            |r| r.id.as_str(),
        )?;
        for (row, qt) in rows {
            out.push(entity_result(row, RankTier::Fts, qt, src));
        }
    }
    Ok(out)
}

fn search_documents(
    c: Corpus<'_>,
    q: &str,
    limit: usize,
    snippet_tokens: i64,
) -> Result<Vec<SearchResult>, SearchError> {
    let src = c.store.source_id();
    let mut out = Vec::new();
    for row in c
        .store
        .search_document_titles(c.build_id, q, limit as i64)?
    {
        out.push(SearchResult {
            kind: "document",
            tier: RankTier::TitleOrHeading,
            id: row.id,
            title: row.title,
            path: row.rel_path,
            source_id: src.to_owned(),
            snippet: row.snippet,
            location: row.attachment.map(|a| Location {
                attachment: Some(a),
                ..Default::default()
            }),
            fts_rank: 0,
            query_tier: LexicalTier::Phrase,
            entity_kind_rank: 99,
            mtime: row.mtime,
            bm25: None,
        });
    }
    let Some(plan) = query_plan(q) else {
        return Ok(out);
    };
    let rows = run_plan(
        &plan,
        limit,
        |e| {
            Ok(c.store
                .search_chunks_fts(c.build_id, e, limit as i64, snippet_tokens)?)
        },
        |r| r.id.as_str(),
    )?;
    for (row, qt) in rows {
        let in_heading = row
            .heading
            .as_deref()
            .is_some_and(|h| heading_matches(h, q));
        out.push(SearchResult {
            kind: "document",
            tier: if in_heading {
                RankTier::TitleOrHeading
            } else {
                RankTier::Fts
            },
            id: row.id,
            title: row.title,
            path: row.rel_path,
            source_id: src.to_owned(),
            snippet: row.snippet,
            location: Some(Location {
                section: row.heading,
                page_start: row.page_start,
                page_end: row.page_end,
                heading_path: row.heading_path,
                attachment: row.attachment,
                ..Default::default()
            }),
            fts_rank: row.fts_rank,
            query_tier: qt,
            entity_kind_rank: 99,
            mtime: row.mtime,
            bm25: row.bm25,
        });
    }
    Ok(out)
}

fn search_paths(c: Corpus<'_>, q: &str, limit: usize) -> Result<Vec<SearchResult>, SearchError> {
    Ok(c.store
        .search_paths_projection(c.build_id, q, limit as i64)?
        .into_iter()
        .map(|row| SearchResult {
            kind: "path",
            tier: RankTier::Path,
            id: row.id,
            title: row.rel_path.clone(),
            path: row.rel_path,
            source_id: c.store.source_id().to_owned(),
            snippet: None,
            location: None,
            fts_rank: 0,
            query_tier: LexicalTier::Phrase,
            entity_kind_rank: 99,
            mtime: row.mtime,
            bm25: None,
        })
        .collect())
}

/// Whether a chunk hit is pinned as a heading match: the query is the
/// whole heading, or its words appear in the heading as a contiguous run
/// of whole words (case-insensitive). `order` matches "Order policy" but
/// not "Reorder policy".
pub fn heading_matches(heading: &str, query: &str) -> bool {
    let h: Vec<String> = word_tokens(heading)
        .iter()
        .map(|w| w.to_lowercase())
        .collect();
    let q: Vec<String> = word_tokens(query)
        .iter()
        .map(|w| w.to_lowercase())
        .collect();
    if q.is_empty() {
        return false;
    }
    heading.trim().eq_ignore_ascii_case(query.trim())
        || h.windows(q.len()).any(|w| w == q.as_slice())
}

/// Keeps each `(kind, id)`'s best-ranked hit and sorts best first.
pub fn merge(results: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut best: HashMap<(&'static str, String), SearchResult> = HashMap::new();
    for r in results {
        let key = (r.kind, r.id.clone());
        match best.get(&key) {
            Some(cur) if sort_key_cmp(&r, cur) != Ordering::Less => {}
            _ => {
                best.insert(key, r);
            }
        }
    }
    let mut out: Vec<SearchResult> = best.into_values().collect();
    out.sort_by(sort_key_cmp);
    out
}

/// Per-stage timing for `--explain`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageTiming {
    pub stage: &'static str,
    pub hits: usize,
    pub duration_ms: f64,
}

/// Lexical search over `corpora`, best first, at most `limit` results.
/// `snippet_tokens` is `search.output.snippet_max_tokens`.
pub fn search(
    corpora: &[Corpus<'_>],
    query: &str,
    limit: usize,
    snippet_tokens: i64,
) -> Result<Vec<SearchResult>, SearchError> {
    Ok(search_with_timings(corpora, query, limit, snippet_tokens)?.0)
}

/// [`search`] plus a per-stage timing breakdown.
///
/// Corpora whose readers can be shared across threads (server mode) are
/// searched concurrently, and each prefetches its whole plan in one
/// round-trip; SQLite corpora run one after another on this thread. The
/// merged ranking does not depend on either.
pub fn search_with_timings(
    corpora: &[Corpus<'_>],
    query: &str,
    limit: usize,
    snippet_tokens: i64,
) -> Result<(Vec<SearchResult>, Vec<StageTiming>), SearchError> {
    let q = query.trim();
    if q.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let sync: Option<Vec<&(dyn docmonk_storage::read::KnowledgeRead + Sync)>> =
        corpora.iter().map(|c| c.store.as_sync()).collect();
    let per_corpus: Vec<CorpusOutput> = match sync {
        Some(stores) if corpora.len() > 1 => std::thread::scope(|scope| {
            let handles: Vec<_> = stores
                .into_iter()
                .zip(corpora)
                .map(|(store, c)| {
                    let build_id = c.build_id;
                    scope.spawn(move || {
                        corpus_search(
                            Corpus {
                                store: store as &dyn docmonk_storage::read::KnowledgeRead,
                                build_id,
                            },
                            q,
                            limit,
                            snippet_tokens,
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(SearchError("search thread panicked".into())))
                })
                .collect::<Result<Vec<_>, _>>()
        })?,
        _ => corpora
            .iter()
            .map(|&c| corpus_search(c, q, limit, snippet_tokens))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let mut timings = Vec::new();
    let mut all = Vec::new();
    for (hits, t) in per_corpus {
        all.extend(hits);
        timings.extend(t);
    }
    let t = std::time::Instant::now();
    let mut merged = merge(all);
    merged.truncate(limit);
    timings.push(StageTiming {
        stage: "merge",
        hits: merged.len(),
        duration_ms: t.elapsed().as_secs_f64() * 1000.0,
    });
    Ok((merged, timings))
}

type CorpusOutput = (Vec<SearchResult>, Vec<StageTiming>);

/// Every lexical stage of one corpus.
fn corpus_search(
    c: Corpus<'_>,
    q: &str,
    limit: usize,
    snippet_tokens: i64,
) -> Result<CorpusOutput, SearchError> {
    let mut timings = Vec::new();
    let mut all = Vec::new();
    let mut stage = |name: &'static str,
                     all: &mut Vec<SearchResult>,
                     run: &dyn Fn() -> Result<Vec<SearchResult>, SearchError>|
     -> Result<(), SearchError> {
        let t = std::time::Instant::now();
        let hits = run()?;
        timings.push(StageTiming {
            stage: name,
            hits: hits.len(),
            duration_ms: t.elapsed().as_secs_f64() * 1000.0,
        });
        all.extend(hits);
        Ok(())
    };
    if c.store.backend_kind() != "sqlite" {
        let mut expressions: Vec<String> = Vec::new();
        for v in query_plan(q).unwrap_or_default() {
            if !expressions.contains(&v.expression) {
                expressions.push(v.expression);
            }
        }
        stage("prefetch", &mut all, &|| {
            c.store
                .prefetch_lexical(c.build_id, q, &expressions, limit as i64, snippet_tokens)?;
            Ok(Vec::new())
        })?;
    }
    stage("entities", &mut all, &|| search_entities(c, q, limit))?;
    stage("documents", &mut all, &|| {
        search_documents(c, q, limit, snippet_tokens)
    })?;
    stage("paths", &mut all, &|| search_paths(c, q, limit))?;
    Ok((all, timings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exprs(q: &str) -> Vec<(LexicalTier, String)> {
        query_plan(q)
            .unwrap()
            .into_iter()
            .map(|v| (v.tier, v.expression))
            .collect()
    }

    #[test]
    fn single_token_plan_is_phrase_plus_fallback() {
        assert_eq!(
            exprs("settlement"),
            [
                (LexicalTier::Phrase, "\"settlement\"".into()),
                (LexicalTier::OrFallback, "\"settlement\"".into())
            ]
        );
        assert!(query_plan("$$$ (( ))").is_none());
    }

    #[test]
    fn multi_token_plan_matches_reference() {
        assert_eq!(
            exprs("cancel an order"),
            [
                (LexicalTier::Phrase, "\"cancel an order\"".into()),
                (
                    LexicalTier::AllTerms,
                    "\"cancel\" AND \"an\" AND \"order\"".into()
                ),
                (LexicalTier::Prefix, "cancel* AND \"an\" AND order*".into()),
                (
                    LexicalTier::OrFallback,
                    "\"cancel\" OR \"an\" OR \"order\"".into()
                ),
            ]
        );
        // No token long enough: no prefix tier.
        assert_eq!(exprs("a to b").len(), 3);
    }

    #[test]
    fn heading_pins_need_whole_words() {
        assert!(heading_matches("Order policy", "order"));
        assert!(heading_matches("Reorder policy", "reorder policy"));
        assert!(!heading_matches("Reorder policy", "order"));
        assert!(heading_matches("Cancel an order", "an order"));
        assert!(!heading_matches("Cancel an order", "order an"));
        assert!(heading_matches("C++ notes", "c++ notes"));
    }

    #[test]
    fn merge_keeps_best_tier_per_key() {
        let mk = |tier, id: &str| SearchResult {
            kind: "entity",
            tier,
            id: id.into(),
            title: id.into(),
            path: "p".into(),
            source_id: "s".into(),
            snippet: None,
            location: None,
            fts_rank: 0,
            query_tier: LexicalTier::Phrase,
            entity_kind_rank: 99,
            mtime: 0.0,
            bm25: None,
        };
        let out = merge(vec![
            mk(RankTier::Fts, "a"),
            mk(RankTier::ExactSymbol, "a"),
            mk(RankTier::Path, "b"),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].id.as_str(), out[0].tier),
            ("a", RankTier::ExactSymbol)
        );
    }
}
