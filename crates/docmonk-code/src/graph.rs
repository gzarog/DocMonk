//! Bounded, deterministic graph traversal over one build
//! (`traverse` / `traverse_symbol`): the inputs of
//! `callers`, `callees`, `references` and `impact`.

use std::collections::HashSet;

use docmonk_storage::knowledge::{Endpoint, EntityRow, ProjectStore, RelationshipRow};
use docmonk_storage::StorageError;
use serde::Serialize;

pub const DEFAULT_MAX_DEPTH: usize = 1;
pub const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraversalEdge {
    pub depth: usize,
    pub relationship: RelationshipRow,
}

fn sort_key(r: &RelationshipRow) -> (String, String, String) {
    (
        r.relationship_type.clone(),
        r.target_entity_id
            .clone()
            .or_else(|| r.target_symbol.clone())
            .unwrap_or_default(),
        r.id.clone(),
    )
}

/// Breadth-first walk from `start` up to `max_depth` hops and `limit` edges.
pub fn traverse(
    store: &ProjectStore,
    build_id: &str,
    start: &str,
    direction: Direction,
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<Vec<TraversalEdge>, StorageError> {
    let endpoint = match direction {
        Direction::Incoming => Endpoint::Target,
        Direction::Outgoing => Endpoint::Source,
    };
    let mut visited: HashSet<String> = HashSet::from([start.to_owned()]);
    let mut frontier = vec![start.to_owned()];
    let mut out = Vec::new();
    let mut depth = 1;
    while !frontier.is_empty() && depth <= max_depth && out.len() < limit {
        let mut next = Vec::new();
        'frontier: for id in &frontier {
            let mut edges = if types.is_empty() {
                store.edges(build_id, endpoint, id, &[], limit as i64)?
            } else {
                let mut all = Vec::new();
                for t in types {
                    all.extend(store.edges(build_id, endpoint, id, &[t], limit as i64)?);
                }
                all
            };
            edges.sort_by_key(sort_key);
            for edge in edges {
                let neighbor = match direction {
                    Direction::Outgoing => edge.target_entity_id.clone(),
                    Direction::Incoming => Some(edge.source_entity_id.clone()),
                };
                out.push(TraversalEdge {
                    depth,
                    relationship: edge,
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

/// Resolves `name` (bare or qualified) and walks from every match.
/// Incoming walks also include edges still recorded only under a bare
/// symbol (unresolved references), for `name` and each match's bare name.
pub fn traverse_symbol(
    store: &ProjectStore,
    build_id: &str,
    name: &str,
    direction: Direction,
    types: &[&str],
    max_depth: usize,
    limit: usize,
) -> Result<(Vec<EntityRow>, Vec<TraversalEdge>), StorageError> {
    let matches = store.entities_named(build_id, name)?;
    let mut edges = Vec::new();
    let mut symbols: Vec<String> = Vec::new();
    for m in &matches {
        edges.extend(traverse(
            store, build_id, &m.id, direction, types, max_depth, limit,
        )?);
        symbols.push(m.name.clone());
    }
    if direction == Direction::Incoming {
        symbols.push(name.to_owned());
        symbols.sort();
        symbols.dedup();
        for s in &symbols {
            for r in store.edges_to_symbol(build_id, s, types, limit as i64)? {
                edges.push(TraversalEdge {
                    depth: 1,
                    relationship: r,
                });
            }
        }
    }
    let mut seen = HashSet::new();
    edges.retain(|e| seen.insert(e.relationship.id.clone()));
    edges.sort_by(|a, b| {
        (a.depth, sort_key(&a.relationship)).cmp(&(b.depth, sort_key(&b.relationship)))
    });
    edges.truncate(limit);
    Ok((matches, edges))
}
