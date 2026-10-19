//! Exact occurrences of typed URL anchors (P03).
//!
//! An endpoint path in a question (`/api/content/groups/image`) is
//! looked up by its match key ([`docmonk_core::urls::url_match_key`]), never
//! through full-text tokens (a tokenizer splits a path into words, which
//! proves nothing about the path):
//!
//! * **Code**: the graph derivation `graph-3` records every URL string
//!   literal as a `url_literal:<key>` reference and every statically
//!   composed framework route as `route:<key>` (see `docmonk-code`). Both
//!   are read with one batched, indexed `target_symbol` equality lookup
//!   per source ([`KnowledgeRead::unresolved_edges_many`]) on every backend,
//!   scoped to the request's published build and graph generation.
//! * **Documents**: full-text search narrows candidate chunks (phrase of
//!   the path's words), then each candidate's stored text is verified to
//!   contain a token with the same match key. Unverified candidates are
//!   dropped, so `/x/image2` never counts as `/x/image`.
//!
//! A source whose graph predates `graph-3` (or whose graph is not visible)
//! is reported in [`ExactOutcome::capabilities`]; its empty result never
//! means "absent".

use std::collections::{BTreeMap, HashMap, HashSet};

use docmonk_core::urls::{self, ROUTE_PREFIX, URL_LITERAL_PREFIX};
use docmonk_storage::knowledge::EntityRow;
use docmonk_storage::read::ReadFilter;
use docmonk_storage::search::word_tokens;
use serde::Serialize;

use crate::route::UrlAnchor;
use crate::{Corpus, SearchError};

/// Document chunks verified per anchor and source (each costs one chunk
/// read), so a generic anchor (`/health`) stays cheap over many sources.
pub const MAX_DOCUMENT_CANDIDATES: usize = 20;

/// The first graph derivation carrying the exact-occurrence projection.
pub const PROJECTION_GRAPH_VERSION: u32 = 3;

/// How directly a piece of evidence supports an exact anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceClass {
    /// The anchor's path written verbatim (string literal, config value).
    DirectExactLiteral,
    /// A framework route composed from static attributes (not runtime
    /// proof: conventional routes and middleware are not seen).
    StaticallyComposedRoute,
    /// A resolved graph relationship of a matched entity (caller, ...).
    ResolvedCodeRelationship,
    /// A document chunk whose text contains the path.
    DocumentReference,
    /// A template or interpolated literal that may produce the path.
    HeuristicMatch,
    /// A source that could not be checked (projection missing, graph not
    /// visible).
    UnresolvedOrNotIndexed,
}

impl EvidenceClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectExactLiteral => "direct_exact_literal",
            Self::StaticallyComposedRoute => "statically_composed_route",
            Self::ResolvedCodeRelationship => "resolved_code_relationship",
            Self::DocumentReference => "document_reference",
            Self::HeuristicMatch => "heuristic_match",
            Self::UnresolvedOrNotIndexed => "unresolved_or_not_indexed",
        }
    }
}

/// One verified occurrence of an anchor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Occurrence {
    pub class: EvidenceClass,
    /// The anchor's path (`/api/content/groups/image`).
    pub anchor: String,
    #[serde(skip)]
    pub corpus: usize,
    pub source_id: String,
    /// Source-relative path.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    /// The literal or route as written.
    pub matched: String,
    /// `GET`, `POST`, ... for a composed route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_method: Option<String>,
    /// The entity owning a code occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    #[serde(skip)]
    pub entity_row: Option<EntityRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,
    pub resolver: String,
    pub confidence: String,
}

/// Whether one source could answer the exact lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceCapability {
    pub source_id: String,
    /// `indexed` (graph derivation carries the projection),
    /// `not_indexed` (an older derivation: rebuild the graph to add it),
    /// `unknown` (no visible graph, or the reader cannot tell).
    pub exact_occurrences: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derivation: Option<String>,
}

/// Options of one exact lookup.
#[derive(Debug, Clone, Default)]
pub struct ExactOptions {
    /// Occurrences kept overall (stable order, see [`find_occurrences`]).
    pub max_results: usize,
    /// Verify document chunks too.
    pub documents: bool,
    /// Hard filters pushed into every read (and re-checked per hit).
    pub filter: ReadFilter,
    pub snippet_tokens: i64,
    /// Template keys tried per anchor (one segment as `{}` each).
    pub max_template_variants: usize,
}

/// The result of [`find_occurrences`].
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExactOutcome {
    pub occurrences: Vec<Occurrence>,
    pub capabilities: Vec<SourceCapability>,
    /// Corpora (indexes) with at least one occurrence: the candidate
    /// sources later stages expand from.
    #[serde(skip)]
    pub candidate_corpora: Vec<usize>,
    /// Occurrences dropped by `max_results`.
    pub truncated: bool,
    /// Document candidates full-text search proposed but verification
    /// rejected (a near miss such as `/x/image2`).
    pub rejected_document_candidates: usize,
    /// Backend reads issued.
    pub reads: usize,
}

