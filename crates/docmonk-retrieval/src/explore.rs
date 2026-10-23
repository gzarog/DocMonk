//! `explore` and `impact` with the query planner, context
//! builder, evidence assembly and document links behind them.
//!
//! * [`plan`]: the deterministic query planner (intent + strategies).
//! * [`build_context`]: deduplicated, priority-sorted evidence capped by a
//!   [`Budget`], with explicit truncation reasons.
//! * [`linked_document_evidence`]: documentation evidence via cross-domain
//!   links.
//! * [`impact`] / [`explore`]: the payloads the CLI and MCP render, with
//!   stable JSON keys.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::exact::{self, EvidenceClass, ExactOutcome, Occurrence};
use crate::graph::{self, ResolvedEdge, SourceMatch, CALL_TYPES, DEFAULT_LIMIT, DEFAULT_MAX_DEPTH};
use crate::hybrid::SemanticOutcome;
use crate::lexical::{self, RankTier, SearchResult};
use crate::route::{self, QueryAnchors, QueryIntent, Route, RouteOptions};
use crate::{Corpus, SearchError};

// ---- planner -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Identifier,
    Fts,
    #[serde(rename = "graph_callers")]
    Callers,
    #[serde(rename = "graph_callees")]
    Callees,
    #[serde(rename = "graph_references")]
    References,
    Tests,
    Documents,
    Semantic,
    /// Indexed exact occurrences of URL anchors ([`crate::exact`]).
    ExactOccurrences,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identifier => "identifier",
            Self::Fts => "fts",
            Self::Callers => "graph_callers",
            Self::Callees => "graph_callees",
            Self::References => "graph_references",
            Self::Tests => "tests",
            Self::Documents => "documents",
            Self::Semantic => "semantic",
            Self::ExactOccurrences => "exact_occurrences",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Identifier,
    Callers,
    Callees,
    Impact,
    Document,
    General,
    /// An API endpoint path is the question's primary anchor.
    #[serde(rename = "endpoint_reference")]
    Endpoint,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identifier => "identifier",
            Self::Callers => "callers",
            Self::Callees => "callees",
            Self::Impact => "impact",
            Self::Document => "document",
            Self::General => "general",
            Self::Endpoint => "endpoint_reference",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    pub query: String,
    pub intent: Intent,
    pub strategies: Vec<Strategy>,
    pub symbol: Option<String>,
    /// Typed anchors of the query (empty for the rule-order [`plan`]).
    pub anchors: QueryAnchors,
    /// The shared routing decision behind a [`plan_routed`] plan.
    pub route: Option<Route>,
}

struct Patterns {
    identifier: Regex,
    impact: Vec<Regex>,
    callers: Vec<Regex>,
    callees: Vec<Regex>,
    document: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let ci = |s: &str| Regex::new(&format!("(?i){s}")).expect("valid pattern");
        Patterns {
            identifier: Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$")
                .expect("valid"),
            impact: [
                r"what breaks if ([\w.]+)",
                r"what happens if ([\w.]+) changes",
                r"impact of ([\w.]+)",
                r"what depends on ([\w.]+)",
            ]
            .map(ci)
            .into(),
            callers: [
                r"who calls ([\w.]+)",
                r"callers of ([\w.]+)",
                r"what calls ([\w.]+)",
                r"who (?:uses|references) ([\w.]+)",
            ]
            .map(ci)
            .into(),
            callees: [
                r"what does ([\w.]+) call",
                r"callees of ([\w.]+)",
                r"what ([\w.]+) calls",
            ]
            .map(ci)
            .into(),
            document: ci(r"\bdocuments?\b|\bdocs?\b|\bdocumentation\b"),
        }
    })
}

fn first_match(patterns: &[Regex], text: &str) -> Option<String> {
    patterns
        .iter()
        .find_map(|p| p.captures(text).map(|c| c[1].to_owned()))
}

/// Classifies `query` into an intent and ordered strategies (rules,
/// in order: impact, callers, callees, document hint,
/// identifier, general). `semantic_enabled` only ever adds
/// [`Strategy::Semantic`] to document/general plans.
///
/// The rule-order planner, kept for compatibility (its plans are what
/// [`plan_routed`] produces for a query without technical anchors).
/// Explore, ask and MCP use [`plan_routed`].
pub fn plan(query: &str, semantic_enabled: bool) -> QueryPlan {
    plan_rules(query, semantic_enabled, &QueryAnchors::default())
}

fn plan_rules(query: &str, semantic_enabled: bool, anchors: &QueryAnchors) -> QueryPlan {
    use Strategy::*;
    let p = patterns();
    let text = query.trim();
    let mk = |intent, strategies: Vec<Strategy>, symbol| QueryPlan {
        query: query.to_owned(),
        intent,
        strategies,
        symbol,
        anchors: anchors.clone(),
        route: None,
    };
    if let Some(s) = first_match(&p.impact, text) {
        return mk(
            Intent::Impact,
            vec![Identifier, Callers, Callees, Tests, Documents],
            Some(s),
        );
    }
    if let Some(s) = first_match(&p.callers, text) {
        return mk(Intent::Callers, vec![Identifier, Callers], Some(s));
    }
    if let Some(s) = first_match(&p.callees, text) {
        return mk(Intent::Callees, vec![Identifier, Callees], Some(s));
    }
    // A documentation keyword never overrides a code identifier: "docs for
    // SettlementService" needs the symbol and its documentation.
    if p.document.is_match(text) {
        if let Some(sym) = anchors.symbols.first() {
            let mut st = vec![Identifier, Fts, References, Documents];
            if semantic_enabled {
                st.push(Semantic);
            }
            return mk(Intent::General, st, Some(sym.clone()));
        }
        let mut st = vec![Fts, Documents];
        if semantic_enabled {
            st.push(Semantic);
        }
        return mk(Intent::Document, st, None);
    }
    if p.identifier.is_match(text) {
        return mk(
            Intent::Identifier,
            vec![Identifier, Fts],
            Some(text.to_owned()),
        );
    }
    let mut st = vec![Identifier, Fts, References];
    if semantic_enabled {
        st.push(Semantic);
    }
    mk(Intent::General, st, None)
}

