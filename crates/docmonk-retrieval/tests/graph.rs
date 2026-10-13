//! Symbol and graph queries against fixtures/expected/graph.json.
//!
//! Entities and edges are compared in id-free form. Within one depth, edges
//! are ordered by entity and relationship ids, which are not part of the
//! expectation, so each depth is compared as a multiset. The depth
//! order itself must match.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_retrieval::graph::{
    self, Direction, ResolvedEdge, TraversalEdge, CALL_TYPES, REFERENCE_TYPES,
};
use docmonk_retrieval::Corpus;
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

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

fn index(rel: &str) -> Indexed {
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
    // Phase 2: the relationship graph of the published build.
    docmonk_knowledge::build_graph(
        &mut store,
        std::path::Path::new(&src.path),
        &build,
        2,
        None,
        &mut |_, _| {},
    )
    .unwrap();
    Indexed {
        _tmp: tmp,
        store,
        build,
    }
}

/// entity id -> `[path, qualified_name, kind, start_line]`.
fn entity_keys(ix: &Indexed) -> HashMap<String, Value> {
    let paths: HashMap<String, String> = ix
        .store
        .files(&ix.build)
        .unwrap()
        .into_iter()
        .map(|f| (f.id, f.rel_path))
        .collect();
    ix.store
        .all_entities(&ix.build)
        .unwrap()
        .into_iter()
        .map(|e| {
            let key = json!([paths[&e.file_id], e.qualified_name, e.kind, e.start_line]);
            (e.id, key)
        })
        .collect()
}

fn edge(ent: &HashMap<String, Value>, e: &TraversalEdge) -> Value {
    let r = &e.relationship;
    let target = match &r.target_entity_id {
        Some(id) => ent.get(id).cloned().unwrap_or(Value::Null),
        None => json!(["symbol", r.target_symbol]),
    };
    json!([
        e.depth,
        r.relationship_type,
        ent.get(&r.source_entity_id),
        target,
        r.confidence,
        r.resolver
    ])
}

fn resolved(ent: &HashMap<String, Value>, r: &ResolvedEdge) -> Value {
    let mut v = edge(ent, &r.edge);
    v.as_array_mut().unwrap().push(
        r.neighbor_entity
            .as_ref()
            .map_or(Value::Null, |n| ent[&n.id].clone()),
    );
    v
}

/// Depth sequence plus one sorted multiset per depth.
fn canonical(edges: &[Value]) -> (Vec<u64>, BTreeMap<u64, Vec<String>>) {
    let depths: Vec<u64> = edges.iter().map(|e| e[0].as_u64().unwrap()).collect();
    let mut by_depth: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for e in edges {
        by_depth
            .entry(e[0].as_u64().unwrap())
            .or_default()
            .push(e.to_string());
    }
    for v in by_depth.values_mut() {
        v.sort();
    }
    (depths, by_depth)
}

/// Same as [`canonical`] without the depth sequence (resolved edge lists
/// are concatenated per match, not depth-sorted, in the expectation).
fn multiset(edges: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = edges.iter().map(Value::to_string).collect();
    v.sort();
    v
}

#[test]
fn symbol_and_graph_queries_are_as_expected() {
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/graph.json")).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for name in ["graph", "code", "linking"] {
        let ix = index(&format!("fixtures/{name}"));
        let ent = entity_keys(&ix);
        let corpora = [Corpus {
            store: &ix.store,
            build_id: &ix.build,
        }];
        for item in golden[name]["queries"].as_array().unwrap() {
            let q = item["query"].as_str().unwrap();
            let mut fail = |what: &str, got: String, want: String| {
                if got != want {
                    failures.push(format!(
                        "[{name}] {q:?} {what}:\n   got  {got}\n   want {want}"
                    ));
                }
            };
            let matches = graph::find_symbol_matches(&corpora, q).unwrap();
            let got: Vec<Value> = matches.iter().map(|m| ent[&m.entity.id].clone()).collect();
            let want: Vec<Value> = item["symbol"].as_array().unwrap().clone();
            // Same-named overloads are ordered by ids, which are not part of
            // the expectation: compare the qualified-name sequence, then the set.
            let names = |v: &[Value]| v.iter().map(|e| e[1].to_string()).collect::<Vec<_>>();
            fail(
                "symbol order",
                format!("{:?}", names(&got)),
                format!("{:?}", names(&want)),
            );
            fail(
                "symbol",
                format!("{:?}", multiset(&got)),
                format!("{:?}", multiset(&want)),
            );
            for depth in [1usize, 3] {
                checked += 1;
                let exp = &item[format!("d{depth}")];
                let lists = |v: &Value| v.as_array().unwrap().clone();
                let (_, callers) = graph::traverse_symbol(
                    &corpora,
                    q,
                    Direction::Incoming,
                    &CALL_TYPES,
                    depth,
                    100,
                )
                .unwrap();
                let (_, callees) = graph::traverse_symbol(
                    &corpora,
                    q,
                    Direction::Outgoing,
                    &CALL_TYPES,
                    depth,
                    100,
                )
                .unwrap();
                let (m, refs) = graph::references(&corpora, q, depth, 100).unwrap();
                let inc = graph::resolved_incoming(&corpora, &m, q, &REFERENCE_TYPES, depth, 100)
                    .unwrap();
                let out =
                    graph::resolved_outgoing(&corpora, &m, &REFERENCE_TYPES, depth, 100).unwrap();
                let tests = graph::find_tests_referencing(&corpora, &m, q, depth, 100).unwrap();
                let traversed = [
                    ("callers", &callers),
                    ("callees", &callees),
                    ("references", &refs),
                ];
                for (what, edges) in traversed {
                    let got: Vec<Value> = edges.iter().map(|e| edge(&ent, e)).collect();
                    fail(
                        &format!("{what}@{depth}"),
                        format!("{:?}", canonical(&got)),
                        format!("{:?}", canonical(&lists(&exp[what]))),
                    );
                }
                let res = [("incoming", &inc), ("outgoing", &out), ("tests", &tests)];
                for (what, edges) in res {
                    let got: Vec<Value> = edges.iter().map(|e| resolved(&ent, e)).collect();
                    fail(
                        &format!("{what}@{depth}"),
                        format!("{:?}", multiset(&got)),
                        format!("{:?}", multiset(&lists(&exp[what]))),
                    );
                }
            }
        }
    }
    for f in failures.iter().take(25) {
        eprintln!("{f}");
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} query/depth checks differ",
        failures.len()
    );
}