/// The graph version number in a derivation identity
/// (`rust-code-2+graph-3` -> 3).
pub fn graph_version(derivation: &str) -> Option<u32> {
    derivation
        .split('+')
        .find_map(|p| p.strip_prefix("graph-"))
        .and_then(|n| n.parse().ok())
}

fn capability(source_id: &str, derivation: Option<String>) -> SourceCapability {
    let state = match derivation.as_deref().map(graph_version) {
        Some(Some(v)) if v >= PROJECTION_GRAPH_VERSION => "indexed",
        Some(_) => "not_indexed",
        None => "unknown",
    };
    SourceCapability {
        source_id: source_id.to_owned(),
        exact_occurrences: state,
        derivation,
    }
}

/// `rel_path:line` of a relationship's `source_location`.
fn split_location(loc: &str) -> (String, Option<i64>) {
    match loc.rsplit_once(':') {
        Some((p, l)) if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) => {
            (p.to_owned(), l.parse().ok())
        }
        _ => (loc.to_owned(), None),
    }
}

/// Every token of `text` that could be a URL path, with its match key.
fn url_keys_in(text: &str) -> HashSet<String> {
    text.split(|c: char| c.is_whitespace() || "\"'`()[]<>,;|".contains(c))
        .filter(|t| t.contains('/'))
        .filter_map(|t| {
            let t: String = t
                .chars()
                .take_while(|&c| urls::is_url_char(c) || c == '?' || c == '#')
                .collect();
            urls::url_match_key(t.trim_end_matches(['.', ':', '?']))
        })
        .collect()
}