/// The explore plan of `query` from the shared router ([`route::route`]):
/// one decision path for explore, evidence, ask and MCP (P02). An API
/// endpoint path anywhere in the question plans exact occurrences first,
/// then the code around them (callers), linked documents and a lexical
/// search focused on the anchor; semantic search is a fallback the caller
/// runs only when exact and lexical evidence fall short. Without anchors
/// the plan is the rule order of [`plan`].
pub fn plan_routed(query: &str, semantic_enabled: bool, opts: RouteOptions) -> QueryPlan {
    use Strategy::*;
    let r = route::route(query, opts);
    if r.intent == QueryIntent::EndpointReference {
        let mut st = vec![ExactOccurrences, Identifier, Callers, Documents, Fts];
        if semantic_enabled {
            st.push(Semantic);
        }
        return QueryPlan {
            query: query.to_owned(),
            intent: Intent::Endpoint,
            strategies: st,
            symbol: r.anchors.symbols.first().cloned(),
            anchors: r.anchors.clone(),
            route: Some(r),
        };
    }
    let mut p = plan_rules(query, semantic_enabled, &r.anchors);
    p.route = Some(r);
    p
}

// ---- evidence ------------------------------------------------------------

/// The confidence ladder, most trusted first.
pub fn confidence_rank(confidence: &str) -> u8 {
    match confidence {
        "exact" => 3,
        "high" => 2,
        "medium" => 1,
        _ => 0,
    }
}

fn highest<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    values.into_iter().max_by_key(|c| confidence_rank(c))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub struct EvidenceLocation {
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub page: Option<i64>,
    pub section: Option<String>,
}

/// A relationship-shaped fact in the evidence contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    pub source: String,
    pub path: String,
    pub location: EvidenceLocation,
    pub entity: String,
    pub relationship: String,
    pub confidence: String,
}

/// An evidence fact plus the snippet that lets a reader interpret it.
#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceItem {
    pub evidence: Evidence,
    pub snippet: String,
}

impl EvidenceItem {
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(&self.evidence).unwrap_or(Value::Null);
        v["snippet"] = json!(self.snippet);
        v
    }
}

/// One hop rendered as `source -[relationship]-> target`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphPath {
    pub source: String,
    pub relationship: String,
    pub target: String,
    pub confidence: String,
}

/// `context.*` budget (`ContextConfig` defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_chars: usize,
    pub max_files: usize,
    pub max_graph_nodes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_chars: 30_000,
            max_files: 20,
            max_graph_nodes: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextResult {
    pub evidence: Vec<Value>,
    pub graph_paths: Vec<GraphPath>,
    pub truncated: bool,
    pub truncation_reasons: Vec<String>,
    pub total_chars: usize,
    pub file_count: usize,
    pub graph_node_count: usize,
}

