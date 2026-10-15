//! Batched knowledge reads (P1.5): traversal, impact and symbol lookups
//! return exactly what the one-row-at-a-time reads return, with far fewer
//! SQL statements.
//!
//! [`Unbatched`] forwards every single-item read of the local store but
//! keeps the port's looping defaults for the batched methods, so it is the
//! reference: one statement per node, type and file.

use std::collections::HashMap;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::graph::{self, Direction};
use docmonk_retrieval::{explore, Corpus};
use docmonk_service::bench::corpus;
use docmonk_service::query::open_sources;
use docmonk_storage::error::Result;
use docmonk_storage::knowledge::{ChunkRow, EntityRow, ProjectStore, RelationshipRow};
use docmonk_storage::read::KnowledgeRead;
use docmonk_storage::search::{
    AttachmentProvenance, DocumentSearchRow, EntityLinkRow, EntitySearchRow, PathSearchRow,
};
use docmonk_storage::stats::sql_statements;
use docmonk_storage::vectors::SubjectMeta;

struct Unbatched<'a>(&'a ProjectStore);

impl KnowledgeRead for Unbatched<'_> {
    fn source_id(&self) -> &str {
        self.0.source_id()
    }
    fn backend_kind(&self) -> &'static str {
        "sqlite"
    }
    fn search_entities_exact(&self, b: &str, q: &str) -> Result<Vec<EntitySearchRow>> {
        self.0.search_entities_exact(b, q)
    }
    fn search_entities_alias(&self, b: &str, a: &str, l: i64) -> Result<Vec<EntitySearchRow>> {
        self.0.search_entities_alias(b, a, l)
    }
    fn search_entities_fts(&self, b: &str, e: &str, l: i64) -> Result<Vec<EntitySearchRow>> {
        self.0.search_entities_fts(b, e, l)
    }
    fn search_document_titles(&self, b: &str, t: &str, l: i64) -> Result<Vec<DocumentSearchRow>> {
        self.0.search_document_titles(b, t, l)
    }
    fn search_chunks_fts(
        &self,
        b: &str,
        e: &str,
        l: i64,
        s: i64,
    ) -> Result<Vec<DocumentSearchRow>> {
        self.0.search_chunks_fts(b, e, l, s)
    }
    fn search_paths_projection(&self, b: &str, q: &str, l: i64) -> Result<Vec<PathSearchRow>> {
        self.0.search_paths_projection(b, q, l)
    }
    fn symbol_entities(&self, b: &str, q: &str) -> Result<Vec<EntityRow>> {
        self.0.symbol_entities(b, q)
    }
    fn entity(&self, b: &str, id: &str) -> Result<Option<EntityRow>> {
        self.0.entity(b, id)
    }
    fn entity_edges(
        &self,
        b: &str,
        e: &str,
        t: Option<&str>,
        out: bool,
        l: i64,
    ) -> Result<Vec<RelationshipRow>> {
        self.0.entity_edges(b, e, t, out, l)
    }
    fn unresolved_edges(
        &self,
        b: &str,
        s: &str,
        t: Option<&str>,
        l: i64,
    ) -> Result<Vec<RelationshipRow>> {
        self.0.unresolved_edges(b, s, t, l)
    }
    fn file_rel_path(&self, b: &str, f: &str) -> Result<Option<String>> {
        self.0.file_rel_path(b, f)
    }
    fn entity_links(&self, b: &str, e: &str) -> Result<Vec<EntityLinkRow>> {
        self.0.entity_links(b, e)
    }
    fn document_attachment(&self, b: &str, d: &str) -> Result<Option<AttachmentProvenance>> {
        self.0.document_attachment(b, d)
    }
    fn chunk(&self, b: &str, id: &str) -> Result<Option<ChunkRow>> {
        self.0.chunk(b, id)
    }
    fn chunk_at(&self, b: &str, d: &str, o: i64) -> Result<Option<ChunkRow>> {
        self.0.chunk_at(b, d, o)
    }
    fn chunk_siblings(
        &self,
        b: &str,
        c: &ChunkRow,
        p: i64,
        n: i64,
    ) -> Result<(Vec<ChunkRow>, Vec<ChunkRow>)> {
        self.0.chunk_siblings(b, c, p, n)
    }
    fn subject_meta(&self, b: &str, keys: &[(String, String)]) -> Result<Vec<SubjectMeta>> {
        self.0.subject_meta(b, keys)
    }
}

#[test]
fn batched_graph_reads_match_and_cut_statements() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("shop");
    corpus::emit(&root, 300, 10);
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let mut cat = docmonk_service::sources::catalog(&home).unwrap();
    let canonical = std::fs::canonicalize(&root).unwrap();
    cat.add(canonical.to_str().unwrap(), vec![], vec![])
        .unwrap();
    let sources = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &sources, "index", |_| {})
        .unwrap()
        .into_result()
        .unwrap();
    let opened = open_sources(&home, None).unwrap();
    let store = opened[0].store.local().unwrap();
    let build = opened[0].build.as_str();
    let batched = [Corpus {
        store,
        build_id: build,
    }];
    let reference_store = Unbatched(store);
    let reference = [Corpus {
        store: &reference_store,
        build_id: build,
    }];

    let mut counts: HashMap<&str, (u64, u64)> = HashMap::new();
    let mut measure = |name: &'static str, f: &dyn Fn(&[Corpus<'_>]) -> serde_json::Value| {
        let t = sql_statements();
        let a = f(&reference);
        let unbatched = sql_statements() - t;
        let t = sql_statements();
        let b = f(&batched);
        let n = sql_statements() - t;
        assert_eq!(a, b, "{name}: batched output differs");
        let e = counts.entry(name).or_default();
        e.0 += unbatched;
        e.1 += n;
    };
    for symbol in ["Log", "NotNull", "Save"] {
        measure("impact", &|c| {
            explore::impact(c, symbol, 3, graph::DEFAULT_LIMIT).unwrap()
        });
        measure("callers", &|c| {
            let (m, e) = graph::traverse_symbol(
                c,
                symbol,
                Direction::Incoming,
                &graph::CALL_TYPES,
                3,
                graph::DEFAULT_LIMIT,
            )
            .unwrap();
            serde_json::json!([
                m.iter()
                    .map(|m| (&m.entity.id, &m.rel_path))
                    .collect::<Vec<_>>(),
                e.iter()
                    .map(|e| (e.depth, &e.relationship.id))
                    .collect::<Vec<_>>()
            ])
        });
    }
    eprintln!("statements (unbatched, batched): {counts:?}");
    let (unbatched, batched_n) = counts["impact"];
    assert!(
        unbatched >= 10 * batched_n,
        "impact at depth 3: {unbatched} statements unbatched vs {batched_n} batched"
    );
}
