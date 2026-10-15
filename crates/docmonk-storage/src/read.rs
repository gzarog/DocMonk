//! The storage-neutral knowledge read port.
//!
//! Retrieval (lexical tiers, graph traversal, context expansion, semantic
//! candidates) reads one source's build through [`KnowledgeRead`]. The
//! local [`ProjectStore`] implements it over SQLite; the server backend
//! implements it over OpenSearch/Elasticsearch. Every method is scoped to
//! one `(source, build)` pair, so a reader can never see a record of
//! another source or of an unpublished build.
//!
//! FTS `expression`s use the restricted grammar retrieval builds
//! (`"tok"`, `"a b"`, `tok*`, joined by ` AND ` or ` OR `); see
//! [`parse_expression`].

use std::collections::HashMap;

use crate::error::Result;
use crate::knowledge::{ChunkRow, EntityRow, ProjectStore, RelationshipRow};
use crate::search::{
    AttachmentProvenance, DocumentSearchRow, EntityLinkRow, EntitySearchRow, PathSearchRow,
};
use crate::vectors::SubjectMeta;

/// How current a source's relationship graph is, as reported next to
/// graph-backed results (`search`, `explore`, `impact`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GraphFreshness {
    /// `ready`, `stale`, `pending`, `building`, `failed`, `disabled` or
    /// `unknown`.
    pub state: String,
    /// The graph generation graph queries read (`None` without one).
    pub generation: Option<String>,
    /// Graph queries return rows.
    pub visible: bool,
    /// The rows served were derived from an older base publication.
    pub stale: bool,
}

impl GraphFreshness {
    pub fn unknown() -> Self {
        Self {
            state: "unknown".into(),
            generation: None,
            visible: false,
            stale: false,
        }
    }
}

/// `(subject_type, subject_id, similarity)` of one vector candidate.
pub type VectorHit = (String, String, f32);

/// Read access to one source's knowledge. Object safe; not `Sync` (a SQLite
/// connection is per thread), so each worker opens its own readers.
pub trait KnowledgeRead {
    /// The source every record belongs to.
    fn source_id(&self) -> &str;

    /// `"sqlite"`, `"opensearch"` or `"elasticsearch"` (diagnostics).
    fn backend_kind(&self) -> &'static str;

    /// The local store, when this reader is one (local ANN index files).
    fn as_project_store(&self) -> Option<&ProjectStore> {
        None
    }

    fn search_entities_exact(&self, build_id: &str, q: &str) -> Result<Vec<EntitySearchRow>>;
    fn search_entities_alias(
        &self,
        build_id: &str,
        alias: &str,
        limit: i64,
    ) -> Result<Vec<EntitySearchRow>>;
    fn search_entities_fts(
        &self,
        build_id: &str,
        expression: &str,
        limit: i64,
    ) -> Result<Vec<EntitySearchRow>>;
    fn search_document_titles(
        &self,
        build_id: &str,
        title: &str,
        limit: i64,
    ) -> Result<Vec<DocumentSearchRow>>;
    fn search_chunks_fts(
        &self,
        build_id: &str,
        expression: &str,
        limit: i64,
        snippet_tokens: i64,
    ) -> Result<Vec<DocumentSearchRow>>;
    fn search_paths_projection(
        &self,
        build_id: &str,
        q: &str,
        limit: i64,
    ) -> Result<Vec<PathSearchRow>>;