type LocKey = (
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

fn loc_key(i: &EvidenceItem) -> LocKey {
    let l = &i.evidence.location;
    (
        i.evidence.path.clone(),
        l.line_start,
        l.line_end,
        l.page,
        l.section.clone(),
    )
}

/// One item per location: the higher confidence wins, ties keep the first.
fn dedupe(items: Vec<EvidenceItem>) -> Vec<EvidenceItem> {
    let mut order: Vec<LocKey> = Vec::new();
    let mut best: HashMap<LocKey, EvidenceItem> = HashMap::new();
    for item in items {
        let key = loc_key(&item);
        match best.get(&key) {
            None => {
                order.push(key.clone());
                best.insert(key, item);
            }
            Some(cur)
                if confidence_rank(&item.evidence.confidence)
                    > confidence_rank(&cur.evidence.confidence) =>
            {
                best.insert(key, item);
            }
            Some(_) => {}
        }
    }
    order.into_iter().filter_map(|k| best.remove(&k)).collect()
}

/// Dedupes, sorts by confidence (descending; equal confidence keeps the
/// order the items were given in, which for ranked matches is the shared
/// fused ranking) and caps `items` to the budget's files and snippet
/// characters; caps `graph_paths` to `max_graph_nodes` distinct endpoints.
pub fn build_context(
    items: Vec<EvidenceItem>,
    graph_paths: Vec<GraphPath>,
    budget: Budget,
) -> ContextResult {
    let mut ordered = dedupe(items);
    ordered.sort_by(|a, b| {
        confidence_rank(&b.evidence.confidence).cmp(&confidence_rank(&a.evidence.confidence))
    });
    let mut included = Vec::new();
    let mut files: HashSet<String> = HashSet::new();
    let (mut total, mut files_exceeded, mut chars_exceeded) = (0usize, false, false);
    for item in ordered {
        if !files.contains(&item.evidence.path) && files.len() >= budget.max_files {
            files_exceeded = true;
            continue;
        }
        let n = item.snippet.chars().count();
        if total + n > budget.max_chars {
            chars_exceeded = true;
            continue;
        }
        files.insert(item.evidence.path.clone());
        total += n;
        included.push(item.to_json());
    }
    let mut nodes: HashSet<String> = HashSet::new();
    let mut kept = Vec::new();
    let mut graph_exceeded = false;
    for p in graph_paths {
        let mut candidate = nodes.clone();
        candidate.insert(p.source.clone());
        candidate.insert(p.target.clone());
        if candidate.len() > budget.max_graph_nodes {
            graph_exceeded = true;
            continue;
        }
        nodes = candidate;
        kept.push(p);
    }
    let mut reasons = Vec::new();
    if files_exceeded {
        reasons.push(format!(
            "evidence truncated: max_files={} reached",
            budget.max_files
        ));
    }
    if chars_exceeded {
        reasons.push(format!(
            "evidence truncated: max_chars={} reached",
            budget.max_chars
        ));
    }
    if graph_exceeded {
        reasons.push(format!(
            "graph paths truncated: max_graph_nodes={} reached",
            budget.max_graph_nodes
        ));
    }
    ContextResult {
        evidence: included,
        graph_paths: kept,
        truncated: !reasons.is_empty(),
        truncation_reasons: reasons,
        total_chars: total,
        file_count: files.len(),
        graph_node_count: nodes.len(),
    }
}

// ---- document links --------------------------------------------------------

/// One `(evidence, confidence)` per distinct `(document, chunk)` linked to
/// any match, in match then link order.
pub fn linked_document_evidence(
    corpora: &[Corpus<'_>],
    matches: &[SourceMatch],
) -> Result<Vec<Evidence>, SearchError> {
    let mut out = Vec::new();
    let mut seen: HashSet<(usize, String, Option<String>)> = HashSet::new();
    for m in matches {
        let c = corpora[m.corpus];
        for l in c.store.entity_links(c.build_id, &m.entity.id)? {
            if !seen.insert((m.corpus, l.document_id.clone(), l.chunk_id.clone())) {
                continue;
            }
            let pinned = l.chunk_id.is_some();
            out.push(Evidence {
                source: l.resolver,
                path: l.document_path,
                location: EvidenceLocation {
                    page: if pinned { l.chunk_page_start } else { None },
                    section: (pinned && !l.chunk_heading_path.is_empty())
                        .then(|| l.chunk_heading_path.join(" > ")),
                    ..Default::default()
                },
                entity: m.entity.qualified_name.clone(),
                relationship: l.link_type,
                confidence: l.confidence,
            });
        }
    }
    Ok(out)
}

fn evidence_json(e: &Evidence) -> Value {
    serde_json::to_value(e).unwrap_or(Value::Null)
}

// ---- impact ------------------------------------------------------------------

const LOW_MAX: usize = 2;
const MEDIUM_MAX: usize = 7;

/// `LOW` (≤ 2), `MEDIUM` (≤ 7) or `HIGH`.
pub fn blast_radius(score: usize) -> &'static str {
    if score <= LOW_MAX {
        "LOW"
    } else if score <= MEDIUM_MAX {
        "MEDIUM"
    } else {
        "HIGH"
    }
}

fn neighbor_names(edges: &[ResolvedEdge]) -> Vec<String> {
    edges
        .iter()
        .filter_map(|e| e.neighbor_entity.as_ref().map(|n| n.qualified_name.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The `impact` payload for `name`.
pub fn impact(
    corpora: &[Corpus<'_>],
    name: &str,
    max_depth: usize,
    limit: usize,
) -> Result<Value, SearchError> {
    let matches = graph::find_symbol_matches(corpora, name)?;
    if matches.is_empty() {
        return Ok(json!({"query": name, "found": false}));
    }
    let callers = graph::resolved_incoming(corpora, &matches, name, &CALL_TYPES, max_depth, limit)?;
    let callees = graph::resolved_outgoing(corpora, &matches, &CALL_TYPES, max_depth, limit)?;
    let tests = graph::find_tests_referencing(corpora, &matches, name, max_depth, limit)?;
    let docs = linked_document_evidence(corpora, &matches)?;
    let defined: Vec<Value> = matches
        .iter()
        .map(|m| {
            json!({
                "entity_id": m.entity.id,
                "qualified_name": m.entity.qualified_name,
                "path": if m.rel_path.is_empty() { &m.entity.file_id } else { &m.rel_path },
                "start_line": m.entity.start_line,
                "end_line": m.entity.end_line,
                "source_id": m.source_id,
            })
        })
        .collect();
    let code_conf = highest(
        callers
            .iter()
            .chain(&callees)
            .map(|e| e.edge.relationship.confidence.as_str()),
    );
    let doc_conf = highest(docs.iter().map(|d| d.confidence.as_str()));
    let distinct_callers: HashSet<(usize, &str)> = callers
        .iter()
        .filter_map(|e| {
            e.neighbor_entity
                .as_ref()
                .map(|n| (e.edge.corpus, n.id.as_str()))
        })
        .collect();
    let distinct_docs: HashSet<(&str, Option<&str>)> = docs
        .iter()
        .map(|d| (d.path.as_str(), d.location.section.as_deref()))
        .collect();
    let score = distinct_callers.len() + distinct_docs.len();
    Ok(json!({
        "query": name,
        "found": true,
        "defined": defined,
        "callers": neighbor_names(&callers),
        "callees": neighbor_names(&callees),
        "tests": neighbor_names(&tests),
        "documentation": docs.iter().map(evidence_json).collect::<Vec<_>>(),
        "confidence": {"code_references": code_conf, "document_links": doc_conf},
        "blast_radius": blast_radius(score),
        "blast_radius_score": score,
    }))
}

// ---- explore -----------------------------------------------------------------

fn tier_confidence(tier: RankTier) -> &'static str {
    match tier {
        RankTier::ExactSymbol => "exact",
        RankTier::QualifiedSymbol | RankTier::TitleOrHeading | RankTier::ExactPath => "high",
        RankTier::AliasSymbol | RankTier::Fts => "medium",
        RankTier::Path => "heuristic",
    }
}

/// A hit of the shared fused ranking as evidence: pinned hits keep their
/// tier's confidence; every other ranked hit is a `medium` match (its
/// place in the ranking already weighs its signals), a semantic-only one
/// related as `similar_to`.
fn ranked_evidence(h: &crate::hybrid::RankedHit) -> EvidenceItem {
    let c = &h.candidate;
    let loc = c.location.clone().unwrap_or_default();
    let lexical = c.lexical_rank.is_some();
    EvidenceItem {
        evidence: Evidence {
            source: format!(
                "{}:{}",
                if lexical { "lexical" } else { "semantic" },
                c.kind
            ),
            path: c.path.clone(),
            location: EvidenceLocation {
                line_start: loc.line_start,
                line_end: loc.line_end,
                page: loc.page_start,
                section: loc.section,
            },
            entity: c.title.clone(),
            relationship: if lexical { "match" } else { "similar_to" }.into(),
            confidence: match (c.exact_match, c.lexical_tier) {
                (true, Some(t)) => tier_confidence(t),
                _ => "medium",
            }
            .into(),
        },
        snippet: c.snippet.clone().unwrap_or_default(),
    }
}

/// Where a matched symbol is defined (its own evidence, ahead of the graph
/// around it).
fn definition_evidence(m: &SourceMatch) -> EvidenceItem {
    let e = &m.entity;
    EvidenceItem {
        evidence: Evidence {
            source: "symbol".into(),
            path: if m.rel_path.is_empty() {
                e.file_id.clone()
            } else {
                m.rel_path.clone()
            },
            location: EvidenceLocation {
                line_start: Some(e.start_line),
                line_end: Some(e.end_line),
                ..Default::default()
            },
            entity: e.qualified_name.clone(),
            relationship: "defines".into(),
            confidence: "exact".into(),
        },
        snippet: e
            .signature
            .clone()
            .unwrap_or_else(|| e.qualified_name.clone()),
    }
}

fn edge_evidence(e: &ResolvedEdge) -> Option<EvidenceItem> {
    let n = e.neighbor_entity.as_ref()?;
    let r = &e.edge.relationship;
    Some(EvidenceItem {
        evidence: Evidence {
            source: r.resolver.clone(),
            path: e.neighbor_path.clone().unwrap_or_else(|| n.file_id.clone()),
            location: EvidenceLocation {
                line_start: Some(n.start_line),
                line_end: Some(n.end_line),
                ..Default::default()
            },
            entity: n.qualified_name.clone(),
            relationship: r.relationship_type.clone(),
            confidence: r.confidence.clone(),
        },
        snippet: n
            .signature
            .clone()
            .unwrap_or_else(|| n.qualified_name.clone()),
    })
}

fn graph_path(e: &ResolvedEdge, incoming: bool, symbol: &str) -> GraphPath {
    let r = &e.edge.relationship;
    let other = e
        .neighbor_entity
        .as_ref()
        .map(|n| n.qualified_name.clone())
        .or_else(|| r.target_symbol.clone())
        .unwrap_or_else(|| "?".into());
    let (source, target) = if incoming {
        (other, symbol.to_owned())
    } else {
        (symbol.to_owned(), other)
    };
    GraphPath {
        source,
        relationship: r.relationship_type.clone(),
        target,
        confidence: r.confidence.clone(),
    }
}

/// A search result as JSON (location keys per hit kind).
pub fn search_result_json(r: &SearchResult) -> Value {
    let location = r.location.as_ref().map(|l| match r.kind {
        "entity" | "passage" => json!({"line_start": l.line_start, "line_end": l.line_end}),
        _ if r.tier == RankTier::TitleOrHeading
            && l.section.is_none()
            && l.heading_path.is_empty() =>
        {
            json!({"attachment": l.attachment})
        }
        _ => {
            let mut v = json!({
                "section": l.section,
                "page_start": l.page_start,
                "page_end": l.page_end,
                "heading_path": l.heading_path,
            });
            if let Some(a) = &l.attachment {
                v["attachment"] = json!(a);
            }
            v
        }
    });
    json!({
        "kind": r.kind,
        "tier": r.tier.label(),
        "id": r.id,
        "title": r.title,
        "path": r.path,
        "source_id": r.source_id,
        "snippet": r.snippet,
        "location": location,
    })
}

pub fn semantic_hit_json(h: &crate::hybrid::SourcedHit) -> Value {
    let s = &h.hit;
    let location = if s.kind == "entity" || s.kind == "passage" {
        json!({"line_start": s.position, "line_end": s.end_line})
    } else {
        let mut v = json!({"section": s.section});
        if let Some(a) = &h.attachment {
            v["attachment"] = json!(a);
        }
        v
    };
    json!({
        "kind": s.kind,
        "id": s.id,
        "title": s.title,
        "path": s.path,
        "source_id": h.source_id,
        "score": s.score,
        "snippet": s.snippet,
        "location": location,
    })
}

fn semantic_evidence(h: &crate::hybrid::SourcedHit) -> EvidenceItem {
    let s = &h.hit;
    EvidenceItem {
        evidence: Evidence {
            source: format!("semantic:{}", s.kind),
            path: s.path.clone(),
            location: EvidenceLocation {
                line_start: (s.kind == "entity" || s.kind == "passage").then_some(s.position),
                line_end: s.end_line,
                page: None,
                section: s.section.clone(),
            },
            entity: s.title.clone(),
            relationship: "similar_to".into(),
            confidence: "heuristic".into(),
        },
        snippet: s.snippet.clone(),
    }
}

/// Exact-occurrence settings of an explore run
/// (`search.exact_occurrences.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactSettings {
    pub enabled: bool,
    pub max_results: usize,
    /// Verify document chunks too.
    pub documents: bool,
    pub max_template_variants: usize,
}

impl Default for ExactSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_results: 50,
            documents: true,
            max_template_variants: 4,
        }
    }
}

