//! `evidence`: routed, filtered, decomposed, diversified and grounded
//! retrieval (ADR 0033, P0-R01..R03).
//!
//! 1. **Route** the query to a typed intent and strategies
//!    ([`docmonk_retrieval::route`]); no model or network is involved.
//! 2. **Hard filters** first: sources outside `filters.source_ids` are not
//!    opened; every candidate of every stage (lexical, semantic, graph,
//!    context, every subquery) is checked against the filters before it is
//!    fused or ranked. No stage can widen them.
//! 3. **Decompose** comparisons, flows and multi-part questions into at
//!    most `search.decomposition.max_subqueries` subqueries.
//! 4. Per (sub)query: lexical tiers, semantic search when routed and
//!    available, RRF fusion with exact-match pinning.
//! 5. **Fuse** subqueries (RRF, pins first), **dedup** by provenance and
//!    content, **collapse** sibling chunks, optional **MMR** diversity.
//! 6. **Context** around the top document hits, within the token budget.
//! 7. **Ground**: typed citations re-verified against the same snapshot;
//!    per-part term coverage; `supported` / `partial` /
//!    `insufficient_evidence`.
//!
//! The whole run is bounded by `search.max_query_budget_ms`: once spent,
//! remaining optional stages are skipped and reported as `degraded`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Instant;

use docmonk_config::DocMonkConfig;
use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::Home;
use docmonk_retrieval::decompose::{decompose, Subquery};
use docmonk_retrieval::grounding::{self, EvidenceRef};
use docmonk_retrieval::hybrid::{self, RankedHit, SourcedHit};
use docmonk_retrieval::lexical::{self, SearchResult};
use docmonk_retrieval::route::{route, QueryIntent, Route, RouteStrategy, SearchFilters};
use docmonk_retrieval::{context, diversify, exact, graph, pipeline, Corpus};
use serde_json::{json, Value};

use crate::query::{corpora, join_location, lazy_embedder, open_sources, search_err, Opened};

/// One `evidence` request.
#[derive(Debug, Clone)]
pub struct EvidenceRequest<'a> {
    pub query: &'a str,
    pub filters: SearchFilters,
    pub limit: usize,
}

/// Whether `r` (a lexical result) passes the hard filters (every document
/// hit carries its document id: no lookup per hit).
fn lexical_allowed(r: &SearchResult, f: &SearchFilters) -> bool {
    let attachment = r.location.as_ref().is_some_and(|l| l.attachment.is_some());
    f.allows(
        &r.source_id,
        &r.path,
        r.kind,
        r.document_id.as_deref(),
        attachment,
    )
}

fn search_err_s(e: docmonk_storage::StorageError) -> DocMonkError {
    e.into()
}

fn semantic_allowed(h: &SourcedHit, f: &SearchFilters) -> bool {
    f.allows(
        &h.source_id,
        &h.hit.path,
        hybrid::semantic_kind(&h.hit.kind),
        h.hit.document_id.as_deref(),
        h.attachment.is_some(),
    )
}

