//! Adaptive, deterministic query routing (ADR 0033, P0-R01).
//!
//! [`route`] classifies a query into a typed [`QueryIntent`] with an
//! explainable confidence and the retrieval strategies that intent needs.
//! Identifier and path cues are checked first, then scored phrase rules;
//! anything weakly scored falls back to [`QueryIntent::Ambiguous`], whose
//! plan is the conservative lexical + (when available) semantic mix.
//! Routing never calls a network service or a model, and it never decides
//! *what* may be searched: hard [`SearchFilters`] are applied separately,
//! before every retrieval stage, and no strategy can widen them.

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

/// The evidence a query needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    ExactSymbol,
    PathLookup,
    CodeNavigation,
    ImpactAnalysis,
    DocumentFact,
    Conceptual,
    CrossSourceComparison,
    MultiHop,
    /// A question about an API endpoint path (`/api/orders/{id}`) embedded
    /// anywhere in it: exact occurrences first, then the code around them.
    EndpointReference,
    Ambiguous,
}

impl QueryIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactSymbol => "exact_symbol",
            Self::PathLookup => "path_lookup",
            Self::CodeNavigation => "code_navigation",
            Self::ImpactAnalysis => "impact_analysis",
            Self::DocumentFact => "document_fact",
            Self::Conceptual => "conceptual",
            Self::CrossSourceComparison => "cross_source_comparison",
            Self::MultiHop => "multi_hop",
            Self::EndpointReference => "endpoint_reference",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// A retrieval stage a route may enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStrategy {
    /// Exact / qualified / alias symbol and title tiers.
    LexicalExact,
    /// Field-boosted full-text search.
    LexicalFts,
    /// Path search.
    Path,
    /// Symbol lookup plus call/reference graph traversal.
    SymbolGraph,
    /// Vector search (only when a model is available).
    Semantic,
    /// Reciprocal Rank Fusion of lexical and semantic candidates.
    HybridFusion,
    /// Heading and neighbouring chunks around selected document hits.
    ContextExpansion,
    /// Cross-encoder reranking (only when enabled).
    Rerank,
    /// Bounded deterministic decomposition into subqueries.
    Decompose,
    /// Indexed exact-occurrence lookup of typed anchors (URL literals,
    /// composed routes) verified against the original text.
    ExactOccurrence,
}

/// Inputs that change which strategies a route may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteOptions {
    pub semantic_available: bool,
    pub reranker_enabled: bool,
    pub decomposition_enabled: bool,
    /// `search.anchors.enabled = false`: embedded technical anchors do not
    /// change the route (the pre-anchor behaviour, kept for rollback).
    pub anchors_disabled: bool,
}

/// The routing decision, with its explanation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Route {
    pub intent: QueryIntent,
    /// 0..=1: how strongly the winning rule matched.
    pub confidence: f64,
    /// Cues that fired, in evaluation order (for `--explain`).
    pub signals: Vec<String>,
    pub strategies: Vec<RouteStrategy>,
    /// The deterministic fallback was used.
    pub fallback: bool,
    /// Symbol or path the route keyed on, if any.
    pub target: Option<String>,
    /// Typed technical anchors found anywhere in the query.
    #[serde(skip_serializing_if = "QueryAnchors::is_empty")]
    pub anchors: QueryAnchors,
}

/// Hard scope of a search. Every field is an AND-ed restriction; empty
/// means "no restriction". Filters are applied to source selection before
/// any retrieval, and to every candidate of every stage (lexical, graph,
/// semantic, context, decomposition) before fusion and ranking. They are an
/// authorization boundary, not a relevance hint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SearchFilters {
    pub source_ids: Vec<String>,
    /// Source-relative path prefixes (`src/billing/`).
    pub path_prefixes: Vec<String>,
    /// Result kinds: `entity`, `document`, `path`.
    pub kinds: Vec<String>,
    /// Document ids (a chunk of one of them, or the document itself).
    pub document_ids: Vec<String>,
    /// Exclude hits inside email attachments.
    pub exclude_attachments: bool,
}