    fn symbol_entities(&self, build_id: &str, q: &str) -> Result<Vec<EntityRow>>;
    fn entity(&self, build_id: &str, id: &str) -> Result<Option<EntityRow>>;
    fn entity_edges(
        &self,
        build_id: &str,
        entity_id: &str,
        relationship_type: Option<&str>,
        outgoing: bool,
        limit: i64,
    ) -> Result<Vec<RelationshipRow>>;
    fn unresolved_edges(
        &self,
        build_id: &str,
        symbol: &str,
        relationship_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RelationshipRow>>;
    fn file_rel_path(&self, build_id: &str, file_id: &str) -> Result<Option<String>>;

    /// [`Self::entity_edges`] for many entities at once: for each id, the
    /// edges of each of `types` (every type when empty), each type capped
    /// at `limit` exactly as one `entity_edges` call per id and type would
    /// return them. Ids without edges map to nothing.
    fn entity_edges_many(
        &self,
        build_id: &str,
        entity_ids: &[String],
        types: &[&str],
        outgoing: bool,
        limit: i64,
    ) -> Result<HashMap<String, Vec<RelationshipRow>>> {
        let mut out: HashMap<String, Vec<RelationshipRow>> = HashMap::new();
        for id in entity_ids {
            let mut edges = Vec::new();
            if types.is_empty() {
                edges = self.entity_edges(build_id, id, None, outgoing, limit)?;
            } else {
                for t in types {
                    edges.extend(self.entity_edges(build_id, id, Some(t), outgoing, limit)?);
                }
            }
            if !edges.is_empty() {
                out.entry(id.clone()).or_default().extend(edges);
            }
        }
        Ok(out)
    }

    /// [`Self::unresolved_edges`] for many symbols at once (same per-symbol
    /// and per-type caps).
    fn unresolved_edges_many(
        &self,
        build_id: &str,
        symbols: &[String],
        types: &[&str],
        limit: i64,
    ) -> Result<HashMap<String, Vec<RelationshipRow>>> {
        let mut out: HashMap<String, Vec<RelationshipRow>> = HashMap::new();
        for s in symbols {
            let mut edges = Vec::new();
            if types.is_empty() {
                edges = self.unresolved_edges(build_id, s, None, limit)?;
            } else {
                for t in types {
                    edges.extend(self.unresolved_edges(build_id, s, Some(t), limit)?);
                }
            }
            if !edges.is_empty() {
                out.entry(s.clone()).or_default().extend(edges);
            }
        }
        Ok(out)
    }

    /// Hint that a lexical query is about to run every tier for `q` (and
    /// each FTS `expression`): a remote reader answers all of them in one
    /// round-trip up front. The results the individual calls return are
    /// unchanged. The local store does nothing.
    fn prefetch_lexical(
        &self,
        _build_id: &str,
        _q: &str,
        _expressions: &[String],
        _limit: i64,
        _snippet_tokens: i64,
    ) -> Result<()> {
        Ok(())
    }

    /// The reader can be shared across threads (a remote reader); `None`
    /// for a SQLite connection, which belongs to one thread.
    fn as_sync(&self) -> Option<&(dyn KnowledgeRead + Sync)> {
        None
    }

    /// Entities of `ids` by id (unknown ids map to nothing).
    fn entities_by_ids(
        &self,
        build_id: &str,
        ids: &[String],
    ) -> Result<HashMap<String, EntityRow>> {
        let mut out = HashMap::new();
        for id in ids {
            if !out.contains_key(id) {
                if let Some(e) = self.entity(build_id, id)? {
                    out.insert(id.clone(), e);
                }
            }
        }
        Ok(out)
    }

    /// Source-relative paths of `file_ids` (unknown ids map to nothing).
    fn file_rel_paths(
        &self,
        build_id: &str,
        file_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        for f in file_ids {
            if !out.contains_key(f) {
                if let Some(p) = self.file_rel_path(build_id, f)? {
                    out.insert(f.clone(), p);
                }
            }
        }
        Ok(out)
    }
    fn entity_links(&self, build_id: &str, entity_id: &str) -> Result<Vec<EntityLinkRow>>;
    fn document_attachment(
        &self,
        build_id: &str,
        document_id: &str,
    ) -> Result<Option<AttachmentProvenance>>;

    /// Attachment provenance of each of `document_ids` (missing ids map to
    /// nothing; a top-level document maps to `None`).
    fn document_attachments(
        &self,
        build_id: &str,
        document_ids: &[String],
    ) -> Result<HashMap<String, Option<AttachmentProvenance>>> {
        let mut out = HashMap::new();
        for d in document_ids {
            if !out.contains_key(d) {
                out.insert(d.clone(), self.document_attachment(build_id, d)?);
            }
        }
        Ok(out)
    }

    fn chunk(&self, build_id: &str, id: &str) -> Result<Option<ChunkRow>>;
    fn chunk_at(&self, build_id: &str, document_id: &str, ordinal: i64)
        -> Result<Option<ChunkRow>>;
    fn chunk_siblings(
        &self,
        build_id: &str,
        c: &ChunkRow,
        previous: i64,
        next: i64,
    ) -> Result<(Vec<ChunkRow>, Vec<ChunkRow>)>;

    /// Server-side top-`k` vector candidates for `query`, or `None` when
    /// this backend answers vector queries elsewhere (the local store uses
    /// its own ANN index files). An empty `Some` means no vectors.
    /// `num_candidates` is the engine's candidate pool (0 = automatic).
    fn vector_search(
        &self,
        _build_id: &str,
        _fingerprint: &str,
        _query: &[f32],
        _k: usize,
        _num_candidates: usize,
    ) -> Result<Option<Vec<VectorHit>>> {
        Ok(None)
    }

    fn subject_meta(&self, build_id: &str, keys: &[(String, String)]) -> Result<Vec<SubjectMeta>>;

    /// The graph's freshness against `build_id`.
    fn graph_freshness(&self, _build_id: &str) -> Result<GraphFreshness> {
        Ok(GraphFreshness::unknown())
    }
}

impl KnowledgeRead for ProjectStore {
    fn source_id(&self) -> &str {
        ProjectStore::source_id(self)
    }
    fn backend_kind(&self) -> &'static str {
        "sqlite"
    }
    fn as_project_store(&self) -> Option<&ProjectStore> {
        Some(self)
    }
    fn search_entities_exact(&self, b: &str, q: &str) -> Result<Vec<EntitySearchRow>> {
        ProjectStore::search_entities_exact(self, b, q)
    }
    fn search_entities_alias(&self, b: &str, a: &str, l: i64) -> Result<Vec<EntitySearchRow>> {
        ProjectStore::search_entities_alias(self, b, a, l)
    }
    fn search_entities_fts(&self, b: &str, e: &str, l: i64) -> Result<Vec<EntitySearchRow>> {
        ProjectStore::search_entities_fts(self, b, e, l)
    }
    fn search_document_titles(&self, b: &str, t: &str, l: i64) -> Result<Vec<DocumentSearchRow>> {
        ProjectStore::search_document_titles(self, b, t, l)
    }
    fn search_chunks_fts(
        &self,
        b: &str,
        e: &str,
        l: i64,
        s: i64,
    ) -> Result<Vec<DocumentSearchRow>> {
        ProjectStore::search_chunks_fts(self, b, e, l, s)
    }
    fn search_paths_projection(&self, b: &str, q: &str, l: i64) -> Result<Vec<PathSearchRow>> {
        ProjectStore::search_paths_projection(self, b, q, l)
    }
    fn symbol_entities(&self, b: &str, q: &str) -> Result<Vec<EntityRow>> {
        ProjectStore::symbol_entities(self, b, q)
    }
    fn entity(&self, b: &str, id: &str) -> Result<Option<EntityRow>> {
        ProjectStore::entity(self, b, id)
    }
    fn entity_edges(
        &self,
        b: &str,
        e: &str,
        t: Option<&str>,
        out: bool,
        l: i64,
    ) -> Result<Vec<RelationshipRow>> {
        ProjectStore::entity_edges(self, b, e, t, out, l)
    }
    fn unresolved_edges(
        &self,
        b: &str,
        s: &str,
        t: Option<&str>,
        l: i64,
    ) -> Result<Vec<RelationshipRow>> {
        ProjectStore::unresolved_edges(self, b, s, t, l)
    }
    fn file_rel_path(&self, b: &str, f: &str) -> Result<Option<String>> {
        ProjectStore::file_rel_path(self, b, f)
    }
    fn entity_edges_many(
        &self,
        b: &str,
        ids: &[String],
        types: &[&str],
        out: bool,
        l: i64,
    ) -> Result<HashMap<String, Vec<RelationshipRow>>> {
        ProjectStore::entity_edges_many(self, b, ids, types, out, l)
    }
    fn unresolved_edges_many(
        &self,
        b: &str,
        symbols: &[String],
        types: &[&str],
        l: i64,
    ) -> Result<HashMap<String, Vec<RelationshipRow>>> {
        ProjectStore::unresolved_edges_many(self, b, symbols, types, l)
    }
    fn file_rel_paths(&self, b: &str, ids: &[String]) -> Result<HashMap<String, String>> {
        ProjectStore::file_rel_paths(self, b, ids)
    }
    fn entities_by_ids(&self, b: &str, ids: &[String]) -> Result<HashMap<String, EntityRow>> {
        ProjectStore::entities_by_ids(self, b, ids)
    }
    fn entity_links(&self, b: &str, e: &str) -> Result<Vec<EntityLinkRow>> {
        ProjectStore::entity_links(self, b, e)
    }
    fn document_attachment(&self, b: &str, d: &str) -> Result<Option<AttachmentProvenance>> {
        ProjectStore::document_attachment(self, b, d)
    }
    fn document_attachments(
        &self,
        b: &str,
        ids: &[String],
    ) -> Result<HashMap<String, Option<AttachmentProvenance>>> {
        ProjectStore::document_attachments(self, b, ids)
    }
    fn chunk(&self, b: &str, id: &str) -> Result<Option<ChunkRow>> {
        ProjectStore::chunk(self, b, id)
    }
    fn chunk_at(&self, b: &str, d: &str, o: i64) -> Result<Option<ChunkRow>> {
        ProjectStore::chunk_at(self, b, d, o)
    }
    fn chunk_siblings(
        &self,
        b: &str,
        c: &ChunkRow,
        p: i64,
        n: i64,
    ) -> Result<(Vec<ChunkRow>, Vec<ChunkRow>)> {
        ProjectStore::chunk_siblings(self, b, c, p, n)
    }
    fn subject_meta(&self, b: &str, keys: &[(String, String)]) -> Result<Vec<SubjectMeta>> {
        ProjectStore::subject_meta(self, b, keys)
    }
    fn graph_freshness(&self, b: &str) -> Result<GraphFreshness> {
        ProjectStore::graph_freshness(self, b)
    }
}

