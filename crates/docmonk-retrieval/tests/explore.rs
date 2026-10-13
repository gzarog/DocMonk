//! `impact` / `explore` against fixtures/expected/explore.json.
//!
//! Ids (entity, source, chunk, parent document) are dropped on both sides.
//! Lists whose order derives from those ids or from insertion order are
//! compared as multisets: documentation, documents,
//! evidence, call flows and dependencies. Everything else must be equal.

use std::path::{Path, PathBuf};

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_retrieval::explore::{self, ExploreOptions};
use docmonk_retrieval::Corpus;
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::Value;

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
    let cfg = docmonk_config::RagMonkConfig::default();
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

/// Drops the keys the generator drops.
fn strip(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "entity_id" | "source_id" | "id" | "parent_document_id"
                    )
                })
                .map(|(k, v)| (k.clone(), strip(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip).collect()),
        other => other.clone(),
    }
}

/// Replaces order-insensitive lists with their sorted serializations.
fn canonical(mut v: Value) -> Value {
    for key in [
        "documentation",
        "documents",
        "evidence",
        "call_flows",
        "dependencies",
    ] {
        if let Some(Value::Array(a)) = v.get_mut(key) {
            let mut s: Vec<String> = a.iter().map(Value::to_string).collect();
            s.sort();
            *a = s.into_iter().map(Value::String).collect();
        }
    }
    v
}

fn diff(got: &Value, want: &Value) -> Vec<String> {
    let (g, w) = (got.as_object().unwrap(), want.as_object().unwrap());
    let mut keys: Vec<&String> = g.keys().chain(w.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| g.get(*k) != w.get(*k))
        .map(|k| {
            format!(
                "{k}: got {} want {}",
                g.get(k).unwrap_or(&Value::Null),
                w.get(k).unwrap_or(&Value::Null)
            )
        })
        .collect()
}

#[test]
fn impact_and_explore_are_as_expected() {
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/explore.json")).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for name in ["linking", "graph"] {
        let ix = index(&format!("fixtures/{name}"));
        let corpora = [Corpus {
            store: &ix.store,
            build_id: &ix.build,
        }];
        for want in golden[name]["impact"].as_array().unwrap() {
            checked += 1;
            let q = want["query"].as_str().unwrap();
            let depth = want["max_depth"].as_u64().unwrap() as usize;
            let mut got = strip(&explore::impact(&corpora, q, depth, 100).unwrap());
            got["max_depth"] = want["max_depth"].clone();
            for d in diff(&canonical(got), &canonical(want.clone())) {
                failures.push(format!("[{name}] impact {q:?}@{depth} {d}"));
            }
        }
        for want in golden[name]["explore"].as_array().unwrap() {
            checked += 1;
            let q = want["query"].as_str().unwrap();
            let plan = explore::plan(q, false);
            let got =
                strip(&explore::explore(&corpora, &plan, None, ExploreOptions::default()).unwrap());
            for d in diff(&canonical(got), &canonical(want.clone())) {
                failures.push(format!("[{name}] explore {q:?} {d}"));
            }
        }
    }
    for f in failures.iter().take(30) {
        eprintln!("{}", f.chars().take(900).collect::<String>());
    }
    assert!(
        failures.is_empty(),
        "{} differences over {checked} payloads",
        failures.len()
    );
}