impl SearchFilters {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether a source may be searched at all.
    pub fn allows_source(&self, source_id: &str) -> bool {
        self.source_ids.is_empty() || self.source_ids.iter().any(|s| s == source_id)
    }

    /// Whether a source-relative path is in scope.
    pub fn allows_path(&self, rel_path: &str) -> bool {
        let p = rel_path.replace('\\', "/");
        self.path_prefixes.is_empty()
            || self
                .path_prefixes
                .iter()
                .any(|pre| p.starts_with(pre.trim_start_matches("./")))
    }

    /// The lexical options these filters push down into every read (the
    /// post-filter still applies [`Self::allows`] to every hit).
    pub fn lexical_options(&self) -> crate::lexical::LexicalOptions {
        crate::lexical::LexicalOptions {
            plan: Default::default(),
            filter: docmonk_storage::read::ReadFilter {
                path_prefixes: self
                    .path_prefixes
                    .iter()
                    .map(|p| p.replace('\\', "/").trim_start_matches("./").to_owned())
                    .collect(),
                document_ids: self.document_ids.clone(),
            },
            kinds: self.kinds.clone(),
        }
    }

    /// The filters a server pushes into its kNN pre-filter.
    pub fn vector_filter(&self) -> docmonk_storage::read::VectorFilter {
        docmonk_storage::read::VectorFilter {
            path_prefixes: self
                .path_prefixes
                .iter()
                .map(|p| p.replace('\\', "/").trim_start_matches("./").to_owned())
                .collect(),
            document_ids: self.document_ids.clone(),
            entities: self.allows_kind("entity"),
            documents: self.allows_kind("document"),
            passages: self.allows_kind("passage"),
            exclude_attachments: self.exclude_attachments,
        }
    }

    pub fn allows_kind(&self, kind: &str) -> bool {
        self.kinds.is_empty() || self.kinds.iter().any(|k| k == kind)
    }

    /// One candidate: its source, relative path, kind, owning document id
    /// (for document hits) and whether it sits in an attachment.
    pub fn allows(
        &self,
        source_id: &str,
        rel_path: &str,
        kind: &str,
        document_id: Option<&str>,
        in_attachment: bool,
    ) -> bool {
        self.allows_source(source_id)
            && self.allows_path(rel_path)
            && self.allows_kind(kind)
            && (self.document_ids.is_empty()
                || document_id.is_some_and(|d| self.document_ids.iter().any(|x| x == d)))
            && !(self.exclude_attachments && in_attachment)
    }
}

struct Rules {
    identifier: Regex,
    path: Regex,
    impact: Regex,
    navigation: Regex,
    comparison: Regex,
    multi_hop: Regex,
    document: Regex,
    question: Regex,
}

fn rules() -> &'static Rules {
    static R: OnceLock<Rules> = OnceLock::new();
    R.get_or_init(|| {
        let ci = |s: &str| Regex::new(&format!("(?i){s}")).expect("valid pattern");
        Rules {
            identifier: Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$")
                .expect("valid"),
            path: Regex::new(r"^[\w.\-]+(/[\w.\-]+)+/?$|^[\w\-]+\.[A-Za-z0-9]{1,6}$").expect("valid"),
            impact: ci(r"what breaks if|what happens if .* changes?|\bimpact of\b|what depends on|\bblast radius\b|επίπτωσ"),
            navigation: ci(r"who calls|callers of|what calls|what does [\w.]+ call|callees of|who (?:uses|references)|where is [\w.]+ (?:defined|used|implemented)|implementation of|definition of"),
            comparison: ci(r"\bcompare\b|\bcomparison\b|\bvs\.?\b|\bversus\b|difference(?:s)? between|differ from|σύγκριση|διαφορ"),
            multi_hop: ci(r"\band then\b|\bwhich (?:then|in turn)\b|\bthrough\b.*\bto\b|\bvia\b|end[- ]to[- ]end|\bflow from\b|\bchain\b"),
            document: ci(r"\bdocuments?\b|\bdocs?\b|\bdocumentation\b|\bpolicy\b|\bguide\b|\bmanual\b|\bhandbook\b|\bemail\b|\bpdf\b|\battachment\b|according to|\bsays?\b|πολιτική|έγγραφ|οδηγ"),
            question: ci(r"^(how|why|what|when|which|who|where|explain|describe|πώς|γιατί|τι|ποι)\b"),
        }
    })
}

