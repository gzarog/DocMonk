//! Symbol lookup and code-graph queries.
//!
//! * [`find_symbol_matches`]: every entity named `name` (bare or qualified)
//!   across corpora, ordered by qualified name, source, path, line.
//! * [`traverse`]: a depth- and result-capped BFS over one corpus's
//!   relationships. Each frontier entity's edges are sorted by
//!   `(type, target, id)`, and visited entities are never expanded twice.
//! * [`traverse_symbol`]: `callers` / `callees`. Incoming walks also merge
//!   the unresolved (name-only) edges recorded under the query and under
//!   every match's bare name. With no match at all, incoming falls back to
//!   unresolved edges in every corpus.
//! * [`references`]: CALLS / IMPORTS / REFERENCES in both directions.
//! * [`resolved_incoming`] / [`resolved_outgoing`]: edges with their other
//!   end resolved to an entity and file.
//! * [`find_tests_referencing`]: callers in files named like tests.

use std::collections::HashSet;

use docmonk_storage::knowledge::{EntityRow, RelationshipRow};
use serde::Serialize;

use crate::{Corpus, SearchError};

/// Default traversal depth.
pub const DEFAULT_MAX_DEPTH: usize = 1;
/// Default number of results.
pub const DEFAULT_LIMIT: usize = 100;
/// Relationship types `references` and the tests signal consider.
pub const REFERENCE_TYPES: [&str; 3] = ["calls", "imports", "references"];
/// Relationship types `callers` / `callees` consider.
pub const CALL_TYPES: [&str; 1] = ["calls"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

/// One entity matching a symbol query.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceMatch {
    /// Index into the corpora the query ran over.
    #[serde(skip)]
    pub corpus: usize,
    pub source_id: String,
    pub entity: EntityRow,
    pub rel_path: String,
}

/// One traversed edge and its hop distance from the start entity.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraversalEdge {
    #[serde(skip)]
    pub corpus: usize,
    pub depth: usize,
    pub relationship: RelationshipRow,
}

/// The edge sort key: type, then target (entity id, else
/// symbol), then id.
fn edge_key(r: &RelationshipRow) -> (String, String, String) {
    (
        r.relationship_type.clone(),
        r.target_entity_id
            .clone()
            .or_else(|| r.target_symbol.clone())
            .unwrap_or_default(),
        r.id.clone(),
    )
}