/// Inputs `explore` does not compute itself.
#[derive(Debug, Clone, Copy)]
pub struct ExploreOptions {
    pub budget: Budget,
    /// `search.output.snippet_max_tokens`.
    pub snippet_tokens: i64,
    pub exact: ExactSettings,
    /// `search.adaptive_lexical.enabled`: with verified exact evidence the
    /// loosest lexical tier is skipped.
    pub adaptive_lexical: bool,
    /// `search.selective_graph.enabled`: lexical and graph stages read only
    /// the sources with exact seeds once there are any.
    pub selective_graph: bool,
    /// `search.selective_graph.max_seed_entities`.
    pub max_seed_entities: usize,
    /// Collect the `explain` diagnostics payload.
    pub explain: bool,
    /// `search.reranker.top_n` when the neural pass is enabled.
    pub rerank_top_n: Option<usize>,
    /// Lexical candidates the shared fusion ranks
    /// ([`crate::pipeline::candidate_budget`]); listings keep the first
    /// [`LEXICAL_LISTING`].
    pub candidates: usize,
}

/// Lexical hits explore lists (documents, symbols' files).
pub const LEXICAL_LISTING: usize = 25;

impl Default for ExploreOptions {
    fn default() -> Self {
        Self {
            budget: Budget::default(),
            snippet_tokens: 32,
            exact: ExactSettings::default(),
            adaptive_lexical: true,
            selective_graph: true,
            max_seed_entities: 25,
            explain: false,
            rerank_top_n: None,
            candidates: crate::pipeline::candidate_budget(0, 10),
        }
    }
}