/// One API endpoint path found in a query.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct UrlAnchor {
    /// As written (surrounding quotes and punctuation stripped).
    pub raw: String,
    /// The path part: no scheme/host, query or fragment, one leading `/`.
    pub path: String,
    /// The exact-match key ([`url_match_key`]).
    pub key: String,
}

/// Typed technical anchors of a query (P02). Extraction is deterministic
/// and never widens hard filters; anchors only steer which retrieval
/// stages run first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct QueryAnchors {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub url_paths: Vec<UrlAnchor>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub file_paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub http_methods: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quoted: Vec<String>,
}

impl QueryAnchors {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

pub use docmonk_core::urls::url_match_key;
use docmonk_core::urls::{is_url_char, looks_like_endpoint, url_path};

/// Extracts typed anchors from anywhere in `text` (P02): URL paths
/// (`/api/content/groups/image`, `https://host/x/y?q=1`), file paths
/// (`src/a.cs`, `C:\repo\a.cs`), identifiers, HTTP methods and quoted
/// literals. Surrounding quotes and trailing punctuation are stripped;
/// duplicates keep their first position.
pub fn extract_anchors(text: &str) -> QueryAnchors {
    let mut a = QueryAnchors::default();
    static QUOTED: OnceLock<Regex> = OnceLock::new();
    let quoted =
        QUOTED.get_or_init(|| Regex::new(r#""([^"\n]{1,200})"|`([^`\n]{1,200})`"#).expect("valid"));
    for c in quoted.captures_iter(text) {
        let q = c
            .get(1)
            .or_else(|| c.get(2))
            .map_or("", |m| m.as_str())
            .trim();
        if !q.is_empty() && !a.quoted.iter().any(|x| x == q) {
            a.quoted.push(q.to_owned());
        }
    }
    let strip = |t: &str| -> String {
        let t = t.trim_matches(|c: char| "\"'`()[]<>,;!".contains(c));
        // A question mark or period ending a sentence is not part of a path.
        t.trim_end_matches(['?', '.', ':', ',']).to_owned()
    };
    for raw_token in text.split_whitespace() {
        let token = strip(raw_token);
        if token.is_empty() {
            continue;
        }
        let windows_path = token.len() > 2
            && token.as_bytes()[1] == b':'
            && token.as_bytes()[0].is_ascii_alphabetic()
            && token.contains('\\');
        if windows_path || (token.contains('\\') && !token.contains('/')) {
            if !a.file_paths.contains(&token) {
                a.file_paths.push(token);
            }
            continue;
        }
        if !token.contains('/') {
            continue;
        }
        let last = token.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let has_ext = last.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && FILE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        });
        if looks_like_endpoint(&token) && (!has_ext || token.contains("://")) {
            let url_part: String = token
                .chars()
                .take_while(|&c| is_url_char(c) || c == '?' || c == '#')
                .collect();
            let Some(key) = url_match_key(&url_part) else {
                continue;
            };
            let path_only = url_path(&url_part);
            if !a.url_paths.iter().any(|u| u.key == key) {
                a.url_paths.push(UrlAnchor {
                    raw: token.clone(),
                    path: path_only,
                    key,
                });
            }
        } else if has_ext && !a.file_paths.contains(&token) {
            a.file_paths.push(token);
        }
    }
    static METHOD: OnceLock<Regex> = OnceLock::new();
    let method = METHOD.get_or_init(|| {
        Regex::new(r"\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\b").expect("valid")
    });
    for m in method.find_iter(text) {
        if !a.http_methods.iter().any(|x| x == m.as_str()) {
            a.http_methods.push(m.as_str().to_owned());
        }
    }
    // Identifiers outside the URL and file anchors (a path's segments are
    // not symbols).
    let mut rest = text.to_owned();
    for u in &a.url_paths {
        rest = rest.replace(&u.raw, " ");
    }
    for f in &a.file_paths {
        rest = rest.replace(f.as_str(), " ");
    }
    // Dotted names first (`OrderService.Cancel`, not `OrderService`); a
    // dotted run needs a capital or `_` so prose (`e.g`) is not a symbol.
    static SYMBOL: OnceLock<Regex> = OnceLock::new();
    let symbol = SYMBOL.get_or_init(|| {
        Regex::new(r"\b([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+|[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]*)+|[a-z]+_[a-z0-9_]+)\b")
            .expect("valid")
    });
    for m in symbol.find_iter(&rest) {
        let id = m.as_str();
        let dotted = id.contains('.');
        if dotted && !id.chars().any(|c| c.is_ascii_uppercase() || c == '_') {
            continue;
        }
        if !a.symbols.iter().any(|s| s == id) {
            a.symbols.push(id.to_owned());
        }
    }
    a
}

fn identifiers(text: &str) -> Vec<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| {
        Regex::new(r"\b([A-Z][a-z0-9]+(?:[A-Z][a-z0-9]*)+|[a-z]+_[a-z0-9_]+|[A-Za-z_]\w*\.[A-Za-z_][\w.]*)\b")
            .expect("valid")
    });
    let mut v: Vec<String> = r.captures_iter(text).map(|c| c[1].to_owned()).collect();
    v.dedup();
    v
}