/// Ranked hits of one (sub)query, filtered before fusion, ranked by the
/// shared path ([`docmonk_retrieval::pipeline::fuse`]).
#[allow(clippy::too_many_arguments)]
fn run_part(
    home: &Home,
    cfg: &DocMonkConfig,
    corp: &[Corpus<'_>],
    text: &str,
    plan: &Route,
    filters: &SearchFilters,
    limit: usize,
    exact_pins: bool,
    degraded: &mut Vec<String>,
    reranker: &mut Option<pipeline::RerankerStatus>,
    engines: &mut BTreeMap<String, &'static str>,
) -> Result<Vec<RankedHit>, DocMonkError> {
    // The candidate budget, independent of the response size.
    let fetch = pipeline::candidate_budget(cfg.search.candidate_pool, limit);
    // Filters are pushed down into the reads; a conceptual question that
    // semantic search will answer skips the loosest lexical tier (its hits
    // are mostly common-word noise), and so does a question exact
    // occurrences already answer.
    let mut opts = filters.lexical_options();
    let semantic_answers = plan.intent == QueryIntent::Conceptual
        && plan.strategies.contains(&RouteStrategy::Semantic);
    let exact_answers = exact_pins && cfg.search.adaptive_lexical.enabled;
    opts.plan.or_fallback = !(semantic_answers || exact_answers);
    let mut lex = Vec::new();
    for c in corp {
        let (rows, _) = lexical::search_with_options(
            std::slice::from_ref(c),
            text,
            fetch,
            cfg.search.output.snippet_max_tokens,
            &opts,
        )
        .map_err(search_err)?;
        lex.extend(rows.into_iter().filter(|r| lexical_allowed(r, filters)));
    }
    let lex = lexical::merge(lex);
    let mut sem: Vec<SourcedHit> = Vec::new();
    if plan.strategies.contains(&RouteStrategy::Semantic) {
        let lazy = lazy_embedder(home, cfg);
        let embedder = lazy.get().ok().map(|a| a.as_ref());
        let s = hybrid::semantic_search_filtered(
            corp,
            embedder,
            text,
            fetch,
            Some(cfg.search.semantic_top_k.max(1) as usize),
            crate::query::ann_params(cfg),
            filters,
        )
        .map_err(search_err)?;
        if !s.available {
            degraded.push(format!("semantic unavailable: {}", s.reason));
        }
        for d in &s.degraded {
            degraded.push(format!("semantic unavailable for {d}"));
        }
        engines.extend(s.engines.iter().cloned());
        sem = s
            .hits
            .into_iter()
            .filter(|h| semantic_allowed(h, filters))
            .collect();
    }
    let sc = &cfg.search;
    let encoder = sc
        .reranker
        .enabled
        .then(|| crate::query::cross_encoder(home))
        .flatten();
    let fused = pipeline::fuse(
        corp,
        text,
        &lex,
        &sem,
        pipeline::FuseOptions {
            limit: fetch,
            rerank_top_n: sc
                .reranker
                .enabled
                .then_some(sc.reranker.top_n.max(1) as usize),
            per_document_cap: None,
            // Coverage orders the fused subqueries once, after sibling
            // collapse (below), so the collapse keeps each file's best-
            // ranked hits.
            term_coverage: false,
        },
        encoder.as_deref(),
    )
    .map_err(search_err)?;
    if fused.reranker.enabled && fused.reranker.state != "applied" {
        degraded.push(format!("reranker {}", fused.reranker.state));
    }
    *reranker = Some(fused.reranker);
    Ok(fused.hits)
}

/// A caller entity found by graph traversal, as a ranked hit.
fn caller_hit(
    source_id: &str,
    rel: &str,
    e: &docmonk_storage::knowledge::EntityRow,
    evidence: Option<String>,
) -> RankedHit {
    RankedHit {
        candidate: hybrid::Candidate {
            kind: "entity".into(),
            id: e.id.clone(),
            title: e.qualified_name.clone(),
            path: rel.to_owned(),
            source_id: source_id.to_owned(),
            snippet: evidence.or_else(|| e.signature.clone()),
            location: Some(lexical::Location {
                line_start: Some(e.start_line),
                line_end: Some(e.end_line),
                ..Default::default()
            }),
            lexical_tier: None,
            lexical_fts_rank: 0,
            lexical_query_tier: lexical::LexicalTier::Phrase,
            entity_kind_rank: 0,
            mtime: 0.0,
            lexical_rank: None,
            bm25: None,
            semantic_score: None,
            semantic_rank: None,
            exact_match: false,
            rrf_score: None,
        },
        tier_label: "graph_caller",
        neural_score: None,
    }
}

/// An exact occurrence as a pinned hit: code occurrences cite their
/// owning entity, document occurrences their chunk. Template and
/// interpolated matches are not pinned (they are not proof).
fn occurrence_hit(o: &exact::Occurrence) -> Option<RankedHit> {
    let (kind, id, title) = match (&o.entity_row, &o.chunk_id) {
        (Some(e), _) => ("entity", e.id.clone(), e.qualified_name.clone()),
        (None, Some(c)) => ("document", c.clone(), o.path.clone()),
        _ => return None,
    };
    let snippet = match &o.http_method {
        Some(m) => format!("{m} {}", o.matched),
        None => o.matched.clone(),
    };
    Some(RankedHit {
        candidate: hybrid::Candidate {
            kind: kind.into(),
            id,
            title,
            path: o.path.clone(),
            source_id: o.source_id.clone(),
            snippet: Some(snippet),
            location: Some(lexical::Location {
                line_start: o.line,
                line_end: o.line,
                section: o.section.clone(),
                page_start: o.page,
                page_end: o.page,
                ..Default::default()
            }),
            lexical_tier: None,
            lexical_fts_rank: 0,
            lexical_query_tier: lexical::LexicalTier::Phrase,
            entity_kind_rank: 0,
            mtime: 0.0,
            lexical_rank: None,
            bm25: None,
            semantic_score: None,
            semantic_rank: None,
            exact_match: o.class != exact::EvidenceClass::HeuristicMatch,
            rrf_score: None,
        },
        tier_label: o.class.as_str(),
        neural_score: None,
    })
}

/// RRF across subqueries; pinned exact matches first.
fn fuse(parts: Vec<Vec<RankedHit>>) -> Vec<RankedHit> {
    let mut best: HashMap<(String, String, String), (RankedHit, f64, usize)> = HashMap::new();
    for part in parts {
        for (rank, h) in part.into_iter().enumerate() {
            let key = (
                h.candidate.source_id.clone(),
                h.candidate.kind.clone(),
                h.candidate.id.clone(),
            );
            let add = 1.0 / (60.0 + rank as f64 + 1.0);
            best.entry(key)
                .and_modify(|e| {
                    e.1 += add;
                    e.2 = e.2.min(rank);
                })
                .or_insert((h, add, rank));
        }
    }
    let mut v: Vec<(RankedHit, f64, usize)> = best.into_values().collect();
    v.sort_by(|a, b| {
        b.0.candidate
            .exact_match
            .cmp(&a.0.candidate.exact_match)
            // Exact matches of one name are equally exact; their per-source
            // rank ties are broken by path-derived ids, so order by path.
            .then_with(|| {
                if a.0.candidate.exact_match && b.0.candidate.exact_match {
                    a.0.candidate.path.cmp(&b.0.candidate.path)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then(b.1.total_cmp(&a.1))
            .then(a.2.cmp(&b.2))
            .then_with(|| a.0.candidate.source_id.cmp(&b.0.candidate.source_id))
            .then_with(|| a.0.candidate.path.cmp(&b.0.candidate.path))
            .then_with(|| a.0.candidate.id.cmp(&b.0.candidate.id))
    });
    v.into_iter().map(|x| x.0).collect()
}

fn root_of<'a>(opened: &'a [Opened], source_id: &str) -> Option<&'a str> {
    opened
        .iter()
        .find(|o| o.source.id == source_id)
        .map(|o| o.source.path.as_str())
}

fn absolute(opened: &[Opened], source_id: &str, rel: &str) -> String {
    match root_of(opened, source_id) {
        Some(root) if !Path::new(rel).is_absolute() => {
            Path::new(root).join(rel).to_string_lossy().into_owned()
        }
        _ => rel.to_owned(),
    }
}

/// How a hit was ranked: the stages that found it and the fused scores
/// (ADR 0038). Every surface reports the same fields.
fn retrieval_of(h: &RankedHit) -> Value {
    let c = &h.candidate;
    json!({
        "pinned": c.exact_match,
        "lexical_rank": c.lexical_rank,
        "semantic_rank": c.semantic_rank,
        "semantic_score": c.semantic_score.map(|s| (f64::from(s) * 1e4).round() / 1e4),
        "rrf_score": c.rrf_score.map(|s| (s * 1e6).round() / 1e6),
        "neural_score": h.neural_score.map(|s| (f64::from(s) * 1e4).round() / 1e4),
    })
}

/// Counts of re-verified citations: `checked`, `valid`, `invalid` and the
/// invalid ones by reason.
fn provenance_summary(checks: &[grounding::Verification]) -> Value {
    let mut by_reason: std::collections::BTreeMap<&str, usize> = Default::default();
    for v in checks.iter().filter(|v| !v.valid) {
        *by_reason.entry(v.reason.unwrap_or("unknown")).or_default() += 1;
    }
    let valid = checks.iter().filter(|v| v.valid).count();
    json!({
        "checked": checks.len(),
        "valid": valid,
        "invalid": checks.len() - valid,
        "invalid_by_reason": by_reason,
    })
}

/// The `evidence` payload (CLI `docmonk evidence --json`, MCP
/// `docmonk_evidence`).
pub fn evidence_value(
    home: &Home,
    cfg: &DocMonkConfig,
    req: &EvidenceRequest<'_>,
) -> Result<Value, DocMonkError> {
    let started = Instant::now();
    let budget_ms = cfg.search.max_query_budget_ms.max(1) as u128;
    let over = |t: &Instant| t.elapsed().as_millis() > budget_ms;
    let mut timings = Vec::new();
    let mut degraded: Vec<String> = Vec::new();
    let sc = &cfg.search;
    let plan = if sc.routing.enabled {
        route(req.query, crate::query::route_options(cfg))
    } else {
        // Routing off: the plain lexical plan, nothing intent-specific.
        Route {
            intent: QueryIntent::Ambiguous,
            confidence: 0.0,
            signals: vec!["routing_disabled".into()],
            strategies: vec![RouteStrategy::LexicalExact, RouteStrategy::LexicalFts],
            fallback: true,
            target: None,
            anchors: Default::default(),
        }
    };
    // Hard source filter before any retrieval: other sources are not read.
    let mut opened = open_sources(home, None)?;
    opened.retain(|o| req.filters.allows_source(&o.source.id));
    let corp = corpora(&opened);
    // Server requests this answer costs (diagnostics.backend).
    let backend_before = opened
        .iter()
        .find_map(|o| o.store.server())
        .map(docmonk_backends::reader::ServerReader::backend_stats);
    crate::query::apply_retrieval_settings(cfg);
    // Exact occurrences of URL anchors first (P03), within the filters.
    let endpoint = plan.intent == QueryIntent::EndpointReference;
    let mut exact_outcome: Option<exact::ExactOutcome> = None;
    if endpoint && sc.exact_occurrences.enabled {
        let t = Instant::now();
        let mut o = exact::find_occurrences(
            &corp,
            &plan.anchors.url_paths,
            &exact::ExactOptions {
                max_results: sc.exact_occurrences.max_results.max(1) as usize,
                documents: true,
                filter: req.filters.lexical_options().filter,
                snippet_tokens: sc.output.snippet_max_tokens,
                max_template_variants: 4,
            },
        )
        .map_err(search_err)?;
        o.occurrences.retain(|x| {
            let kind = if x.chunk_id.is_some() {
                "document"
            } else {
                "entity"
            };
            req.filters
                .allows(&x.source_id, &x.path, kind, x.document_id.as_deref(), false)
        });
        timings.push(json!({"stage": "exact_occurrences", "ms": t.elapsed().as_millis()}));
        for c in o
            .capabilities
            .iter()
            .filter(|c| c.exact_occurrences != "indexed")
        {
            degraded.push(format!(
                "exact occurrences not checked in source {} ({}): rebuild its graph to add them",
                c.source_id, c.exact_occurrences
            ));
        }
        exact_outcome = Some(o);
    }
    let exact_pins: Vec<RankedHit> = exact_outcome
        .iter()
        .flat_map(|o| o.occurrences.iter().filter_map(occurrence_hit))
        .collect();
    let subqueries: Vec<Subquery> = if plan.strategies.contains(&RouteStrategy::Decompose) {
        decompose(req.query, sc.decomposition.max_subqueries.max(1) as usize)
    } else {
        Vec::new()
    };
    let parts: Vec<String> = if endpoint {
        // Lexical search focused on the anchors, not the whole question.
        vec![plan
            .anchors
            .url_paths
            .iter()
            .map(|u| u.path.clone())
            .collect::<Vec<_>>()
            .join(" ")]
    } else if subqueries.is_empty() {
        vec![req.query.to_owned()]
    } else {
        subqueries.iter().map(|s| s.text.clone()).collect()
    };
    let limit = req.limit.max(1);
    let t = Instant::now();
    let mut per_part = Vec::new();
    let mut reranker: Option<pipeline::RerankerStatus> = None;
    // How each source's semantic search answered (diagnostics.semantic_engines).
    let mut engines: BTreeMap<String, &'static str> = BTreeMap::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 && over(&started) {
            degraded.push(format!(
                "query budget {budget_ms}ms reached: subquery '{p}' skipped"
            ));
            per_part.push(Vec::new());
            continue;
        }
        per_part.push(run_part(
            home,
            cfg,
            &corp,
            p,
            &plan,
            &req.filters,
            limit,
            !exact_pins.is_empty(),
            &mut degraded,
            &mut reranker,
            &mut engines,
        )?);
    }
    timings.push(json!({"stage": "retrieve", "ms": t.elapsed().as_millis()}));
    let contributions: Vec<usize> = per_part.iter().map(Vec::len).collect();
    let part_hits = per_part.clone();
    let mut hits = fuse(per_part);
    let deduped = diversify::dedup(&mut hits);
    let collapsed = diversify::collapse_siblings(
        &mut hits,
        sc.diversity.per_document_cap.max(1) as usize,
        sc.diversity.per_source_cap.max(0) as usize,
    );
    // Comparisons need every side: keep each part's best in-scope hit.
    if parts.len() > 1 {
        for ph in &part_hits {
            if let Some(top) = ph.first() {
                let present = hits
                    .iter()
                    .take(limit)
                    .any(|h| h.candidate.id == top.candidate.id);
                if !present && !hits.is_empty() {
                    hits.retain(|h| h.candidate.id != top.candidate.id);
                    let at = (limit - 1).min(hits.len());
                    hits.insert(at, top.clone());
                }
            }
        }
    }
    // Fact-seeking routes: the shared term-coverage order (ADR 0038) over
    // the fused subqueries.
    let coverage = sc.routing.enabled && pipeline::orders_by_coverage(plan.intent);
    if coverage {
        pipeline::order_by_term_coverage(&mut hits, req.query);
    }
    if sc.diversity.enabled && !over(&started) {
        hits = diversify::mmr(hits, sc.diversity.lambda);
    }
    if !exact_pins.is_empty() {
        let key = |h: &RankedHit| {
            (
                h.candidate.source_id.clone(),
                h.candidate.kind.clone(),
                h.candidate.id.clone(),
            )
        };
        let mut seen = std::collections::HashSet::new();
        let mut pinned: Vec<RankedHit> = exact_pins
            .iter()
            .filter(|h| seen.insert(key(h)))
            .cloned()
            .collect();
        hits.retain(|h| !seen.contains(&key(h)));
        pinned.append(&mut hits);
        hits = pinned;
    }
    hits.truncate(limit);

    // Graph context for symbol-centred routes (filtered like everything).
    // For navigation and impact questions the callers *are* the answer:
    // they become evidence right after the pinned definitions.
    let mut graph_edges = Vec::new();
    if plan.strategies.contains(&RouteStrategy::SymbolGraph) && !over(&started) {
        if let Some(target) = &plan.target {
            let edges = if endpoint {
                // Callers of the entities holding the exact occurrences
                // (handlers, client methods): only their sources are read.
                let mut starts: Vec<(usize, String)> = Vec::new();
                for o in exact_outcome.iter().flat_map(|o| &o.occurrences) {
                    if o.class == exact::EvidenceClass::HeuristicMatch {
                        continue;
                    }
                    if let Some(e) = &o.entity_row {
                        let s = (o.corpus, e.id.clone());
                        if !starts.contains(&s) {
                            starts.push(s);
                        }
                    }
                }
                starts.truncate(sc.selective_graph.max_seed_entities.max(1) as usize);
                let mut edges: Vec<graph::TraversalEdge> = graph::traverse_many(
                    &corp,
                    &starts,
                    graph::Direction::Incoming,
                    &graph::CALL_TYPES,
                    2,
                    50,
                )
                .map_err(search_err)?
                .into_iter()
                .flatten()
                .collect();
                edges.truncate(50);
                edges
            } else {
                graph::traverse_symbol(
                    &corp,
                    target,
                    graph::Direction::Incoming,
                    &graph::CALL_TYPES,
                    2,
                    50,
                )
                .map_err(search_err)?
                .1
            };
            let callers_are_answer = matches!(
                plan.intent,
                QueryIntent::CodeNavigation
                    | QueryIntent::ImpactAnalysis
                    | QueryIntent::EndpointReference
            );
            let pins = hits.iter().take_while(|h| h.candidate.exact_match).count();
            let mut callers = Vec::new();
            for e in edges {
                let loc = e.relationship.source_location.clone().unwrap_or_default();
                let file = loc
                    .rsplit_once(':')
                    .map_or(loc.as_str(), |(p, _)| p)
                    .to_owned();
                let Some(c) = corp.get(e.corpus) else {
                    continue;
                };
                let source_id = c.store.source_id().to_owned();
                if !req.filters.allows(&source_id, &file, "entity", None, false) {
                    continue;
                }
                graph_edges.push(json!({
                    "source_id": source_id,
                    "relationship_type": e.relationship.relationship_type,
                    "depth": e.depth,
                    "source_location": root_of(&opened, &source_id).map(|r| join_location(r, &loc)),
                    "evidence": e.relationship.evidence,
                }));
                if callers_are_answer {
                    if let Some(ent) = c
                        .store
                        .entity(c.build_id, &e.relationship.source_entity_id)
                        .map_err(search_err_s)?
                    {
                        let rel = c
                            .store
                            .file_rel_path(c.build_id, &ent.file_id)
                            .map_err(search_err_s)?
                            .unwrap_or(file);
                        callers.push((
                            e.depth,
                            caller_hit(&source_id, &rel, &ent, e.relationship.evidence.clone()),
                        ));
                    }
                }
            }
            // Direct callers first, then deterministic order (entity ids
            // depend on the source root, so they are not a stable key).
            callers.sort_by(
                |(da, a): &(usize, RankedHit), (db, b): &(usize, RankedHit)| {
                    let key = |d: usize, h: &RankedHit| {
                        (
                            d,
                            h.candidate.path.clone(),
                            h.candidate.location.as_ref().and_then(|l| l.line_start),
                            h.candidate.title.clone(),
                        )
                    };
                    key(*da, a).cmp(&key(*db, b))
                },
            );
            let mut seen = std::collections::HashSet::new();
            let callers: Vec<RankedHit> = callers
                .into_iter()
                .map(|(_, h)| h)
                .filter(|h| seen.insert(h.candidate.id.clone()))
                .collect();
            hits.retain(|h| {
                !callers
                    .iter()
                    .any(|c: &RankedHit| c.candidate.id == h.candidate.id)
            });
            for (i, c) in callers.into_iter().enumerate() {
                let at = (pins + i).min(hits.len());
                hits.insert(at, c);
            }
            hits.truncate(limit);
        }
    }

    // Grounding against the same snapshot.
    let builds: HashMap<String, String> = opened
        .iter()
        .map(|o| (o.source.id.clone(), o.build.clone()))
        .collect();
    // Each record's canonical content is read once (batched per corpus),
    // stamped on its citation and reused by the verification.
    let mut refs = grounding::evidence_refs(&hits, |s| builds.get(s).cloned());
    let contents = grounding::canonical_contents(&corp, &refs).map_err(search_err)?;
    grounding::stamp_contents(&mut refs, &contents);
    let checks =
        grounding::verify_with(&corp, &refs, &req.filters, &contents).map_err(search_err)?;
    let valid: Vec<&EvidenceRef> = refs
        .iter()
        .zip(&checks)
        .filter(|(_, v)| v.valid)
        .map(|(r, _)| r)
        .collect();
    let min = sc.grounding.min_coverage;
    let supports: Vec<grounding::Support> = parts
        .iter()
        .map(|p| grounding::support(p, &valid, min))
        .collect();
    let verdict = if sc.grounding.enabled {
        grounding::abstain(&supports)
    } else {
        "not_checked"
    };

    // Context around the top document and code hits (semantic-only ones
    // included), within the token budget.
    let ctx_opts = context::ContextOptions::from_config(&sc.context);
    let mut contexts: HashMap<String, Value> = HashMap::new();
    if plan.strategies.contains(&RouteStrategy::ContextExpansion) && !ctx_opts.disabled() {
        for h in hits
            .iter()
            .filter(|h| matches!(h.candidate.kind.as_str(), "document" | "entity" | "passage"))
            .take(3)
        {
            if over(&started) {
                degraded.push("query budget reached: context expansion skipped".into());
                break;
            }
            if let Some(c) = pipeline::expand_context(&corp, h, &ctx_opts).map_err(search_err)? {
                contexts.insert(h.candidate.id.clone(), c);
            }
        }
    }
    let evidence: Vec<Value> = refs
        .iter()
        .zip(&checks)
        .zip(&hits)
        .map(|((r, v), h)| {
            json!({
                "id": r.id,
                "valid": v.valid,
                "invalid_reason": v.reason,
                "kind": r.kind,
                "source_id": r.source_id,
                "build_id": r.build_id,
                "record_id": r.record_id,
                "title": h.candidate.title,
                "path": absolute(&opened, &r.source_id, &r.path),
                "line_start": r.line_start,
                "line_end": r.line_end,
                "page_start": r.page_start,
                "page_end": r.page_end,
                "heading": r.heading,
                "fingerprint": r.fingerprint,
                "content_fingerprint": r.content_fingerprint,
                "language": r.language,
                "tier": h.tier_label,
                "retrieval": retrieval_of(h),
                "text": r.text,
                "context": contexts.get(&r.record_id),
            })
        })
        .collect();
    let provenance = provenance_summary(&checks);
    timings.push(json!({"stage": "total", "ms": started.elapsed().as_millis()}));
    let backend_after = opened
        .iter()
        .find_map(|o| o.store.server())
        .map(docmonk_backends::reader::ServerReader::backend_stats);
    let backend = match (&backend_before, &backend_after) {
        (Some(b), Some(a)) => json!({
            "kind": opened.first().map(|o| o.store.read().backend_kind()),
            "requests": a.requests.saturating_sub(b.requests),
            "bytes_sent": a.bytes_sent.saturating_sub(b.bytes_sent),
            "bytes_received": a.bytes_received.saturating_sub(b.bytes_received),
        }),
        _ => json!({"kind": opened.first().map(|o| o.store.read().backend_kind()), "requests": 0}),
    };
    let mut out = json!({
        "query": req.query,
        "verdict": verdict,
        "plan": {
            "intent": plan.intent.as_str(),
            "confidence": plan.confidence,
            "signals": plan.signals,
            "strategies": plan.strategies,
            "fallback": plan.fallback,
            "target": plan.target,
            "subqueries": subqueries,
            "filters": req.filters,
            "budget_ms": budget_ms,
        },
        "evidence": evidence,
        "provenance": provenance,
        "support": supports,
        "graph": graph_edges,
        "diagnostics": {
            "subquery_candidates": contributions,
            "deduplicated": deduped,
            "collapsed": collapsed,
            "diversity": sc.diversity.enabled,
            "sources_searched": opened.iter().map(|o| o.source.id.clone()).collect::<Vec<_>>(),
            "degraded": degraded,
            "timings": timings,
            "reranker": reranker,
            "semantic_engines": engines,
            "backend": backend,
        },
    });
    // Additive keys, only when the route has anchors.
    if !plan.anchors.is_empty() {
        out["plan"]["anchors"] = json!(plan.anchors);
    }
    if let Some(o) = &exact_outcome {
        out["exact_occurrences"] = json!(o
            .occurrences
            .iter()
            .map(|x| {
                let mut v = json!(x);
                v["evidence_class"] = json!(x.class.as_str());
                v["path"] = json!(absolute(&opened, &x.source_id, &x.path));
                v
            })
            .collect::<Vec<_>>());
        out["exact_capabilities"] = json!(o.capabilities);
    }
    Ok(out)
}
