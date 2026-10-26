//! Exact URL anchors end to end over fixtures/endpoints (P02/P03/P05/P07):
//! the routed planner, the exact-occurrence projection (URL literals and
//! composed ASP.NET routes in the relationship graph), verified document
//! occurrences, negative controls and bounded graph expansion.

// clean-slate-audit: allow-start (API URL paths with a version segment are user data)

use std::path::{Path, PathBuf};

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_retrieval::explore::{self, ExploreOptions, Intent};
use docmonk_retrieval::Corpus;
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::Value;

const Q01: &str = "Is the URL path /sb/v1/content/groups/image used, referenced, or related to \
                   any code or documentation in the indexed DocMonk project? Trace matching \
                   route/client/API references and relevant call paths.";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_tree(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
        }
    }
}

struct Indexed {
    _tmp: tempfile::TempDir,
    store: ProjectStore,
    build: String,
}

fn index(rel: &str, with_graph: bool) -> Indexed {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    copy_tree(&repo_root().join(rel), &root);
    let home = Home::new(tmp.path().join("home"));
    let layout = StorageLayout::new(&home);
    let mut cp = ControlPlane::open(&layout, 8).unwrap();
    let canonical = docmonk_core::paths::resolve(&root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let src = cp
        .add_source(&NewSource {
            canonical_path: canonical,
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
        })
        .unwrap()
        .0;
    let cfg = docmonk_config::DocMonkConfig::default();
    run_source(
        &layout,
        &mut cp,
        &src,
        &docmonk_convert::registry_with(&cfg, &docmonk_convert::RegistryOptions::default()),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    let build = cp.state(&src.id).unwrap().active_build_id.unwrap();
    let mut store =
        ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
    if with_graph {
        docmonk_knowledge::build_graph(
            &mut store,
            std::path::Path::new(&src.path),
            &build,
            2,
            None,
            &mut |_, _| {},
        )
        .unwrap();
    }
    Indexed {
        _tmp: tmp,
        store,
        build,
    }
}

fn run(ix: &Indexed, q: &str, explain: bool) -> Value {
    let corpora = [Corpus {
        store: &ix.store,
        build_id: &ix.build,
    }];
    let plan = explore::plan_routed(q, true, Default::default());
    let mut embedded = 0;
    let v = explore::explore_with(
        &corpora,
        &plan,
        &mut |p| {
            // The lazy semantic stage is only reached without exact
            // evidence; it never runs here.
            assert!(!p.exact_evidence, "semantic asked despite exact evidence");
            embedded += 1;
            Ok(None)
        },
        ExploreOptions {
            explain,
            ..ExploreOptions::default()
        },
    )
    .unwrap();
    assert_eq!(embedded, 0, "no query embedding for well-supported anchors");
    v
}

fn occurrences(v: &Value) -> Vec<(String, String, Option<i64>, String)> {
    v["exact_occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["evidence_class"].as_str().unwrap().to_owned(),
                o["path"].as_str().unwrap().to_owned(),
                o["line"].as_i64(),
                o["matched"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn embedded_and_bare_url_questions_find_the_same_exact_evidence() {
    let ix = index("fixtures/endpoints", true);
    let q01 = run(&ix, Q01, true);
    assert_eq!(q01["intent"], "endpoint_reference");
    assert_eq!(
        q01["anchors"]["url_paths"][0]["path"],
        "/sb/v1/content/groups/image"
    );
    assert_eq!(q01["anchors"]["url_paths"].as_array().unwrap().len(), 1);
    let got = occurrences(&q01);
    let want = [
        (
            "direct_exact_literal",
            "src/Clients/ImageClient.cs",
            Some(5),
            "sb/v1/content/groups/image",
        ),
        (
            "direct_exact_literal",
            "src/Clients/ImageClient.cs",
            Some(9),
            "/sb/v1/content/groups/image",
        ),
        (
            "statically_composed_route",
            "src/Content/ContentController.cs",
            Some(7),
            "/sb/v1/content/groups/image",
        ),
        (
            "document_reference",
            "docs/operations.md",
            None,
            "/sb/v1/content/groups/image",
        ),
    ];
    for w in &want {
        assert!(
            got.iter()
                .any(|g| (g.0.as_str(), g.1.as_str(), g.2, g.3.as_str()) == *w),
            "missing {w:?} in {got:#?}"
        );
    }
    // Negative controls: image2 and the /image/thumbnail suffix are never
    // direct or document matches of /image.
    for g in &got {
        assert!(!g.3.contains("image2"), "{g:?}");
        assert!(!g.3.contains("thumbnail"), "{g:?}");
    }
    assert!(!got.iter().any(|g| g.1 == "docs/unrelated.md"), "{got:#?}");
    // The composed route's template sibling ({id}) is only heuristic.
    assert!(got
        .iter()
        .filter(|g| g.3.contains("{id}"))
        .all(|g| g.0 == "heuristic_match"));
    assert_eq!(q01["exact_complete"], true);
    assert_eq!(q01["exact_capabilities"][0]["exact_occurrences"], "indexed");
    // Call path: GalleryService.Refresh calls the client method holding
    // the literal.
    let flows: Vec<(String, String)> = q01["call_flows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["source"].as_str().unwrap().to_owned(),
                f["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        flows
            .iter()
            .any(|(s, t)| s.ends_with("GalleryService.Refresh")
                && t.ends_with("ImageClient.FetchGroupImage")),
        "{flows:?}"
    );
    // Diagnostics reconcile: every top-level stage is measured.
    let d = &q01["diagnostics"];
    let stages: Vec<&str> = d["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stage"].as_str().unwrap())
        .collect();
    assert!(stages.contains(&"exact_occurrences"), "{stages:?}");
    assert!(stages.contains(&"lexical"), "{stages:?}");
    assert!(d["stages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["duration_ms"].as_f64().unwrap() >= 0.0));
    assert_eq!(d["semantic"], "skipped_exact_evidence");
    assert_eq!(d["lexical_or_fallback"], false);
    assert_eq!(d["lexical_query"], "/sb/v1/content/groups/image");
    assert!(d["exact_rejected_document_candidates"].as_u64().unwrap() >= 1);
    assert_eq!(d["route"]["intent"], "endpoint_reference");

    // The bare endpoint has the same exact evidence.
    let bare = run(&ix, "/sb/v1/content/groups/image", false);
    assert_eq!(bare["intent"], "endpoint_reference");
    assert_eq!(occurrences(&bare), got);
    assert!(bare.get("diagnostics").is_none(), "explain is opt-in");
}

#[test]
fn a_near_miss_url_never_matches_the_shorter_one() {
    let ix = index("fixtures/endpoints", true);
    let v = run(&ix, "Where is /sb/v1/content/groups/image2 used?", false);
    let got = occurrences(&v);
    assert!(
        got.iter().any(|g| g.0 == "direct_exact_literal"
            && g.1 == "src/Clients/ImageClient.cs"
            && g.2 == Some(14)),
        "{got:#?}"
    );
    for g in &got {
        if g.0 != "heuristic_match" {
            assert!(g.3.ends_with("image2"), "{g:?}");
        }
    }
}

#[test]
fn a_source_without_the_projection_is_reported_not_absent() {
    // No relationship graph: nothing can be checked in code.
    let ix = index("fixtures/endpoints", false);
    let corpora = [Corpus {
        store: &ix.store,
        build_id: &ix.build,
    }];
    let plan = explore::plan_routed(Q01, false, Default::default());
    assert_eq!(plan.intent, Intent::Endpoint);
    let v = explore::explore(&corpora, &plan, None, ExploreOptions::default()).unwrap();
    assert_eq!(v["exact_complete"], false);
    assert_eq!(v["exact_capabilities"][0]["exact_occurrences"], "unknown");
    // Documents are still verified without the graph.
    assert!(occurrences(&v)
        .iter()
        .any(|g| g.0 == "document_reference" && g.1 == "docs/operations.md"));
}

#[test]
fn rollback_flags_restore_the_previous_behaviour() {
    let ix = index("fixtures/endpoints", true);
    let corpora = [Corpus {
        store: &ix.store,
        build_id: &ix.build,
    }];
    let plan = explore::plan_routed(
        Q01,
        false,
        docmonk_retrieval::route::RouteOptions {
            anchors_disabled: true,
            ..Default::default()
        },
    );
    assert_ne!(plan.intent, Intent::Endpoint);
    let v = explore::explore(&corpora, &plan, None, ExploreOptions::default()).unwrap();
    assert!(v.get("exact_occurrences").is_none());
    // Exact occurrences off: the endpoint route still plans, nothing exact.
    let plan = explore::plan_routed(Q01, false, Default::default());
    let mut opts = ExploreOptions::default();
    opts.exact.enabled = false;
    let v = explore::explore(&corpora, &plan, None, opts).unwrap();
    assert_eq!(v["intent"], "endpoint_reference");
    assert!(v.get("exact_occurrences").is_none());
}

/// Delegates to a store under another source id, counting reads by kind.
struct Counting<'a> {
    inner: &'a ProjectStore,
    id: String,
    lexical: std::cell::Cell<usize>,
    edges: std::cell::Cell<usize>,
    symbols: std::cell::Cell<usize>,
    unresolved: std::cell::Cell<usize>,
}

mod counting_impl {
    use super::Counting;
    use docmonk_storage::error::Result;
    use docmonk_storage::knowledge::{ChunkRow, EntityRow, RelationshipRow};
    use docmonk_storage::read::{KnowledgeRead, ReadFilter};
    use docmonk_storage::search::{
        AttachmentProvenance, DocumentSearchRow, EntityLinkRow, EntitySearchRow, PathSearchRow,
    };
    use docmonk_storage::vectors::SubjectMeta;
    use std::collections::HashMap;

    fn bump(c: &std::cell::Cell<usize>) {
        c.set(c.get() + 1);
    }

    impl KnowledgeRead for Counting<'_> {
        fn source_id(&self) -> &str {
            &self.id
        }
        fn backend_kind(&self) -> &'static str {
            "sqlite"
        }
        fn search_entities_exact(&self, b: &str, q: &str) -> Result<Vec<EntitySearchRow>> {
            bump(&self.lexical);
            self.inner.search_entities_exact(b, q)
        }
        fn search_entities_alias(&self, b: &str, a: &str, l: i64) -> Result<Vec<EntitySearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_entities_alias(self.inner, b, a, l)
        }
        fn search_entities_fts(&self, b: &str, e: &str, l: i64) -> Result<Vec<EntitySearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_entities_fts(self.inner, b, e, l)
        }
        fn search_passages_fts_in(
            &self,
            b: &str,
            e: &str,
            l: i64,
            s: i64,
            f: &ReadFilter,
        ) -> Result<Vec<docmonk_storage::search::PassageSearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_passages_fts_in(self.inner, b, e, l, s, f)
        }
        fn search_entities_fts_in(
            &self,
            b: &str,
            e: &str,
            l: i64,
            f: &ReadFilter,
        ) -> Result<Vec<EntitySearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_entities_fts_in(self.inner, b, e, l, f)
        }
        fn search_document_titles(
            &self,
            b: &str,
            t: &str,
            l: i64,
        ) -> Result<Vec<DocumentSearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_document_titles(self.inner, b, t, l)
        }
        fn search_chunks_fts(
            &self,
            b: &str,
            e: &str,
            l: i64,
            s: i64,
        ) -> Result<Vec<DocumentSearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_chunks_fts(self.inner, b, e, l, s)
        }
        fn search_chunks_fts_in(
            &self,
            b: &str,
            e: &str,
            l: i64,
            s: i64,
            f: &ReadFilter,
        ) -> Result<Vec<DocumentSearchRow>> {
            KnowledgeRead::search_chunks_fts_in(self.inner, b, e, l, s, f)
        }
        fn search_paths_projection(&self, b: &str, q: &str, l: i64) -> Result<Vec<PathSearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_paths_projection(self.inner, b, q, l)
        }
        fn search_paths_projection_in(
            &self,
            b: &str,
            q: &str,
            l: i64,
            f: &ReadFilter,
        ) -> Result<Vec<PathSearchRow>> {
            bump(&self.lexical);
            KnowledgeRead::search_paths_projection_in(self.inner, b, q, l, f)
        }
        fn symbol_entities(&self, b: &str, q: &str) -> Result<Vec<EntityRow>> {
            bump(&self.symbols);
            KnowledgeRead::symbol_entities(self.inner, b, q)
        }
        fn entity(&self, b: &str, id: &str) -> Result<Option<EntityRow>> {
            KnowledgeRead::entity(self.inner, b, id)
        }
        fn entity_edges(
            &self,
            b: &str,
            e: &str,
            t: Option<&str>,
            o: bool,
            l: i64,
        ) -> Result<Vec<RelationshipRow>> {
            bump(&self.edges);
            KnowledgeRead::entity_edges(self.inner, b, e, t, o, l)
        }
        fn entity_edges_many(
            &self,
            b: &str,
            ids: &[String],
            t: &[&str],
            o: bool,
            l: i64,
        ) -> Result<HashMap<String, Vec<RelationshipRow>>> {
            bump(&self.edges);
            KnowledgeRead::entity_edges_many(self.inner, b, ids, t, o, l)
        }
        fn unresolved_edges(
            &self,
            b: &str,
            s: &str,
            t: Option<&str>,
            l: i64,
        ) -> Result<Vec<RelationshipRow>> {
            bump(&self.unresolved);
            KnowledgeRead::unresolved_edges(self.inner, b, s, t, l)
        }
        fn unresolved_edges_many(
            &self,
            b: &str,
            s: &[String],
            t: &[&str],
            l: i64,
        ) -> Result<HashMap<String, Vec<RelationshipRow>>> {
            bump(&self.unresolved);
            KnowledgeRead::unresolved_edges_many(self.inner, b, s, t, l)
        }
        fn file_rel_path(&self, b: &str, f: &str) -> Result<Option<String>> {
            KnowledgeRead::file_rel_path(self.inner, b, f)
        }
        fn file_rel_paths(&self, b: &str, ids: &[String]) -> Result<HashMap<String, String>> {
            KnowledgeRead::file_rel_paths(self.inner, b, ids)
        }
        fn entities_by_ids(&self, b: &str, ids: &[String]) -> Result<HashMap<String, EntityRow>> {
            KnowledgeRead::entities_by_ids(self.inner, b, ids)
        }
        fn entity_links(&self, b: &str, e: &str) -> Result<Vec<EntityLinkRow>> {
            KnowledgeRead::entity_links(self.inner, b, e)
        }
        fn document_attachment(&self, b: &str, d: &str) -> Result<Option<AttachmentProvenance>> {
            KnowledgeRead::document_attachment(self.inner, b, d)
        }
        fn chunk(&self, b: &str, id: &str) -> Result<Option<ChunkRow>> {
            KnowledgeRead::chunk(self.inner, b, id)
        }
        fn chunk_at(&self, b: &str, d: &str, o: i64) -> Result<Option<ChunkRow>> {
            KnowledgeRead::chunk_at(self.inner, b, d, o)
        }
        fn chunk_siblings(
            &self,
            b: &str,
            c: &ChunkRow,
            p: i64,
            n: i64,
        ) -> Result<(Vec<ChunkRow>, Vec<ChunkRow>)> {
            KnowledgeRead::chunk_siblings(self.inner, b, c, p, n)
        }
        fn subject_meta(&self, b: &str, keys: &[(String, String)]) -> Result<Vec<SubjectMeta>> {
            KnowledgeRead::subject_meta(self.inner, b, keys)
        }
        fn graph_derivation(&self, b: &str) -> Result<Option<String>> {
            KnowledgeRead::graph_derivation(self.inner, b)
        }
    }
}