/// The symbol a navigation / impact phrase names (`who calls X`, `what
/// breaks if X changes`, ...).
fn phrase_target(text: &str) -> Option<String> {
    static R: OnceLock<Vec<Regex>> = OnceLock::new();
    let rs = R.get_or_init(|| {
        [
            r"(?i)who calls ([\w.]+)",
            r"(?i)callers of ([\w.]+)",
            r"(?i)what calls ([\w.]+)",
            r"(?i)who (?:uses|references) ([\w.]+)",
            r"(?i)what does ([\w.]+) call",
            r"(?i)callees of ([\w.]+)",
            r"(?i)where is ([\w.]+) (?:defined|used|implemented)",
            r"(?i)(?:implementation|definition) of ([\w.]+)",
            r"(?i)what breaks if ([\w.]+)",
            r"(?i)what happens if ([\w.]+) changes",
            r"(?i)impact of ([\w.]+)",
            r"(?i)what depends on ([\w.]+)",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("valid"))
        .collect()
    });
    rs.iter()
        .find_map(|r| r.captures(text).map(|c| c[1].to_owned()))
}

use docmonk_core::urls::FILE_EXTENSIONS;

/// Routes `query` (see the module docs). Deterministic: the same query and
/// options always produce the same route.
pub fn route(query: &str, opts: RouteOptions) -> Route {
    use RouteStrategy::*;
    let text = query.trim();
    let r = rules();
    let mut signals = Vec::new();
    let semantic = |mut v: Vec<RouteStrategy>| {
        if opts.semantic_available {
            v.push(Semantic);
            v.push(HybridFusion);
        }
        if opts.reranker_enabled && v.contains(&HybridFusion) {
            v.push(Rerank);
        }
        v
    };
    let anchors = if opts.anchors_disabled {
        QueryAnchors::default()
    } else {
        extract_anchors(text)
    };
    let mk = |intent, confidence: f64, signals: Vec<String>, strategies, target, fallback| Route {
        intent,
        confidence: (confidence * 100.0).round() / 100.0,
        signals,
        strategies,
        fallback,
        target,
        anchors: anchors.clone(),
    };
    if text.is_empty() {
        return mk(
            QueryIntent::Ambiguous,
            0.0,
            vec!["empty".into()],
            vec![LexicalFts],
            None,
            true,
        );
    }
    // 0. An API endpoint path anywhere in the query is the primary target,
    // whatever else the question mentions ("documentation", "trace", ...):
    // exact occurrences first, then the code relationships around them.
    if let Some(url) = anchors.url_paths.first() {
        signals.push("url_anchor".into());
        if r.document.is_match(text) {
            signals.push("document_phrase".into());
        }
        return mk(
            QueryIntent::EndpointReference,
            0.95,
            signals,
            vec![ExactOccurrence, LexicalExact, SymbolGraph, LexicalFts],
            Some(url.path.clone()),
            false,
        );
    }
    // 1. Structural cues: a path (slashes, or a known file extension) or a
    // bare identifier.
    let file_ext = text
        .rsplit_once('.')
        .is_some_and(|(_, ext)| FILE_EXTENSIONS.contains(&ext));
    if r.path.is_match(text) && (text.contains('/') || file_ext || !r.identifier.is_match(text)) {
        signals.push("path_shape".into());
        return mk(
            QueryIntent::PathLookup,
            0.95,
            signals,
            vec![Path, LexicalExact, LexicalFts],
            Some(text.to_owned()),
            false,
        );
    }
    if r.identifier.is_match(text)
        && (text.contains('.') || text.contains('_') || text != text.to_lowercase())
    {
        signals.push("identifier_shape".into());
        return mk(
            QueryIntent::ExactSymbol,
            0.95,
            signals,
            vec![LexicalExact, SymbolGraph, LexicalFts],
            Some(text.to_owned()),
            false,
        );
    }
    // 2. Scored phrase rules; the highest score wins, ties by rule order.
    let ids = identifiers(text);
    let mut scored: Vec<(QueryIntent, f64)> = Vec::new();
    if r.impact.is_match(text) {
        signals.push("impact_phrase".into());
        scored.push((QueryIntent::ImpactAnalysis, 0.9));
    }
    if r.navigation.is_match(text) {
        signals.push("navigation_phrase".into());
        scored.push((QueryIntent::CodeNavigation, 0.85));
    }
    if r.comparison.is_match(text) {
        signals.push("comparison_phrase".into());
        scored.push((
            QueryIntent::CrossSourceComparison,
            if ids.len() >= 2 { 0.9 } else { 0.75 },
        ));
    }
    // Several questions in one query: answer each part.
    if text.matches('?').count() >= 2 || text.contains(';') {
        signals.push("multi_part".into());
        scored.push((QueryIntent::MultiHop, 0.85));
    }
    if r.multi_hop.is_match(text) {
        signals.push("multi_hop_phrase".into());
        scored.push((
            QueryIntent::MultiHop,
            if ids.len() >= 2 { 0.8 } else { 0.6 },
        ));
    }
    if r.document.is_match(text) {
        signals.push("document_phrase".into());
        scored.push((QueryIntent::DocumentFact, 0.7));
    }
    if !ids.is_empty() {
        signals.push(format!("identifiers:{}", ids.len()));
    }
    let words = text.split_whitespace().count();
    if r.question.is_match(text) && words >= 4 {
        signals.push("question_form".into());
        scored.push((QueryIntent::Conceptual, 0.6));
    }
    let best = scored
        .iter()
        .copied()
        .fold(None::<(QueryIntent, f64)>, |acc, x| match acc {
            Some(a) if a.1 >= x.1 => Some(a),
            _ => Some(x),
        });
    let target = match best {
        Some((QueryIntent::ImpactAnalysis | QueryIntent::CodeNavigation, _)) => {
            phrase_target(text).or_else(|| ids.first().cloned())
        }
        _ => ids.first().cloned(),
    };
    match best {
        Some((QueryIntent::ImpactAnalysis, c)) => mk(
            QueryIntent::ImpactAnalysis,
            c,
            signals,
            vec![LexicalExact, SymbolGraph, ContextExpansion],
            target,
            false,
        ),
        Some((QueryIntent::CodeNavigation, c)) => mk(
            QueryIntent::CodeNavigation,
            c,
            signals,
            vec![LexicalExact, SymbolGraph, LexicalFts],
            target,
            false,
        ),
        Some((QueryIntent::CrossSourceComparison, c)) => {
            let mut st = vec![LexicalExact, LexicalFts, ContextExpansion];
            if opts.decomposition_enabled {
                st.insert(0, Decompose);
            }
            mk(
                QueryIntent::CrossSourceComparison,
                c,
                signals,
                semantic(st),
                target,
                false,
            )
        }
        Some((QueryIntent::MultiHop, c)) => {
            let mut st = vec![LexicalExact, SymbolGraph, LexicalFts, ContextExpansion];
            if opts.decomposition_enabled {
                st.insert(0, Decompose);
            }
            mk(
                QueryIntent::MultiHop,
                c,
                signals,
                semantic(st),
                target,
                false,
            )
        }
        Some((QueryIntent::DocumentFact, c)) => mk(
            QueryIntent::DocumentFact,
            c,
            signals,
            semantic(vec![LexicalFts, LexicalExact, ContextExpansion]),
            target,
            false,
        ),
        Some((QueryIntent::Conceptual, c)) => mk(
            QueryIntent::Conceptual,
            c,
            signals,
            semantic(vec![LexicalFts, LexicalExact, ContextExpansion]),
            target,
            false,
        ),
        // 3. Fallback: short keyword queries and anything unscored.
        _ => {
            signals.push("fallback".into());
            mk(
                QueryIntent::Ambiguous,
                0.3,
                signals,
                semantic(vec![LexicalExact, LexicalFts, Path]),
                target,
                true,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(q: &str) -> QueryIntent {
        route(q, RouteOptions::default()).intent
    }

    /// Classification table (golden): one row per intent plus edge cases.
    #[test]
    fn classification_table() {
        for (q, want) in [
            ("SettlementService", QueryIntent::ExactSymbol),
            (
                "services.settlement_service.SettlementService.process",
                QueryIntent::ExactSymbol,
            ),
            ("retry_settlement", QueryIntent::ExactSymbol),
            ("src/billing/ledger.cs", QueryIntent::PathLookup),
            ("handbook.md", QueryIntent::PathLookup),
            (
                "who calls SettlementService.process",
                QueryIntent::CodeNavigation,
            ),
            ("where is LedgerWriter defined", QueryIntent::CodeNavigation),
            (
                "what breaks if RetryPolicy changes",
                QueryIntent::ImpactAnalysis,
            ),
            ("impact of PaymentGateway", QueryIntent::ImpactAnalysis),
            (
                "compare BillingService and LedgerService retry behaviour",
                QueryIntent::CrossSourceComparison,
            ),
            (
                "billing vs ledger retries",
                QueryIntent::CrossSourceComparison,
            ),
            (
                "how does an order flow from OrderService through PaymentGateway to the ledger",
                QueryIntent::MultiHop,
            ),
            (
                "what does the expense policy say about equipment",
                QueryIntent::DocumentFact,
            ),
            (
                "according to the handbook how many vacation days",
                QueryIntent::DocumentFact,
            ),
            (
                "τι λέει η πολιτική για τις αποζημιώσεις",
                QueryIntent::DocumentFact,
            ),
            (
                "how are failed payments retried overnight",
                QueryIntent::Conceptual,
            ),
            (
                "how are refunds approved? who signs invoices?",
                QueryIntent::MultiHop,
            ),
            ("settlement", QueryIntent::Ambiguous),
            ("ledger retries", QueryIntent::Ambiguous),
            ("", QueryIntent::Ambiguous),
        ] {
            assert_eq!(intent(q), want, "{q:?}");
        }
    }

    #[test]
    fn plans_are_distinguishable_and_explained() {
        let o = RouteOptions::default();
        let sym = route("SettlementService", o);
        let doc = route("what does the expense policy say about equipment", o);
        let imp = route("what breaks if RetryPolicy changes", o);
        assert!(sym.strategies.contains(&RouteStrategy::SymbolGraph));
        assert!(!doc.strategies.contains(&RouteStrategy::SymbolGraph));
        assert!(imp.strategies.contains(&RouteStrategy::SymbolGraph));
        assert_ne!(sym.strategies, doc.strategies);
        assert_ne!(doc.strategies, imp.strategies);
        assert_eq!(imp.target.as_deref(), Some("RetryPolicy"));
        assert!(!doc.signals.is_empty());
        assert!(!sym.fallback && route("settlement", o).fallback);
    }

    #[test]
    fn optional_stages_only_when_available_and_enabled() {
        let q = "compare BillingService and LedgerService retry behaviour";
        let off = route(q, RouteOptions::default());
        assert!(!off.strategies.contains(&RouteStrategy::Semantic));
        assert!(!off.strategies.contains(&RouteStrategy::Decompose));
        let on = route(
            q,
            RouteOptions {
                semantic_available: true,
                reranker_enabled: true,
                decomposition_enabled: true,
                anchors_disabled: false,
            },
        );
        for s in [
            RouteStrategy::Semantic,
            RouteStrategy::HybridFusion,
            RouteStrategy::Rerank,
            RouteStrategy::Decompose,
        ] {
            assert!(on.strategies.contains(&s), "{s:?}");
        }
        // An exact symbol never gets semantic or reranking.
        let sym = route(
            "SettlementService",
            RouteOptions {
                semantic_available: true,
                reranker_enabled: true,
                decomposition_enabled: true,
                anchors_disabled: false,
            },
        );
        assert!(!sym.strategies.contains(&RouteStrategy::Semantic));
    }

    #[test]
    fn routing_is_deterministic() {
        let o = RouteOptions {
            semantic_available: true,
            ..Default::default()
        };
        for q in [
            "how does billing work",
            "BillingService",
            "docs about refunds",
        ] {
            assert_eq!(route(q, o), route(q, o));
        }
    }

    // clean-slate-audit: allow-start (API URL paths with a version segment are user data)
    const Q01: &str =
        "Is the URL path /sb/v1/content/groups/image used, referenced, or related to any \
                       code or documentation in the indexed DocMonk project? Trace matching \
                       route/client/API references and relevant call paths.";

    #[test]
    fn embedded_url_is_the_primary_anchor() {
        let r = route(Q01, RouteOptions::default());
        assert_eq!(r.intent, QueryIntent::EndpointReference);
        assert_eq!(r.target.as_deref(), Some("/sb/v1/content/groups/image"));
        assert_eq!(r.anchors.url_paths.len(), 1, "{:?}", r.anchors);
        assert_eq!(r.anchors.url_paths[0].key, "sb/v1/content/groups/image");
        assert!(r.strategies.contains(&RouteStrategy::ExactOccurrence));
        assert!(!r.strategies.contains(&RouteStrategy::Semantic));
        // "route/client/API" is plain English, not a path anchor.
        assert!(r.anchors.file_paths.is_empty());
        // The bare endpoint has the same primary target.
        let bare = route("/sb/v1/content/groups/image", RouteOptions::default());
        assert_eq!(bare.intent, QueryIntent::EndpointReference);
        assert_eq!(bare.target, r.target);
        assert_eq!(bare.anchors.url_paths[0].key, r.anchors.url_paths[0].key);
        // Documentation keywords never override the exact anchor.
        let mixed = route(
            "what does the documentation say about /sb/v1/content/groups/image?",
            RouteOptions {
                semantic_available: true,
                ..Default::default()
            },
        );
        assert_eq!(mixed.intent, QueryIntent::EndpointReference);
        assert!(!mixed.strategies.contains(&RouteStrategy::Semantic));
        // Rollback flag: the pre-anchor route.
        let off = route(
            Q01,
            RouteOptions {
                anchors_disabled: true,
                ..Default::default()
            },
        );
        assert_ne!(off.intent, QueryIntent::EndpointReference);
        assert!(off.anchors.is_empty());
    }

    #[test]
    fn url_keys_normalize_without_over_normalizing() {
        let k = |s: &str| url_match_key(s);
        assert_eq!(
            k("/sb/v1/content/groups/image").as_deref(),
            Some("sb/v1/content/groups/image")
        );
        assert_eq!(
            k("https://api.example.com//SB/v1/content/groups/image/?x=1#f").as_deref(),
            Some("sb/v1/content/groups/image")
        );
        assert_eq!(
            k("~/sb/v1/content/groups/image").as_deref(),
            Some("sb/v1/content/groups/image")
        );
        assert_eq!(k("api/orders/{id:int}").as_deref(), Some("api/orders/{}"));
        assert_eq!(
            k("api/orders/{orderId}/lines").as_deref(),
            Some("api/orders/{}/lines")
        );
        assert_eq!(k("/a%2Fb/C").as_deref(), Some("a%2fb/c"));
        assert_ne!(
            k("/sb/v1/content/groups/image2"),
            k("/sb/v1/content/groups/image")
        );
        assert_ne!(
            k("/sb/v1/groups/content/image"),
            k("/sb/v1/content/groups/image")
        );
        assert_eq!(k("/"), None);
        assert_eq!(k("https://host"), None);
    }

    #[test]
    fn anchors_are_typed() {
        let a = extract_anchors(
            "Does `OrderService.Cancel` call GET \"/api/v2/orders/{id}\" or src/billing/ledger.cs \
             or C:\\repo\\Billing\\Ledger.cs, and/or https://h.example.com/v1/items?page=2.",
        );
        let urls: Vec<&str> = a.url_paths.iter().map(|u| u.path.as_str()).collect();
        assert_eq!(urls, ["/api/v2/orders/{id}", "/v1/items"]);
        assert_eq!(
            a.file_paths,
            ["src/billing/ledger.cs", "C:\\repo\\Billing\\Ledger.cs"]
        );
        assert_eq!(a.http_methods, ["GET"]);
        assert!(
            a.symbols.contains(&"OrderService.Cancel".to_owned()),
            "{:?}",
            a.symbols
        );
        assert!(a.quoted.contains(&"OrderService.Cancel".to_owned()));
        // Non-ASCII words and plain slashes are not anchors.
        let g = extract_anchors("τι λέει η πολιτική για input/output και ναι/όχι");
        assert!(g.url_paths.is_empty() && g.file_paths.is_empty(), "{g:?}");
        // A file path stays a file path.
        assert_eq!(
            route("src/billing/ledger.cs", RouteOptions::default()).intent,
            QueryIntent::PathLookup
        );
    }
    // clean-slate-audit: allow-end

    #[test]
    fn filters_are_conjunctive() {
        let f = SearchFilters {
            source_ids: vec!["a".into()],
            path_prefixes: vec!["src/billing/".into()],
            kinds: vec!["document".into()],
            document_ids: vec!["d1".into()],
            exclude_attachments: true,
        };
        assert!(f.allows("a", "src/billing/x.md", "document", Some("d1"), false));
        assert!(!f.allows("b", "src/billing/x.md", "document", Some("d1"), false));
        assert!(!f.allows("a", "src/ledger/x.md", "document", Some("d1"), false));
        assert!(!f.allows("a", "src/billing/x.md", "entity", Some("d1"), false));
        assert!(!f.allows("a", "src/billing/x.md", "document", Some("d2"), false));
        assert!(!f.allows("a", "src/billing/x.md", "document", None, false));
        assert!(!f.allows("a", "src/billing/x.md", "document", Some("d1"), true));
        assert!(SearchFilters::default().allows("z", "any", "path", None, true));
        // Windows separators are normalized; "./" prefixes are tolerated.
        let p = SearchFilters {
            path_prefixes: vec!["./src/".into()],
            ..Default::default()
        };
        assert!(p.allows_path("src\\a.cs"));
    }
}
