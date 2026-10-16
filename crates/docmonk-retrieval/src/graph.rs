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
//!
//! Cross-source edges: a consumer's reference bound to exactly one
//! producer entity (its dependency manifest) is followed into that
//! producer, in both directions, when both sources are in the queried
//! corpora (source filters stay hard restrictions) and both are still at
//! the generations the binding was computed against. Ambiguous, unbound
//! and stale bindings are never traversed. Nodes are keyed by
//! `(corpus, entity id)`, so same-named symbols of unrelated sources never
//! merge.

use std::collections::{HashMap, HashSet};

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
    /// The corpus holding `relationship` (the consumer, for an external
    /// edge).
    #[serde(skip)]
    pub corpus: usize,
    pub depth: usize,
    pub relationship: RelationshipRow,
    /// Set when the edge crosses into another source through a verified
    /// dependency binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalHop>,
}

/// The producer end of a cross-source edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalHop {
    #[serde(skip)]
    pub producer_corpus: usize,
    pub producer_source_id: String,
    pub consumer_source_id: String,
    pub target_entity_id: String,
}

/// `(consumer corpus, relationship id, unresolved target symbol)`.
type IncomingRef = (usize, String, String);

/// Verified cross-source bindings among `corpora`, both ways.
#[derive(Debug, Default)]
struct Overlay {
    /// `(consumer corpus, relationship id) -> hop`.
    out: HashMap<(usize, String), ExternalHop>,
    /// `(producer corpus, entity id) -> [(consumer corpus, relationship id,
    /// reference text)]`.
    inc: HashMap<(usize, String), Vec<IncomingRef>>,
}