/// Exact occurrences of `anchors` across `corpora` (see the module docs).
/// Order: class (direct, composed, document, heuristic), then source,
/// path, line, matched text: identical for every backend.
pub fn find_occurrences(
    corpora: &[Corpus<'_>],
    anchors: &[UrlAnchor],
    opts: &ExactOptions,
) -> Result<ExactOutcome, SearchError> {
    let mut out = ExactOutcome::default();
    if anchors.is_empty() {
        return Ok(out);
    }
    // symbol -> (anchor path, exact?)
    let mut wanted: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for a in anchors {
        for prefix in [URL_LITERAL_PREFIX, ROUTE_PREFIX] {
            wanted
                .entry(format!("{prefix}{}", a.key))
                .or_insert((a.path.clone(), true));
            for v in urls::template_variants(&a.key, opts.max_template_variants) {
                wanted
                    .entry(format!("{prefix}{v}"))
                    .or_insert((a.path.clone(), false));
            }
        }
    }
    let symbols: Vec<String> = wanted.keys().cloned().collect();
    let limit = opts.max_results.max(1);
    let mut seen_hit: HashSet<usize> = HashSet::new();
    for (i, c) in corpora.iter().enumerate() {
        crate::check_deadline()?;
        let source_id = c.store.source_id().to_owned();
        out.capabilities.push(capability(
            &source_id,
            c.store.graph_derivation(c.build_id)?,
        ));
        out.reads += 2;
        let found =
            c.store
                .unresolved_edges_many(c.build_id, &symbols, &["references"], limit as i64)?;
        let rows: Vec<_> = found.into_values().flatten().collect();
        let owners: Vec<String> = rows.iter().map(|r| r.source_entity_id.clone()).collect();
        let entities = if owners.is_empty() {
            HashMap::new()
        } else {
            out.reads += 1;
            c.store.entities_by_ids(c.build_id, &owners)?
        };
        for r in rows {
            let Some(symbol) = r.target_symbol.as_deref() else {
                continue;
            };
            let Some((anchor, exact)) = wanted.get(symbol) else {
                continue;
            };
            let (path, line) = split_location(r.source_location.as_deref().unwrap_or(""));
            if !opts.filter.allows_path(&path) || !opts.filter.document_ids.is_empty() {
                continue;
            }
            let route = symbol.starts_with(ROUTE_PREFIX);
            let class = match (exact, route, r.resolver.as_str()) {
                (true, false, "literal:url") => EvidenceClass::DirectExactLiteral,
                (true, true, _) => EvidenceClass::StaticallyComposedRoute,
                _ => EvidenceClass::HeuristicMatch,
            };
            let evidence = r.evidence.clone().unwrap_or_default();
            let (http_method, matched) = if route {
                match evidence.split_once(' ') {
                    Some((m, p)) => (Some(m.to_owned()), p.to_owned()),
                    None => (None, evidence),
                }
            } else {
                (None, evidence)
            };
            let entity_row = entities.get(&r.source_entity_id).cloned();
            seen_hit.insert(i);
            out.occurrences.push(Occurrence {
                class,
                anchor: anchor.clone(),
                corpus: i,
                source_id: source_id.clone(),
                path,
                line,
                matched,
                http_method,
                entity: entity_row.as_ref().map(|e| e.qualified_name.clone()),
                entity_row,
                document_id: None,
                chunk_id: None,
                section: None,
                page: None,
                resolver: r.resolver,
                confidence: r.confidence,
            });
        }
        if opts.documents {
            for a in anchors {
                let words = word_tokens(&a.path);
                if words.is_empty() {
                    continue;
                }
                let expression = format!("\"{}\"", words.join(" "));
                out.reads += 1;
                let rows = c.store.search_chunks_fts_in(
                    c.build_id,
                    &expression,
                    limit.min(MAX_DOCUMENT_CANDIDATES) as i64,
                    opts.snippet_tokens,
                    &opts.filter,
                )?;
                for row in rows {
                    if !opts.filter.allows_path(&row.rel_path)
                        || !opts.filter.allows_document(&row.document_id)
                    {
                        continue;
                    }
                    out.reads += 1;
                    let Some(chunk) = c.store.chunk(c.build_id, &row.id)? else {
                        continue;
                    };
                    let text = crate::context::text_of(&chunk);
                    if !url_keys_in(&text).contains(&a.key) {
                        out.rejected_document_candidates += 1;
                        continue;
                    }
                    seen_hit.insert(i);
                    out.occurrences.push(Occurrence {
                        class: EvidenceClass::DocumentReference,
                        anchor: a.path.clone(),
                        corpus: i,
                        source_id: source_id.clone(),
                        path: row.rel_path,
                        line: None,
                        matched: a.path.clone(),
                        http_method: None,
                        entity: None,
                        entity_row: None,
                        document_id: Some(row.document_id),
                        chunk_id: Some(row.id),
                        section: (!chunk.heading_path.is_empty())
                            .then(|| chunk.heading_path.join(" > ")),
                        page: chunk.page_start,
                        resolver: "verified_text".into(),
                        confidence: "exact".into(),
                    });
                }
            }
        }
    }
    let class_rank = |c: EvidenceClass| match c {
        EvidenceClass::DirectExactLiteral => 0,
        EvidenceClass::StaticallyComposedRoute => 1,
        EvidenceClass::DocumentReference => 2,
        _ => 3,
    };
    out.occurrences.sort_by(|a, b| {
        (
            class_rank(a.class),
            &a.source_id,
            &a.path,
            a.line,
            &a.matched,
            &a.chunk_id,
        )
            .cmp(&(
                class_rank(b.class),
                &b.source_id,
                &b.path,
                b.line,
                &b.matched,
                &b.chunk_id,
            ))
    });
    out.occurrences.dedup_by(|a, b| {
        a.source_id == b.source_id
            && a.path == b.path
            && a.line == b.line
            && a.class == b.class
            && a.matched == b.matched
            && a.chunk_id == b.chunk_id
    });
    if out.occurrences.len() > limit {
        out.occurrences.truncate(limit);
        out.truncated = true;
    }
    out.candidate_corpora = seen_hit.into_iter().collect();
    out.candidate_corpora.sort_unstable();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_versions_and_capabilities() {
        assert_eq!(graph_version("rust-code-2+graph-3"), Some(3));
        assert_eq!(graph_version("rust-code-2+graph-2"), Some(2));
        assert_eq!(graph_version("other"), None);
        assert_eq!(
            capability("s", Some("rust-code-2+graph-3".into())).exact_occurrences,
            "indexed"
        );
        assert_eq!(
            capability("s", Some("rust-code-2+graph-2".into())).exact_occurrences,
            "not_indexed"
        );
        assert_eq!(capability("s", None).exact_occurrences, "unknown");
    }

    // clean-slate-audit: allow-start (API URL paths with a version segment are user data)
    #[test]
    fn document_text_keys_respect_segments() {
        let keys = url_keys_in(
            "Call `GET /sb/v1/content/groups/image?size=2` (see https://h.example/sb/v1/x). \
             Not /sb/v1/content/groups/image2.",
        );
        assert!(keys.contains("sb/v1/content/groups/image"));
        assert!(keys.contains("sb/v1/x"));
        assert!(keys.contains("sb/v1/content/groups/image2"));
        assert!(!url_keys_in("only /sb/v1/content/groups/image2 here")
            .contains("sb/v1/content/groups/image"));
        assert_eq!(split_location("a/b.cs:12"), ("a/b.cs".into(), Some(12)));
        assert_eq!(split_location("a/b.cs"), ("a/b.cs".into(), None));
    }
    // clean-slate-audit: allow-end
}