fn counting<'a>(inner: &'a ProjectStore, id: &str) -> Counting<'a> {
    Counting {
        inner,
        id: id.to_owned(),
        lexical: Default::default(),
        edges: Default::default(),
        symbols: Default::default(),
        unresolved: Default::default(),
    }
}

/// 150 sources, one holding the endpoint: every source gets one batched
/// exact lookup, but lexical tiers and graph traversal run only on the
/// matching one, and the evidence equals the single-source run.
#[test]
fn one_matching_source_of_150_is_the_only_one_expanded() {
    let hit = index("fixtures/endpoints", true);
    let empty = index("fixtures/graph", true);
    let mut readers = vec![counting(&hit.store, "s_hit")];
    for i in 0..149 {
        readers.push(counting(&empty.store, &format!("s_{i:03}")));
    }
    let corpora: Vec<Corpus<'_>> = readers
        .iter()
        .enumerate()
        .map(|(i, r)| Corpus {
            store: r,
            build_id: if i == 0 { &hit.build } else { &empty.build },
        })
        .collect();
    let plan = explore::plan_routed(Q01, false, Default::default());
    let v = explore::explore(
        &corpora,
        &plan,
        None,
        ExploreOptions {
            explain: true,
            ..ExploreOptions::default()
        },
    )
    .unwrap();
    let d = &v["diagnostics"];
    assert_eq!(d["source_count_considered"], 150);
    assert_eq!(d["source_count_searched"], 1);
    assert_eq!(d["source_count_expanded"], 1);
    assert_eq!(v["exact_capabilities"].as_array().unwrap().len(), 150);
    for r in &readers[1..] {
        assert_eq!(r.unresolved.get(), 1, "{}: one batched exact lookup", r.id);
        assert_eq!(r.lexical.get(), 0, "{}: no lexical tiers", r.id);
        assert_eq!(r.edges.get(), 0, "{}: no graph traversal", r.id);
        assert_eq!(r.symbols.get(), 0, "{}: no symbol lookup", r.id);
    }
    assert!(readers[0].lexical.get() > 0 && readers[0].edges.get() > 0);
    let single = run(&hit, Q01, false);
    assert_eq!(occurrences(&v), occurrences(&single));
}
// clean-slate-audit: allow-end