/// One term of a parsed FTS expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtsTerm {
    /// `"a"` or `"a b c"`: an exact token or phrase.
    Phrase(String),
    /// `tok*`: a token prefix.
    Prefix(String),
}

/// A parsed FTS expression: every term must match (`all`), or any may.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsExpression {
    pub all: bool,
    pub terms: Vec<FtsTerm>,
}

/// Parses the restricted FTS5 grammar retrieval generates. `None` for an
/// expression outside it (never produced by retrieval).
pub fn parse_expression(expr: &str) -> Option<FtsExpression> {
    let (all, parts): (bool, Vec<&str>) = if expr.contains(" OR ") {
        if expr.contains(" AND ") {
            return None;
        }
        (false, expr.split(" OR ").collect())
    } else {
        (true, expr.split(" AND ").collect())
    };
    let mut terms = Vec::new();
    for p in parts {
        let p = p.trim();
        if let Some(inner) = p.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
            if inner.is_empty() || inner.contains('"') {
                return None;
            }
            terms.push(FtsTerm::Phrase(inner.to_owned()));
        } else if let Some(stem) = p.strip_suffix('*') {
            if stem.is_empty() || stem.contains(['"', ' ', '*']) {
                return None;
            }
            terms.push(FtsTerm::Prefix(stem.to_owned()));
        } else {
            return None;
        }
    }
    (!terms.is_empty()).then_some(FtsExpression { all, terms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_generated_grammar() {
        assert_eq!(
            parse_expression("\"settle bet\""),
            Some(FtsExpression {
                all: true,
                terms: vec![FtsTerm::Phrase("settle bet".into())]
            })
        );
        assert_eq!(
            parse_expression("settle* AND \"in\""),
            Some(FtsExpression {
                all: true,
                terms: vec![
                    FtsTerm::Prefix("settle".into()),
                    FtsTerm::Phrase("in".into())
                ]
            })
        );
        let or = parse_expression("\"a\" OR \"b\"").unwrap();
        assert!(!or.all);
        assert_eq!(or.terms.len(), 2);
        assert!(parse_expression("a AND b").is_none());
        assert!(parse_expression("\"a\" OR \"b\" AND \"c\"").is_none());
    }

    fn assert_object_safe(_: &dyn KnowledgeRead) {}

    #[test]
    fn trait_is_object_safe() {
        let _ = assert_object_safe;
    }
}