/// Every entity named `name` across `corpora`.
pub fn find_symbol_matches(
    corpora: &[Corpus<'_>],
    name: &str,
) -> Result<Vec<SourceMatch>, SearchError> {
    let mut out = Vec::new();
    for (i, c) in corpora.iter().enumerate() {
        for entity in c.store.symbol_entities(c.build_id, name)? {
            let rel_path = c
                .store
                .file_rel_path(c.build_id, &entity.file_id)?
                .unwrap_or_default();
            out.push(SourceMatch {
                corpus: i,
                source_id: c.store.source_id().to_owned(),
                entity,
                rel_path,
            });
        }
    }
    // Ordered by qualified name and source. Ids depend on the source
    // path, so same-named entities
    // are ordered by path and line before the id.
    out.sort_by(|a, b| {
        (
            &a.entity.qualified_name,
            &a.source_id,
            &a.rel_path,
            a.entity.start_line,
            &a.entity.id,
        )
            .cmp(&(
                &b.entity.qualified_name,
                &b.source_id,
                &b.rel_path,
                b.entity.start_line,
                &b.entity.id,
            ))
    });
    Ok(out)
}

/// BFS from `start` over `types` (all types when empty), at most
/// `max_depth` hops and `limit` edges.
pub fn traverse(
    corpora: &[Corpus<'_>],
    corpus: usize,
    start: &str,
    direction: Direction,
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<Vec<TraversalEdge>, SearchError> {
    let c = corpora[corpus];
    let outgoing = direction == Direction::Outgoing;
    let mut visited: HashSet<String> = HashSet::from([start.to_owned()]);
    let mut frontier = vec![start.to_owned()];
    let mut out = Vec::new();
    let mut depth = 1;
    while !frontier.is_empty() && depth <= max_depth && out.len() < limit {
        let mut next = Vec::new();
        'frontier: for id in &frontier {
            let mut edges = Vec::new();
            if types.is_empty() {
                edges = c
                    .store
                    .entity_edges(c.build_id, id, None, outgoing, limit as i64)?;
            } else {
                for t in types {
                    edges.extend(c.store.entity_edges(
                        c.build_id,
                        id,
                        Some(t),
                        outgoing,
                        limit as i64,
                    )?);
                }
            }
            edges.sort_by_key(edge_key);
            for e in edges {
                let neighbor = if outgoing {
                    e.target_entity_id.clone()
                } else {
                    Some(e.source_entity_id.clone())
                };
                out.push(TraversalEdge {
                    corpus,
                    depth,
                    relationship: e,
                });
                if out.len() >= limit {
                    break 'frontier;
                }
                if let Some(n) = neighbor {
                    if visited.insert(n.clone()) {
                        next.push(n);
                    }
                }
            }
        }
        frontier = next;
        depth += 1;
    }
    out.truncate(limit);
    Ok(out)
}

/// Depth-1 edges recorded only under `symbol` in one corpus.
pub fn unresolved_symbol_edges(
    corpora: &[Corpus<'_>],
    corpus: usize,
    symbol: &str,
    types: &[&str],
    limit: usize,
) -> Result<Vec<TraversalEdge>, SearchError> {
    let c = corpora[corpus];
    let mut edges = Vec::new();
    if types.is_empty() {
        edges = c
            .store
            .unresolved_edges(c.build_id, symbol, None, limit as i64)?;
    } else {
        for t in types {
            edges.extend(
                c.store
                    .unresolved_edges(c.build_id, symbol, Some(t), limit as i64)?,
            );
        }
    }
    edges.sort_by_key(edge_key);
    edges.truncate(limit);
    Ok(edges
        .into_iter()
        .map(|relationship| TraversalEdge {
            corpus,
            depth: 1,
            relationship,
        })
        .collect())
}

/// Unresolved incoming edges under `name` and each match's bare name, each
/// `(corpus, symbol)` pair once.
fn unresolved_for_matches(
    corpora: &[Corpus<'_>],
    matches: &[SourceMatch],
    name: &str,
    types: &[&str],
    limit: usize,
    seen: &mut HashSet<(usize, String)>,
    m: &SourceMatch,
) -> Result<Vec<TraversalEdge>, SearchError> {
    let _ = matches;
    let mut out = Vec::new();
    let mut symbols = vec![name.to_owned(), m.entity.name.clone()];
    symbols.dedup();
    for s in symbols {
        if seen.insert((m.corpus, s.clone())) {
            out.extend(unresolved_symbol_edges(
                corpora, m.corpus, &s, types, limit,
            )?);
        }
    }
    Ok(out)
}

fn sort_by_depth(edges: &mut [TraversalEdge]) {
    edges.sort_by(|a, b| {
        (a.depth, edge_key(&a.relationship)).cmp(&(b.depth, edge_key(&b.relationship)))
    });
}

/// `callers` (incoming) / `callees` (outgoing) of `name`.
pub fn traverse_symbol(
    corpora: &[Corpus<'_>],
    name: &str,
    direction: Direction,
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<(Vec<SourceMatch>, Vec<TraversalEdge>), SearchError> {
    let matches = find_symbol_matches(corpora, name)?;
    let mut edges = Vec::new();
    if !matches.is_empty() {
        let mut seen = HashSet::new();
        for m in &matches {
            edges.extend(traverse(
                corpora,
                m.corpus,
                &m.entity.id,
                direction,
                types,
                max_depth,
                limit,
            )?);
            if direction == Direction::Incoming {
                edges.extend(unresolved_for_matches(
                    corpora, &matches, name, types, limit, &mut seen, m,
                )?);
            }
        }
        sort_by_depth(&mut edges);
        edges.truncate(limit);
        return Ok((matches, edges));
    }
    if direction == Direction::Incoming {
        for i in 0..corpora.len() {
            edges.extend(unresolved_symbol_edges(corpora, i, name, types, limit)?);
        }
        edges.sort_by(|a, b| edge_key(&a.relationship).cmp(&edge_key(&b.relationship)));
        edges.truncate(limit);
    }
    Ok((matches, edges))
}

/// Every CALLS / IMPORTS / REFERENCES edge touching `name`, both directions.
pub fn references(
    corpora: &[Corpus<'_>],
    name: &str,
    max_depth: usize,
    limit: usize,
) -> Result<(Vec<SourceMatch>, Vec<TraversalEdge>), SearchError> {
    let matches = find_symbol_matches(corpora, name)?;
    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    for m in &matches {
        edges.extend(traverse(
            corpora,
            m.corpus,
            &m.entity.id,
            Direction::Incoming,
            &REFERENCE_TYPES,
            max_depth,
            limit,
        )?);
        edges.extend(unresolved_for_matches(
            corpora,
            &matches,
            name,
            &REFERENCE_TYPES,
            limit,
            &mut seen,
            m,
        )?);
        edges.extend(traverse(
            corpora,
            m.corpus,
            &m.entity.id,
            Direction::Outgoing,
            &REFERENCE_TYPES,
            max_depth,
            limit,
        )?);
    }
    sort_by_depth(&mut edges);
    edges.truncate(limit);
    Ok((matches, edges))
}

/// An edge with its other end (relative to the explored symbol) resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedEdge {
    pub edge: TraversalEdge,
    pub source_id: String,
    pub neighbor_entity: Option<EntityRow>,
    /// Source-relative path of the neighbour's file.
    pub neighbor_path: Option<String>,
}

fn resolve(
    corpora: &[Corpus<'_>],
    source_id: &str,
    edge: TraversalEdge,
    direction: Direction,
) -> Result<ResolvedEdge, SearchError> {
    let c = corpora[edge.corpus];
    let r = &edge.relationship;
    let neighbor_id = match direction {
        Direction::Incoming => Some(r.source_entity_id.clone()),
        Direction::Outgoing => r.target_entity_id.clone(),
    };
    let entity = match neighbor_id {
        Some(id) if !id.is_empty() => c.store.entity(c.build_id, &id)?,
        _ => None,
    };
    let neighbor_path = match &entity {
        Some(e) => c.store.file_rel_path(c.build_id, &e.file_id)?,
        None => None,
    };
    Ok(ResolvedEdge {
        edge,
        source_id: source_id.to_owned(),
        neighbor_entity: entity,
        neighbor_path,
    })
}

/// Incoming edges of `types` into every match (plus unresolved edges under
/// `name` and each match's bare name), callers resolved.
pub fn resolved_incoming(
    corpora: &[Corpus<'_>],
    matches: &[SourceMatch],
    name: &str,
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<Vec<ResolvedEdge>, SearchError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for m in matches {
        for e in traverse(
            corpora,
            m.corpus,
            &m.entity.id,
            Direction::Incoming,
            types,
            max_depth,
            limit,
        )? {
            out.push(resolve(corpora, &m.source_id, e, Direction::Incoming)?);
        }
        for e in unresolved_for_matches(corpora, matches, name, types, limit, &mut seen, m)? {
            out.push(resolve(corpora, &m.source_id, e, Direction::Incoming)?);
        }
    }
    Ok(out)
}

/// Outgoing edges of `types` from every match, callees resolved.
pub fn resolved_outgoing(
    corpora: &[Corpus<'_>],
    matches: &[SourceMatch],
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<Vec<ResolvedEdge>, SearchError> {
    let mut out = Vec::new();
    for m in matches {
        for e in traverse(
            corpora,
            m.corpus,
            &m.entity.id,
            Direction::Outgoing,
            types,
            max_depth,
            limit,
        )? {
            out.push(resolve(corpora, &m.source_id, e, Direction::Outgoing)?);
        }
    }
    Ok(out)
}

/// Test-file naming conventions per indexed language (`.py`, Go, C#, Java,
/// JS/TS); `\` is treated as `/`.
pub fn is_test_file(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let file = p.rsplit('/').next().unwrap_or(&p);
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, e),
        _ => return false,
    };
    match ext {
        "py" => {
            (stem.starts_with("test_") && stem.len() > 5)
                || (stem.ends_with("_test") && stem.len() > 5)
        }
        "go" => stem.ends_with("_test") && stem.len() > "_test".len(),
        "cs" | "java" => {
            (stem.ends_with("Test") && stem.len() > 4)
                || (stem.ends_with("Tests") && stem.len() > 5)
        }
        "js" | "jsx" | "ts" | "tsx" => ["test", "spec"].iter().any(|k| {
            stem.strip_suffix(k)
                .and_then(|s| s.strip_suffix('.'))
                .is_some_and(|s| !s.is_empty())
        }),
        _ => false,
    }
}

/// Callers of a match (CALLS / IMPORTS / REFERENCES) that live in
/// test-named files.
pub fn find_tests_referencing(
    corpora: &[Corpus<'_>],
    matches: &[SourceMatch],
    name: &str,
    max_depth: usize,
    limit: usize,
) -> Result<Vec<ResolvedEdge>, SearchError> {
    Ok(
        resolved_incoming(corpora, matches, name, &REFERENCE_TYPES, max_depth, limit)?
            .into_iter()
            .filter(|e| e.neighbor_path.as_deref().is_some_and(is_test_file))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::is_test_file;

    #[test]
    fn test_file_conventions_match_reference_patterns() {
        for yes in [
            "tests/test_billing.py",
            "billing_test.py",
            "pkg/ledger_test.go",
            "src/LedgerTests.cs",
            "LedgerTest.java",
            "web/orders.test.ts",
            "web/orders.spec.jsx",
            "src\\test_win.py",
        ] {
            assert!(is_test_file(yes), "{yes}");
        }
        for no in [
            "test.py",
            "test_.py",
            "_test.py",
            "_test.go",
            "Tests.cs",
            "testing.py",
            "contest_x.rs",
            "orders.ts",
            ".test.ts",
            "test_dir/app.py",
        ] {
            assert!(!is_test_file(no), "{no}");
        }
    }
}