impl Overlay {
    fn load(corpora: &[Corpus<'_>]) -> Result<Self, SearchError> {
        let mut o = Overlay::default();
        let index: HashMap<&str, usize> = corpora
            .iter()
            .enumerate()
            .map(|(i, c)| (c.store.source_id(), i))
            .collect();
        let mut generations: HashMap<usize, Option<String>> = HashMap::new();
        let mut generation = |i: usize| -> Result<Option<String>, SearchError> {
            if let Some(g) = generations.get(&i) {
                return Ok(g.clone());
            }
            let g = corpora[i]
                .store
                .dependency_generation(corpora[i].build_id)?;
            generations.insert(i, g.clone());
            Ok(g)
        };
        for (ci, c) in corpora.iter().enumerate() {
            let Some(m) = c.store.external_manifest()? else {
                continue;
            };
            if m.state != "current"
                || m.bindings.is_empty()
                || generation(ci)?.as_deref() != Some(m.consumer_base_generation.as_str())
            {
                continue;
            }
            for b in &m.bindings {
                let (Some(p), Some(t), false) = (&b.producer, &b.target_entity_id, b.ambiguous)
                else {
                    continue;
                };
                let (Some(&pi), Some(snap)) = (index.get(p.as_str()), m.producers.get(p)) else {
                    continue;
                };
                if pi == ci || generation(pi)?.as_deref() != Some(snap.base_generation.as_str()) {
                    continue;
                }
                o.out.insert(
                    (ci, b.relationship_id.clone()),
                    ExternalHop {
                        producer_corpus: pi,
                        producer_source_id: p.clone(),
                        consumer_source_id: c.store.source_id().to_owned(),
                        target_entity_id: t.clone(),
                    },
                );
                // Bindings written before the symbol was recorded: the
                // unresolved edge is keyed by the reference's last segment.
                let symbol = b.target_symbol.clone().unwrap_or_else(|| {
                    let r = b.reference_text.as_str();
                    r.rsplit('.').next().unwrap_or(r).to_owned()
                });
                o.inc.entry((pi, t.clone())).or_default().push((
                    ci,
                    b.relationship_id.clone(),
                    symbol,
                ));
            }
        }
        Ok(o)
    }
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
        let entities = c.store.symbol_entities(c.build_id, name)?;
        let file_ids: Vec<String> = entities.iter().map(|e| e.file_id.clone()).collect();
        let paths = c.store.file_rel_paths(c.build_id, &file_ids)?;
        for entity in entities {
            let rel_path = paths.get(&entity.file_id).cloned().unwrap_or_default();
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
    Ok(traverse_many(
        corpora,
        &[(corpus, start.to_owned())],
        direction,
        types,
        max_depth,
        limit,
    )?
    .pop()
    .unwrap_or_default())
}

/// [`traverse`] from many starts at once: each start keeps its own BFS
/// (visited set, depth and `limit`), so the result per start is exactly
/// what [`traverse`] returns for it, but every BFS level of every start in
/// one corpus is read with one batched query.
pub fn traverse_many(
    corpora: &[Corpus<'_>],
    starts: &[(usize, String)],
    direction: Direction,
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<Vec<Vec<TraversalEdge>>, SearchError> {
    type Node = (usize, String);
    struct Walk {
        visited: HashSet<Node>,
        frontier: Vec<Node>,
        out: Vec<TraversalEdge>,
    }
    let outgoing = direction == Direction::Outgoing;
    let overlay = Overlay::load(corpora)?;
    let mut walks: Vec<Walk> = starts
        .iter()
        .map(|start| Walk {
            visited: HashSet::from([start.clone()]),
            frontier: vec![start.clone()],
            out: Vec::new(),
        })
        .collect();
    let mut depth = 1;
    while depth <= max_depth {
        let active = |w: &Walk| !w.frontier.is_empty() && w.out.len() < limit;
        // One batched read per corpus for this level of every walk.
        let mut wanted: HashMap<usize, Vec<String>> = HashMap::new();
        // Incoming external edges: consumer corpus -> (reference texts,
        // relationship ids).
        let mut external: HashMap<usize, (Vec<String>, HashSet<String>)> = HashMap::new();
        for w in walks.iter().filter(|w| active(w)) {
            for (corpus, id) in &w.frontier {
                wanted.entry(*corpus).or_default().push(id.clone());
                if !outgoing {
                    for (cc, rel, text) in overlay
                        .inc
                        .get(&(*corpus, id.clone()))
                        .into_iter()
                        .flatten()
                    {
                        let e = external.entry(*cc).or_default();
                        e.0.push(text.clone());
                        e.1.insert(rel.clone());
                    }
                }
            }
        }
        if wanted.is_empty() {
            break;
        }
        let mut level: HashMap<usize, HashMap<String, Vec<RelationshipRow>>> = HashMap::new();
        for (corpus, mut ids) in wanted {
            ids.sort();
            ids.dedup();
            let c = corpora[corpus];
            level.insert(
                corpus,
                c.store
                    .entity_edges_many(c.build_id, &ids, types, outgoing, limit as i64)?,
            );
        }
        let mut external_rows: HashMap<(usize, String), RelationshipRow> = HashMap::new();
        for (corpus, (mut texts, ids)) in external {
            texts.sort();
            texts.dedup();
            let c = corpora[corpus];
            let found = c
                .store
                .unresolved_edges_many(c.build_id, &texts, types, i64::MAX)?;
            for r in found.into_values().flatten() {
                if ids.contains(&r.id) {
                    external_rows.insert((corpus, r.id.clone()), r);
                }
            }
        }
        for w in walks.iter_mut() {
            if !active(w) {
                continue;
            }
            let mut next = Vec::new();
            'frontier: for (corpus, id) in &w.frontier {
                let mut edges: Vec<(usize, RelationshipRow)> = level[corpus]
                    .get(id)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| (*corpus, e))
                    .collect();
                if !outgoing {
                    for (cc, rel, _) in overlay
                        .inc
                        .get(&(*corpus, id.clone()))
                        .into_iter()
                        .flatten()
                    {
                        if let Some(r) = external_rows.get(&(*cc, rel.clone())) {
                            edges.push((*cc, r.clone()));
                        }
                    }
                }
                edges.sort_by(|a, b| edge_key(&a.1).cmp(&edge_key(&b.1)));
                for (ec, e) in edges {
                    let hop = overlay.out.get(&(ec, e.id.clone())).cloned();
                    let neighbor = if outgoing {
                        match (&e.target_entity_id, &hop) {
                            (Some(t), _) => Some((ec, t.clone())),
                            (None, Some(h)) => {
                                Some((h.producer_corpus, h.target_entity_id.clone()))
                            }
                            (None, None) => None,
                        }
                    } else {
                        Some((ec, e.source_entity_id.clone()))
                    };
                    w.out.push(TraversalEdge {
                        corpus: ec,
                        depth,
                        relationship: e,
                        external: hop,
                    });
                    if w.out.len() >= limit {
                        break 'frontier;
                    }
                    if let Some(n) = neighbor {
                        if w.visited.insert(n.clone()) {
                            next.push(n);
                        }
                    }
                }
            }
            w.frontier = next;
        }
        depth += 1;
    }
    Ok(walks
        .into_iter()
        .map(|mut w| {
            w.out.truncate(limit);
            w.out
        })
        .collect())
}

/// Depth-1 unresolved edges of every match, as [`unresolved_for_matches`]
/// returns them one match at a time (each `(corpus, symbol)` pair goes to
/// the first match that names it), with one batched read per corpus.
fn unresolved_for_all(
    corpora: &[Corpus<'_>],
    matches: &[SourceMatch],
    name: &str,
    types: &[&str],
    limit: usize,
) -> Result<Vec<Vec<TraversalEdge>>, SearchError> {
    let mut seen: HashSet<(usize, String)> = HashSet::new();
    let mut claims: Vec<Vec<(usize, String)>> = Vec::with_capacity(matches.len());
    let mut wanted: HashMap<usize, Vec<String>> = HashMap::new();
    for m in matches {
        let mut symbols = vec![name.to_owned(), m.entity.name.clone()];
        symbols.dedup();
        let mut mine = Vec::new();
        for s in symbols {
            if seen.insert((m.corpus, s.clone())) {
                wanted.entry(m.corpus).or_default().push(s.clone());
                mine.push((m.corpus, s));
            }
        }
        claims.push(mine);
    }
    let mut found: HashMap<(usize, String), Vec<TraversalEdge>> = HashMap::new();
    for (corpus, symbols) in wanted {
        let c = corpora[corpus];
        for (symbol, mut edges) in
            c.store
                .unresolved_edges_many(c.build_id, &symbols, types, limit as i64)?
        {
            edges.sort_by_key(edge_key);
            edges.truncate(limit);
            found.insert(
                (corpus, symbol),
                edges
                    .into_iter()
                    .map(|relationship| TraversalEdge {
                        corpus,
                        depth: 1,
                        relationship,
                        external: None,
                    })
                    .collect(),
            );
        }
    }
    Ok(claims
        .into_iter()
        .map(|mine| {
            mine.into_iter()
                .flat_map(|k| found.remove(&k).unwrap_or_default())
                .collect()
        })
        .collect())
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
            external: None,
        })
        .collect())
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
        let starts: Vec<(usize, String)> = matches
            .iter()
            .map(|m| (m.corpus, m.entity.id.clone()))
            .collect();
        let walks = traverse_many(corpora, &starts, direction, types, max_depth, limit)?;
        let unresolved = if direction == Direction::Incoming {
            unresolved_for_all(corpora, &matches, name, types, limit)?
        } else {
            vec![Vec::new(); matches.len()]
        };
        for (walk, extra) in walks.into_iter().zip(unresolved) {
            edges.extend(walk);
            edges.extend(extra);
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
    let starts: Vec<(usize, String)> = matches
        .iter()
        .map(|m| (m.corpus, m.entity.id.clone()))
        .collect();
    let incoming = traverse_many(
        corpora,
        &starts,
        Direction::Incoming,
        &REFERENCE_TYPES,
        max_depth,
        limit,
    )?;
    let unresolved = unresolved_for_all(corpora, &matches, name, &REFERENCE_TYPES, limit)?;
    let outgoing = traverse_many(
        corpora,
        &starts,
        Direction::Outgoing,
        &REFERENCE_TYPES,
        max_depth,
        limit,
    )?;
    for ((i, u), o) in incoming.into_iter().zip(unresolved).zip(outgoing) {
        edges.extend(i);
        edges.extend(u);
        edges.extend(o);
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

/// Resolves the other end of every edge with one entity read and one path
/// read per corpus (not two reads per edge), keeping the order of `edges`.
fn resolve_all(
    corpora: &[Corpus<'_>],
    edges: Vec<(String, TraversalEdge)>,
    direction: Direction,
) -> Result<Vec<ResolvedEdge>, SearchError> {
    let neighbor = |e: &TraversalEdge| -> Option<(usize, String)> {
        let r = &e.relationship;
        match (direction, &e.external) {
            (Direction::Incoming, _) => Some((e.corpus, r.source_entity_id.clone())),
            (Direction::Outgoing, Some(h)) if r.target_entity_id.is_none() => {
                Some((h.producer_corpus, h.target_entity_id.clone()))
            }
            (Direction::Outgoing, _) => r.target_entity_id.clone().map(|t| (e.corpus, t)),
        }
        .filter(|(_, id)| !id.is_empty())
    };
    let mut by_corpus: HashMap<usize, Vec<String>> = HashMap::new();
    for (_, e) in &edges {
        if let Some((corpus, id)) = neighbor(e) {
            by_corpus.entry(corpus).or_default().push(id);
        }
    }
    let mut entities: HashMap<(usize, String), EntityRow> = HashMap::new();
    let mut paths: HashMap<(usize, String), String> = HashMap::new();
    for (corpus, ids) in by_corpus {
        let c = corpora[corpus];
        let found = c.store.entities_by_ids(c.build_id, &ids)?;
        let files: Vec<String> = found.values().map(|e| e.file_id.clone()).collect();
        for (f, p) in c.store.file_rel_paths(c.build_id, &files)? {
            paths.insert((corpus, f), p);
        }
        for (id, e) in found {
            entities.insert((corpus, id), e);
        }
    }
    Ok(edges
        .into_iter()
        .map(|(source_id, edge)| {
            let at = neighbor(&edge);
            let corpus = at.as_ref().map_or(edge.corpus, |(c, _)| *c);
            let entity = at.and_then(|k| entities.get(&k).cloned());
            let neighbor_path = entity
                .as_ref()
                .and_then(|e| paths.get(&(corpus, e.file_id.clone())).cloned());
            ResolvedEdge {
                edge,
                source_id,
                neighbor_entity: entity,
                neighbor_path,
            }
        })
        .collect())
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
    let starts: Vec<(usize, String)> = matches
        .iter()
        .map(|m| (m.corpus, m.entity.id.clone()))
        .collect();
    let walks = traverse_many(
        corpora,
        &starts,
        Direction::Incoming,
        types,
        max_depth,
        limit,
    )?;
    let unresolved = unresolved_for_all(corpora, matches, name, types, limit)?;
    let mut tagged = Vec::new();
    for ((m, walk), extra) in matches.iter().zip(walks).zip(unresolved) {
        tagged.extend(
            walk.into_iter()
                .chain(extra)
                .map(|e| (m.source_id.clone(), e)),
        );
    }
    resolve_all(corpora, tagged, Direction::Incoming)
}

/// Outgoing edges of `types` from every match, callees resolved.
pub fn resolved_outgoing(
    corpora: &[Corpus<'_>],
    matches: &[SourceMatch],
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<Vec<ResolvedEdge>, SearchError> {
    let starts: Vec<(usize, String)> = matches
        .iter()
        .map(|m| (m.corpus, m.entity.id.clone()))
        .collect();
    let walks = traverse_many(
        corpora,
        &starts,
        Direction::Outgoing,
        types,
        max_depth,
        limit,
    )?;
    let mut tagged = Vec::new();
    for (m, walk) in matches.iter().zip(walks) {
        tagged.extend(walk.into_iter().map(|e| (m.source_id.clone(), e)));
    }
    resolve_all(corpora, tagged, Direction::Outgoing)
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