/// Per-stage timings and counters of one explore run (P00). Durations
/// are monotonic wall-clock milliseconds of each stage on this thread.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Diagnostics {
    pub stages: Vec<lexical::StageTiming>,
    pub source_count_considered: usize,
    pub source_count_searched: usize,
    pub source_count_expanded: usize,
    pub graph_seeds: usize,
    pub graph_edges: usize,
    pub traversal_truncated: bool,
    pub lexical_query: Option<String>,
    pub lexical_or_fallback: bool,
    pub semantic: &'static str,
    pub exact_reads: usize,
    pub exact_rejected_document_candidates: usize,
    pub exact_truncated: bool,
}

impl Diagnostics {
    fn time<T>(
        &mut self,
        stage: &'static str,
        hits: impl Fn(&T) -> usize,
        run: impl FnOnce() -> Result<T, SearchError>,
    ) -> Result<T, SearchError> {
        crate::check_deadline()?;
        let t = std::time::Instant::now();
        let out = run()?;
        self.stages.push(lexical::StageTiming {
            stage,
            hits: hits(&out),
            duration_ms: t.elapsed().as_secs_f64() * 1000.0,
        });
        Ok(out)
    }
}

/// What the lexical and exact stages found, for the caller's lazy
/// semantic decision (P07).
pub struct ExploreProgress<'r> {
    pub plan: &'r QueryPlan,
    pub lexical: &'r [SearchResult],
    /// Verified exact occurrences or matched symbols exist.
    pub exact_evidence: bool,
}

fn occurrence_confidence(class: EvidenceClass) -> &'static str {
    match class {
        EvidenceClass::DirectExactLiteral | EvidenceClass::DocumentReference => "exact",
        EvidenceClass::StaticallyComposedRoute | EvidenceClass::ResolvedCodeRelationship => "high",
        _ => "heuristic",
    }
}

fn occurrence_evidence(o: &Occurrence) -> EvidenceItem {
    EvidenceItem {
        evidence: Evidence {
            source: o.resolver.clone(),
            path: o.path.clone(),
            location: EvidenceLocation {
                line_start: o.line,
                line_end: o.line,
                page: o.page,
                section: o.section.clone(),
            },
            entity: o.entity.clone().unwrap_or_else(|| o.anchor.clone()),
            relationship: o.class.as_str().into(),
            confidence: occurrence_confidence(o.class).into(),
        },
        snippet: match &o.http_method {
            Some(m) => format!("{m} {}", o.matched),
            None => o.matched.clone(),
        },
    }
}

fn occurrence_json(o: &Occurrence) -> Value {
    let mut v = serde_json::to_value(o).unwrap_or(Value::Null);
    v["evidence_class"] = json!(o.class.as_str());
    v
}

/// Graph seeds of the code occurrences: their owning entities, one per
/// `(corpus, entity)`, at most `max`. Heuristic matches seed only when
/// there is no stronger occurrence.
fn occurrence_seeds(outcome: &ExactOutcome, max: usize) -> (Vec<SourceMatch>, bool) {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut truncated = false;
    let strong = outcome
        .occurrences
        .iter()
        .any(|o| o.entity_row.is_some() && o.class != EvidenceClass::HeuristicMatch);
    for o in &outcome.occurrences {
        if strong && o.class == EvidenceClass::HeuristicMatch {
            continue;
        }
        let Some(e) = &o.entity_row else {
            continue;
        };
        if !seen.insert((o.corpus, e.id.clone())) {
            continue;
        }
        if out.len() >= max {
            truncated = true;
            break;
        }
        out.push(SourceMatch {
            corpus: o.corpus,
            source_id: o.source_id.clone(),
            entity: e.clone(),
            rel_path: o.path.clone(),
        });
    }
    (out, truncated)
}

