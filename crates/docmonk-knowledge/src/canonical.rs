//! Canonical fingerprint of a build's relationship graph.
//!
//! Every edge field (ids, type, endpoints, resolver, confidence, location,
//! evidence, reference text) and every link field, ordered by id, hashed
//! with SHA-256. Two graphs with equal fingerprints are identical row for
//! row, so the fingerprint is the equivalence check between an optimized
//! and a reference derivation (benchmarks and regression tests). Only
//! counts and the digest leave this module: no row content is reported.

use docmonk_storage::knowledge::{ProjectStore, RelationshipRow};
use docmonk_storage::StorageError;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphFingerprint {
    pub edges: usize,
    pub links: usize,
    /// SHA-256 over the canonical edge rows, then the canonical link rows.
    pub digest: String,
}

/// Fingerprints the graph rows recorded for `build_id` (visible or not).
pub fn fingerprint(store: &ProjectStore, build_id: &str) -> Result<GraphFingerprint, StorageError> {
    let mut edges = edge_rows(store, build_id)?;
    edges.sort_by(|a, b| a.id.cmp(&b.id));
    let mut links = store.links(build_id)?;
    links.sort_by(|a, b| a.id.cmp(&b.id));
    let mut h = Sha256::new();
    for e in &edges {
        h.update(serde_json::to_vec(e).unwrap_or_default());
        h.update([b'\n']);
    }
    h.update([0]);
    for l in &links {
        let fields = [
            Some(l.id.as_str()),
            Some(l.link_type.as_str()),
            Some(l.entity_id.as_str()),
            Some(l.document_id.as_str()),
            l.chunk_id.as_deref(),
            Some(l.resolver.as_str()),
            Some(l.confidence.as_str()),
            l.evidence.as_deref(),
        ];
        h.update(serde_json::to_vec(&fields).unwrap_or_default());
        h.update([b'\n']);
    }
    Ok(GraphFingerprint {
        edges: edges.len(),
        links: links.len(),
        digest: h.finalize().iter().map(|b| format!("{b:02x}")).collect(),
    })
}

/// Every relationship row of the build in one statement (independent of
/// the per-file and staging read paths it is used to check).
fn edge_rows(store: &ProjectStore, build_id: &str) -> Result<Vec<RelationshipRow>, StorageError> {
    let sql = "SELECT id, file_id, relationship_type, source_entity_id, target_entity_id,
                      target_symbol, resolver, confidence, source_location, evidence,
                      reference_text
               FROM relationships WHERE build_id = ?1";
    let what = "graph fingerprint";
    let mut stmt = store
        .connection()
        .prepare(sql)
        .map_err(StorageError::sqlite(what))?;
    let rows = stmt
        .query_map([build_id], |r| {
            Ok(RelationshipRow {
                id: r.get(0)?,
                file_id: r.get(1)?,
                relationship_type: r.get(2)?,
                source_entity_id: r.get(3)?,
                target_entity_id: r.get(4)?,
                target_symbol: r.get(5)?,
                resolver: r.get(6)?,
                confidence: r.get(7)?,
                source_location: r.get(8)?,
                evidence: r.get(9)?,
                reference_text: r.get(10)?,
            })
        })
        .map_err(StorageError::sqlite(what))?;
    rows.collect::<Result<_, _>>()
        .map_err(StorageError::sqlite(what))
}
