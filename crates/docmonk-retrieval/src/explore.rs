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

use crate::graph::{self, ResolvedEdge, SourceMatch, CALL_TYPES, DEFAULT_LIMIT, DEFAULT_MAX_DEPTH};
use crate::hybrid::SemanticOutcome;
use crate::lexical::{self, RankTier, SearchResult};
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    pub query: String,
    pub intent: Intent,
    pub strategies: Vec<Strategy>,
    pub symbol: Option<String>,
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
pub fn plan(query: &str, semantic_enabled: bool) -> QueryPlan {
    use Strategy::*;
    let p = patterns();
    let text = query.trim();
    let mk = |intent, strategies: Vec<Strategy>, symbol| QueryPlan {
        query: query.to_owned(),
        intent,
        strategies,
        symbol,
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
    if p.document.is_match(text) {
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

/// Dedupes, sorts (confidence desc, then path, line, page, entity; `None`
/// last) and caps `items` to the budget's files and snippet characters;
/// caps `graph_paths` to `max_graph_nodes` distinct endpoints.
pub fn build_context(
    items: Vec<EvidenceItem>,
    graph_paths: Vec<GraphPath>,
    budget: Budget,
) -> ContextResult {
    let mut ordered = dedupe(items);
    let none_last = |v: Option<i64>| (v.is_none(), v.unwrap_or(0));
    ordered.sort_by(|a, b| {
        let (la, lb) = (&a.evidence.location, &b.evidence.location);
        confidence_rank(&b.evidence.confidence)
            .cmp(&confidence_rank(&a.evidence.confidence))
            .then_with(|| a.evidence.path.cmp(&b.evidence.path))
            .then(none_last(la.line_start).cmp(&none_last(lb.line_start)))
            .then(none_last(la.page).cmp(&none_last(lb.page)))
            .then_with(|| a.evidence.entity.cmp(&b.evidence.entity))
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
        RankTier::QualifiedSymbol | RankTier::TitleOrHeading => "high",
        RankTier::AliasSymbol | RankTier::Fts => "medium",
        RankTier::Path => "heuristic",
    }
}

fn lexical_evidence(r: &SearchResult) -> EvidenceItem {
    let loc = r.location.clone().unwrap_or_default();
    EvidenceItem {
        evidence: Evidence {
            source: format!("lexical:{}", r.kind),
            path: r.path.clone(),
            location: EvidenceLocation {
                line_start: loc.line_start,
                line_end: loc.line_end,
                page: None,
                section: loc.section,
            },
            entity: r.title.clone(),
            relationship: "match".into(),
            confidence: tier_confidence(r.tier).into(),
        },
        snippet: r.snippet.clone().unwrap_or_default(),
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
        "entity" => json!({"line_start": l.line_start, "line_end": l.line_end}),
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
    let location = if s.kind == "entity" {
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
                line_start: (s.kind == "entity").then_some(s.position),
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

/// Inputs `explore` does not compute itself.
#[derive(Debug, Clone, Copy)]
pub struct ExploreOptions {
    pub budget: Budget,
    /// `search.output.snippet_max_tokens`.
    pub snippet_tokens: i64,
}

impl Default for ExploreOptions {
    fn default() -> Self {
        Self {
            budget: Budget::default(),
            snippet_tokens: 32,
        }
    }
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
    use Strategy::*;
    let has = |s: Strategy| query_plan.strategies.contains(&s);
    let symbol_query = query_plan
        .symbol
        .clone()
        .unwrap_or_else(|| query_plan.query.clone());
    let lexical_results = if has(Fts) || has(Identifier) {
        lexical::search(corpora, &query_plan.query, 25, opts.snippet_tokens)?
    } else {
        Vec::new()
    };
    let matches = if has(Identifier) {
        graph::find_symbol_matches(corpora, &symbol_query)?
    } else {
        Vec::new()
    };
    let (mut callers, mut callees, mut tests) = (Vec::new(), Vec::new(), Vec::new());
    if !matches.is_empty() {
        if has(Callers) {
            callers = graph::resolved_incoming(
                corpora,
                &matches,
                &symbol_query,
                &CALL_TYPES,
                DEFAULT_MAX_DEPTH,
                DEFAULT_LIMIT,
            )?;
        }
        if has(Callees) {
            callees = graph::resolved_outgoing(
                corpora,
                &matches,
                &CALL_TYPES,
                DEFAULT_MAX_DEPTH,
                DEFAULT_LIMIT,
            )?;
        }
        if has(Tests) {
            tests = graph::find_tests_referencing(
                corpora,
                &matches,
                &symbol_query,
                DEFAULT_MAX_DEPTH,
                DEFAULT_LIMIT,
            )?;
        }
    }
    let doc_links = if !matches.is_empty() && has(Documents) {
        linked_document_evidence(corpora, &matches)?
    } else {
        Vec::new()
    };
    let symbol_display = matches
        .first()
        .map_or_else(|| symbol_query.clone(), |m| m.entity.qualified_name.clone());
    let semantic = if has(Semantic) { semantic } else { None };

    let mut items = Vec::new();
    let mut paths = Vec::new();
    for e in &callers {
        items.extend(edge_evidence(e));
        paths.push(graph_path(e, true, &symbol_display));
    }
    for e in &callees {
        items.extend(edge_evidence(e));
        paths.push(graph_path(e, false, &symbol_display));
    }
    for ev in &doc_links {
        items.push(EvidenceItem {
            snippet: ev.entity.clone(),
            evidence: ev.clone(),
        });
    }
    if matches.is_empty() {
        items.extend(
            lexical_results
                .iter()
                .filter(|r| r.snippet.as_deref().is_some_and(|s| !s.is_empty()))
                .map(lexical_evidence),
        );
    }
    if let Some(s) = semantic {
        items.extend(s.hits.iter().map(semantic_evidence));
    }
    let ctx = build_context(items, paths.clone(), opts.budget);

    let documents: Vec<Value> = lexical_results
        .iter()
        .filter(|r| r.kind == "document")
        .map(search_result_json)
        .collect();
    let rel_paths: BTreeSet<&str> = lexical_results.iter().map(|r| r.path.as_str()).collect();
    let summary = format!(
        "Found {} symbol(s) and {} document(s) matching '{}'; see below for details.",
        matches.len(),
        documents.len(),
        query_plan.query
    );
    Ok(json!({
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
    }))
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