/// The `explore` payload for `query_plan`. `semantic` is the
/// semantic search outcome when the plan includes [`Strategy::Semantic`]
/// (the caller runs it, since it needs the embedder).
pub fn explore(
    corpora: &[Corpus<'_>],
    query_plan: &QueryPlan,
    semantic: Option<&SemanticOutcome>,
    opts: ExploreOptions,
) -> Result<Value, SearchError> {
    explore_with(corpora, query_plan, &mut |_| Ok(semantic.cloned()), opts)
}

/// [`explore`] with a lazy semantic stage: `semantic` is called once,
/// after the exact, lexical and graph stages, only when the plan includes
/// [`Strategy::Semantic`]; it sees what those stages found and returns
/// `None` to skip the embedding (P07).
pub fn explore_with(
    corpora: &[Corpus<'_>],
    query_plan: &QueryPlan,
    semantic: &mut dyn FnMut(&ExploreProgress<'_>) -> Result<Option<SemanticOutcome>, SearchError>,
    opts: ExploreOptions,
) -> Result<Value, SearchError> {
    explore_ranked(corpora, query_plan, semantic, opts, None)
}

/// [`explore_with`] with the cross-encoder of the shared ranking path
/// (used when `opts.rerank_top_n` is set).
pub fn explore_ranked(
    corpora: &[Corpus<'_>],
    query_plan: &QueryPlan,
    semantic: &mut dyn FnMut(&ExploreProgress<'_>) -> Result<Option<SemanticOutcome>, SearchError>,
    opts: ExploreOptions,
    encoder: Option<&docmonk_ml::reranker::CrossEncoder>,
) -> Result<Value, SearchError> {
    use Strategy::*;
    let has = |s: Strategy| query_plan.strategies.contains(&s);
    let mut diag = Diagnostics {
        source_count_considered: corpora.len(),
        semantic: "not_planned",
        ..Diagnostics::default()
    };
    let endpoint = query_plan.intent == Intent::Endpoint;

    // 1. Exact occurrences of URL anchors (indexed, verified).
    let outcome = if has(ExactOccurrences) && opts.exact.enabled {
        let ex = exact::ExactOptions {
            max_results: opts.exact.max_results,
            documents: opts.exact.documents,
            filter: Default::default(),
            snippet_tokens: opts.snippet_tokens,
            max_template_variants: opts.exact.max_template_variants,
        };
        let o = diag.time(
            "exact_occurrences",
            |o: &ExactOutcome| o.occurrences.len(),
            || exact::find_occurrences(corpora, &query_plan.anchors.url_paths, &ex),
        )?;
        diag.exact_reads = o.reads;
        diag.exact_rejected_document_candidates = o.rejected_document_candidates;
        diag.exact_truncated = o.truncated;
        Some(o)
    } else {
        None
    };
    let exact_found = outcome.as_ref().is_some_and(|o| {
        o.occurrences
            .iter()
            .any(|x| x.class != EvidenceClass::HeuristicMatch)
    });
    // Selective stages (P05): once exact seeds exist, later stages read
    // only their sources (an optimization, never a scope change: hard
    // filters were applied to `corpora` by the caller).
    let selected: Vec<usize> = match &outcome {
        Some(o) if opts.selective_graph && exact_found => o.candidate_corpora.clone(),
        _ => (0..corpora.len()).collect(),
    };
    let scoped: Vec<Corpus<'_>> = selected.iter().map(|&i| corpora[i]).collect();
    // Indexes into `scoped` map back to `corpora` through `selected`.
    let remap = |i: usize| selected[i];

    // 2. Lexical, focused on the anchor for endpoint questions.
    let symbol_query = query_plan
        .symbol
        .clone()
        .unwrap_or_else(|| query_plan.query.clone());
    let lexical_text = if endpoint {
        query_plan
            .anchors
            .url_paths
            .iter()
            .map(|u| u.path.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        query_plan.query.clone()
    };
    let mut lex_opts = lexical::LexicalOptions::default();
    lex_opts.plan.or_fallback = !(opts.adaptive_lexical && exact_found);
    diag.lexical_or_fallback = lex_opts.plan.or_fallback;
    let (lexical_results, lexical_candidates) = if has(Fts) || has(Identifier) {
        diag.lexical_query = Some(lexical_text.clone());
        diag.source_count_searched = scoped.len();
        let (mut rows, stages) = diag.time(
            "lexical",
            |(r, _): &(Vec<SearchResult>, Vec<lexical::StageTiming>)| r.len(),
            || {
                lexical::search_with_options(
                    &scoped,
                    &lexical_text,
                    opts.candidates.max(LEXICAL_LISTING),
                    opts.snippet_tokens,
                    &lex_opts,
                )
            },
        )?;
        if opts.explain {
            diag.stages.extend(stages.into_iter().map(|mut t| {
                t.stage = match t.stage {
                    "prefetch" => "lexical.prefetch",
                    "entities" => "lexical.entities",
                    "passages" => "lexical.passages",
                    "documents" => "lexical.documents",
                    "paths" => "lexical.paths",
                    "merge" => "lexical.merge",
                    other => other,
                };
                t
            }));
        }
        let candidates = rows.clone();
        rows.truncate(LEXICAL_LISTING);
        (rows, candidates)
    } else {
        (Vec::new(), Vec::new())
    };

    // 3. Symbols: exact seeds' owners (endpoint), else the named symbol.
    let mut matches: Vec<SourceMatch> = Vec::new();
    if let Some(o) = &outcome {
        let (seeds, truncated) = occurrence_seeds(o, opts.max_seed_entities);
        diag.traversal_truncated |= truncated;
        matches.extend(seeds);
    }
    if has(Identifier) && (!endpoint || query_plan.symbol.is_some()) {
        let found = diag.time(
            "symbols",
            |m: &Vec<SourceMatch>| m.len(),
            || graph::find_symbol_matches(&scoped, &symbol_query),
        )?;
        for mut m in found {
            m.corpus = remap(m.corpus);
            if !matches
                .iter()
                .any(|x| x.corpus == m.corpus && x.entity.id == m.entity.id)
            {
                matches.push(m);
            }
        }
    }
    diag.graph_seeds = matches.len();
    let expanded: BTreeSet<usize> = matches.iter().map(|m| m.corpus).collect();
    diag.source_count_expanded = expanded.len();

    // 4. Graph expansion from the seeds only (bounded depth and edges).
    let (mut callers, mut callees, mut tests, mut references) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    if !matches.is_empty() {
        let seed_name = if endpoint {
            matches[0].entity.name.clone()
        } else {
            symbol_query.clone()
        };
        if has(Callers) {
            callers = diag.time(
                "graph.callers",
                |e: &Vec<ResolvedEdge>| e.len(),
                || {
                    graph::resolved_incoming(
                        corpora,
                        &matches,
                        &seed_name,
                        &CALL_TYPES,
                        DEFAULT_MAX_DEPTH,
                        DEFAULT_LIMIT,
                    )
                },
            )?;
        }
        if has(Callees) {
            callees = diag.time(
                "graph.callees",
                |e: &Vec<ResolvedEdge>| e.len(),
                || {
                    graph::resolved_outgoing(
                        corpora,
                        &matches,
                        &CALL_TYPES,
                        DEFAULT_MAX_DEPTH,
                        DEFAULT_LIMIT,
                    )
                },
            )?;
        }
        if has(Tests) {
            tests = diag.time(
                "graph.tests",
                |e: &Vec<ResolvedEdge>| e.len(),
                || {
                    graph::find_tests_referencing(
                        corpora,
                        &matches,
                        &symbol_query,
                        DEFAULT_MAX_DEPTH,
                        DEFAULT_LIMIT,
                    )
                },
            )?;
        }
        // The planned References strategy (F07): incoming `references`
        // edges of the matched symbols.
        if has(References) {
            references = diag.time(
                "graph.references",
                |e: &Vec<ResolvedEdge>| e.len(),
                || {
                    graph::resolved_incoming(
                        corpora,
                        &matches,
                        &symbol_query,
                        &["references"],
                        DEFAULT_MAX_DEPTH,
                        DEFAULT_LIMIT,
                    )
                },
            )?;
        }
    }
    diag.graph_edges = callers.len() + callees.len() + tests.len() + references.len();
    diag.traversal_truncated |= [&callers, &callees, &tests, &references]
        .iter()
        .any(|e| e.len() >= DEFAULT_LIMIT);
    let doc_links = if !matches.is_empty() && has(Documents) {
        diag.time(
            "documents.links",
            |d: &Vec<Evidence>| d.len(),
            || linked_document_evidence(corpora, &matches),
        )?
    } else {
        Vec::new()
    };

    // 5. Semantic, lazily (the caller decides with what was found).
    let semantic = if has(Semantic) && endpoint && exact_found {
        // Verified exact evidence answers an endpoint question: the query
        // is never embedded (P07).
        diag.semantic = "skipped_exact_evidence";
        None
    } else if has(Semantic) {
        let progress = ExploreProgress {
            plan: query_plan,
            lexical: &lexical_results,
            exact_evidence: exact_found || !matches.is_empty(),
        };
        let s = diag.time(
            "semantic",
            |s: &Option<SemanticOutcome>| s.as_ref().map_or(0, |s| s.hits.len()),
            || semantic(&progress),
        )?;
        diag.semantic = if s.is_some() { "ran" } else { "skipped" };
        s
    } else {
        None
    };
    let semantic = semantic.as_ref();
    // The shared ranking (ADR 0038): lexical and semantic candidates fused
    // exactly as `search --hybrid` and `evidence` fuse them.
    let fused = diag.time(
        "fuse",
        |f: &crate::pipeline::Fused| f.hits.len(),
        || {
            crate::pipeline::fuse(
                corpora,
                &lexical_text,
                &lexical_candidates,
                semantic.map_or(&[][..], |s| s.hits.as_slice()),
                crate::pipeline::FuseOptions {
                    limit: 25,
                    rerank_top_n: opts.rerank_top_n,
                    per_document_cap: None,
                    term_coverage: query_plan
                        .route
                        .as_ref()
                        .is_some_and(|r| crate::pipeline::orders_by_coverage(r.intent)),
                },
                encoder,
            )
        },
    )?;

    // 6. Evidence and context.
    let names: HashMap<(usize, &str), &str> = matches
        .iter()
        .map(|m| {
            (
                (m.corpus, m.entity.id.as_str()),
                m.entity.qualified_name.as_str(),
            )
        })
        .collect();
    let symbol_display = matches
        .first()
        .map_or_else(|| symbol_query.clone(), |m| m.entity.qualified_name.clone());
    let display_for = |e: &ResolvedEdge| -> String {
        let r = &e.edge.relationship;
        let id = r.target_entity_id.as_deref().unwrap_or("");
        names
            .get(&(e.edge.corpus, id))
            .map_or_else(|| symbol_display.clone(), |n| (*n).to_owned())
    };
    let mut items = Vec::new();
    let mut paths = Vec::new();
    if let Some(o) = &outcome {
        items.extend(o.occurrences.iter().map(occurrence_evidence));
    }
    // The matched symbols' own definitions, when the graph has no edges to
    // answer with (a navigation question's callers stay its evidence).
    if callers.is_empty() && callees.is_empty() && references.is_empty() {
        items.extend(matches.iter().map(definition_evidence));
    }
    for e in &callers {
        items.extend(edge_evidence(e));
        let target = if endpoint {
            display_for(e)
        } else {
            symbol_display.clone()
        };
        paths.push(graph_path(e, true, &target));
    }
    for e in &callees {
        items.extend(edge_evidence(e));
        paths.push(graph_path(e, false, &symbol_display));
    }
    for e in &references {
        items.extend(edge_evidence(e));
        paths.push(graph_path(e, true, &symbol_display));
    }
    for ev in &doc_links {
        items.push(EvidenceItem {
            snippet: ev.entity.clone(),
            evidence: ev.clone(),
        });
    }
    if matches.is_empty() && !exact_found {
        // No symbol or exact anchor: the fused ranking is the evidence.
        items.extend(
            fused
                .hits
                .iter()
                .filter(|h| {
                    h.candidate
                        .snippet
                        .as_deref()
                        .is_some_and(|s| !s.is_empty())
                })
                .map(ranked_evidence),
        );
    } else if let Some(s) = semantic {
        items.extend(s.hits.iter().map(semantic_evidence));
    }
    let ctx = build_context(items, paths.clone(), opts.budget);

    let documents: Vec<Value> = lexical_results
        .iter()
        .filter(|r| r.kind == "document")
        .map(search_result_json)
        .collect();
    let mut rel_paths: BTreeSet<&str> = lexical_results.iter().map(|r| r.path.as_str()).collect();
    if let Some(o) = &outcome {
        rel_paths.extend(o.occurrences.iter().map(|x| x.path.as_str()));
    }
    let summary = match &outcome {
        Some(o) => format!(
            "Found {} exact occurrence(s), {} symbol(s) and {} document(s) matching '{}'; see below for details.",
            o.occurrences.len(),
            matches.len(),
            documents.len(),
            query_plan.query
        ),
        None => format!(
            "Found {} symbol(s) and {} document(s) matching '{}'; see below for details.",
            matches.len(),
            documents.len(),
            query_plan.query
        ),
    };
    let mut out = json!({
        "query": query_plan.query,
        "intent": query_plan.intent.as_str(),
        "strategies": query_plan.strategies.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        "summary": summary,
        "symbols": matches.iter().map(|m| json!({
            "entity_id": m.entity.id,
            "qualified_name": m.entity.qualified_name,
            "kind": m.entity.kind,
            "source_id": m.source_id,
        })).collect::<Vec<_>>(),
        "paths": rel_paths,
        "call_flows": paths.iter().filter(|p| p.relationship == "calls").collect::<Vec<_>>(),
        "dependencies": callees.iter().filter_map(|e| e.neighbor_entity.as_ref().map(|n| json!({
            "qualified_name": n.qualified_name,
            "relationship": e.edge.relationship.relationship_type,
        }))).collect::<Vec<_>>(),
        "documents": documents,
        "tests": neighbor_names(&tests),
        "requirements": [],
        "incidents": [],
        "evidence": ctx.evidence,
        "evidence_truncated": ctx.truncated,
        "evidence_truncation_reasons": ctx.truncation_reasons,
        "semantic_results": semantic.map_or_else(Vec::new, |s| s.hits.iter().map(semantic_hit_json).collect()),
        "semantic_available": semantic.map(|s| s.available),
        "semantic_reason": semantic.map(|s| s.reason.clone()),
    });
    // Additive keys, present only when they carry information.
    // Technical anchors (paths) are reported with the payload; symbol-only
    // anchors are in the `--explain` route.
    let a = &query_plan.anchors;
    if !a.url_paths.is_empty() || !a.file_paths.is_empty() {
        out["anchors"] = json!(a);
    }
    if !references.is_empty() {
        out["references"] = json!(paths
            .iter()
            .filter(|p| p.relationship == "references")
            .collect::<Vec<_>>());
    }
    if let Some(o) = &outcome {
        out["exact_occurrences"] = json!(o
            .occurrences
            .iter()
            .map(occurrence_json)
            .collect::<Vec<_>>());
        out["exact_occurrences_truncated"] = json!(o.truncated);
        // A source that could not answer is never reported as "absent".
        out["exact_capabilities"] = json!(o.capabilities);
        let incomplete: Vec<&str> = o
            .capabilities
            .iter()
            .filter(|c| c.exact_occurrences != "indexed")
            .map(|c| c.source_id.as_str())
            .collect();
        out["exact_complete"] = json!(incomplete.is_empty());
        if !incomplete.is_empty() {
            out["exact_unchecked_sources"] = json!(incomplete);
        }
    }
    if opts.explain {
        // `lexical.*` entries are sub-stages of `lexical`.
        let total: f64 = diag
            .stages
            .iter()
            .filter(|t| !t.stage.starts_with("lexical."))
            .map(|t| t.duration_ms)
            .sum();
        let mut d = serde_json::to_value(&diag).unwrap_or(Value::Null);
        d["stages_total_ms"] = json!(total);
        if let Some(r) = &query_plan.route {
            d["route"] = json!(r);
        }
        d["reranker"] = json!(fused.reranker);
        out["diagnostics"] = d;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(path: &str, line: Option<i64>, conf: &str, snippet: &str) -> EvidenceItem {
        EvidenceItem {
            evidence: Evidence {
                source: "s".into(),
                path: path.into(),
                location: EvidenceLocation {
                    line_start: line,
                    ..Default::default()
                },
                entity: format!("{path}:{line:?}"),
                relationship: "calls".into(),
                confidence: conf.into(),
            },
            snippet: snippet.into(),
        }
    }

    #[test]
    fn planner_matches_reference_examples() {
        let p = |q: &str| {
            let r = plan(q, false);
            (
                r.intent,
                r.strategies.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                r.symbol,
            )
        };
        assert_eq!(
            p("BetSettled"),
            (
                Intent::Identifier,
                vec!["identifier", "fts"],
                Some("BetSettled".into())
            )
        );
        assert_eq!(
            p("who calls SettlementService?"),
            (
                Intent::Callers,
                vec!["identifier", "graph_callers"],
                Some("SettlementService".into())
            )
        );
        assert_eq!(p("What breaks if a.b changes?").0, Intent::Impact);
        assert_eq!(
            p("what does Ledger.post call").2.as_deref(),
            Some("Ledger.post")
        );
        assert_eq!(
            p("documents about delayed settlement").1,
            vec!["fts", "documents"]
        );
        assert_eq!(
            plan("documents about x", true).strategies.last(),
            Some(&Strategy::Semantic)
        );
        assert_eq!(p("how does settlement work").0, Intent::General);
    }

    #[test]
    fn context_dedupes_prioritizes_and_truncates() {
        let items = vec![
            ev("b.py", Some(1), "medium", "xx"),
            ev("b.py", Some(1), "exact", "yy"),
            ev("a.py", None, "medium", "zz"),
            ev("c.py", Some(3), "heuristic", "0123456789"),
        ];
        let r = build_context(
            items,
            vec![],
            Budget {
                max_chars: 6,
                max_files: 5,
                max_graph_nodes: 2,
            },
        );
        let got: Vec<(String, String)> = r
            .evidence
            .iter()
            .map(|e| {
                (
                    e["path"].as_str().unwrap().into(),
                    e["confidence"].as_str().unwrap().into(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("b.py".into(), "exact".into()),
                ("a.py".into(), "medium".into())
            ]
        );
        assert!(r.truncated);
        assert_eq!(
            r.truncation_reasons,
            ["evidence truncated: max_chars=6 reached"]
        );
        let gp = |s: &str, t: &str| GraphPath {
            source: s.into(),
            relationship: "calls".into(),
            target: t.into(),
            confidence: "exact".into(),
        };
        let r = build_context(
            vec![],
            vec![gp("a", "b"), gp("a", "c"), gp("b", "a")],
            Budget {
                max_graph_nodes: 2,
                ..Budget::default()
            },
        );
        assert_eq!(r.graph_paths.len(), 2);
        assert_eq!(r.graph_node_count, 2);
    }

    #[test]
    fn blast_radius_buckets() {
        assert_eq!(
            [0, 2, 3, 7, 8].map(blast_radius),
            ["LOW", "LOW", "MEDIUM", "MEDIUM", "HIGH"]
        );
    }
}
