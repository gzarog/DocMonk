//! Cross-domain links over fixtures/linking against
//! fixtures/expected/links.json, plus incremental and manual-link behaviour.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use docmonk_core::paths::project_id_for_canonical;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_knowledge::manual;
use docmonk_storage::control::{ControlPlane, SourceRecord};
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
            let text = String::from_utf8(std::fs::read(&p).unwrap())
                .unwrap()
                .replace("\r\n", "\n");
            std::fs::write(dest, text).unwrap();
        }
    }
}

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    layout: StorageLayout,
    cp: ControlPlane,
    src: SourceRecord,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    copy_tree(&repo_root().join("fixtures/linking"), &root);
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    Fx {
        _tmp: tmp,
        root,
        layout,
        cp,
        src,
    }
}

impl Fx {
    fn index(&mut self) {
        let cfg = docmonk_config::DocMonkConfig::default();
        let r = run_source(
            &self.layout,
            &mut self.cp,
            &self.src,
            &docmonk_convert::registry(&cfg),
            &Options::from_config(&cfg),
            &mut NoProgress,
        )
        .unwrap();
        assert!(r.published || r.build_id.is_some(), "{r:?}");
        // Phase 2: links are part of the relationship graph.
        let (mut store, active) = self.store();
        docmonk_knowledge::build_graph(&mut store, &self.root, &active, 2, None, &mut |_, _| {})
            .unwrap();
    }

    fn store(&self) -> (ProjectStore, String) {
        let active = self
            .cp
            .state(&self.src.id)
            .unwrap()
            .active_build_id
            .unwrap();
        let s = ProjectStore::open(
            &self.layout,
            &project_id_for_canonical(&self.src.path),
            &self.src.id,
            8,
        )
        .unwrap();
        (s, active)
    }

    /// Links in canonical, id-free form.
    fn canonical(&self) -> Vec<Value> {
        let (s, b) = self.store();
        let paths: HashMap<String, String> = s
            .files(&b)
            .unwrap()
            .into_iter()
            .map(|f| (f.id, f.rel_path))
            .collect();
        let ents: HashMap<String, _> = s
            .all_entities(&b)
            .unwrap()
            .into_iter()
            .map(|e| (e.id.clone(), e))
            .collect();
        let docs: HashMap<String, String> = s
            .documents(&b)
            .unwrap()
            .into_iter()
            .map(|d| (d.id, d.file_id))
            .collect();
        let ords: HashMap<String, i64> = s
            .link_units(&b)
            .unwrap()
            .into_iter()
            .map(|u| (u.id, u.ordinal))
            .collect();
        let mut out: Vec<Value> = s
            .links(&b)
            .unwrap()
            .into_iter()
            .map(|l| {
                let e = &ents[&l.entity_id];
                json!({
                    "entity": [paths[&e.file_id], e.qualified_name, e.kind, e.start_line],
                    "document": paths[&docs[&l.document_id]],
                    "section": l.chunk_id.as_ref().map(|c| ords[c]),
                    "type": l.link_type, "resolver": l.resolver,
                    "confidence": l.confidence, "evidence": l.evidence,
                })
            })
            .collect();
        out.sort_by_key(key);
        out
    }
}

fn key(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let b: std::collections::BTreeMap<_, _> =
                    m.iter().map(|(k, x)| (k.clone(), sorted(x))).collect();
                Value::Object(b.into_iter().collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            x => x.clone(),
        }
    }
    serde_json::to_string(&sorted(v)).unwrap()
}

#[test]
fn cold_links_are_as_expected() {
    let mut f = fixture();
    f.index();
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/links.json")).unwrap(),
    )
    .unwrap();
    let mut want: Vec<Value> = golden["links"].as_array().unwrap().clone();
    want.sort_by_key(key);
    let got = f.canonical();
    let only_rust: Vec<_> = got.iter().filter(|x| !want.contains(x)).collect();
    let only_py: Vec<_> = want.iter().filter(|x| !got.contains(x)).collect();
    assert!(
        only_rust.is_empty() && only_py.is_empty(),
        "only got: {only_rust:#?}\nonly expected: {only_py:#?}"
    );
}

#[test]
fn incremental_relinking_equals_a_cold_build() {
    let mut f = fixture();
    f.index();
    // Edit a document and a code file; drop a mention.
    std::fs::write(
        f.root.join("docs/unicode.txt"),
        "Only Ledger here, and formatCurrency too\n",
    )
    .unwrap();
    let ts = std::fs::read_to_string(f.root.join("web/orders.ts")).unwrap();
    std::fs::write(
        f.root.join("web/orders.ts"),
        ts.replace("formatCurrency", "formatMoney"),
    )
    .unwrap();
    f.index();
    let incremental = f.canonical();

    let mut cold = fixture();
    copy_tree(&f.root, &cold.root);
    cold.index();
    assert_eq!(incremental, cold.canonical());
    assert!(!incremental
        .iter()
        .any(|l| l["evidence"] == "formatCurrency"));
}

#[test]
fn manual_links_are_exact_persistent_and_removable() {
    let mut f = fixture();
    f.index();
    let (mut s, b) = f.store();
    let link = manual::add(
        &mut s,
        &b,
        "web.orders.OrderController.cancelOrder",
        "orders.html",
        Some(1),
        Some("by hand".into()),
        "t",
    )
    .unwrap()
    .expect("new link");
    assert!(manual::add(
        &mut s,
        &b,
        "web.orders.OrderController.cancelOrder",
        "orders.html",
        Some(1),
        None,
        "t"
    )
    .unwrap()
    .is_none());
    assert!(manual::add(&mut s, &b, "nope", "orders.html", None, None, "t").is_err());
    let user = |f: &Fx| {
        f.canonical()
            .into_iter()
            .filter(|l| l["resolver"] == "user")
            .collect::<Vec<_>>()
    };
    let u = user(&f);
    assert_eq!(u.len(), 1);
    assert_eq!(
        (u[0]["confidence"].as_str(), u[0]["section"].as_i64()),
        (Some("exact"), Some(1))
    );

    // Survives a rebuild that rewrites both sides.
    let ts = std::fs::read_to_string(f.root.join("web/orders.ts")).unwrap();
    std::fs::write(f.root.join("web/orders.ts"), format!("{ts}\n// touched\n")).unwrap();
    let html = std::fs::read_to_string(f.root.join("docs/orders.html")).unwrap();
    std::fs::write(
        f.root.join("docs/orders.html"),
        html.replace("Orders", "Orders!"),
    )
    .unwrap();
    drop(s);
    f.index();
    assert_eq!(user(&f).len(), 1, "manual link survives reprocessing");

    let (mut s, b) = f.store();
    assert!(manual::remove(&mut s, &b, &link.id).unwrap());
    assert!(user(&f).is_empty());
}

#[test]
fn links_appear_only_after_the_graph_stage_and_manual_intent_is_reapplied() {
    let mut f = fixture();
    let cfg = docmonk_config::DocMonkConfig::default();
    run_source(
        &f.layout,
        &mut f.cp,
        &f.src,
        &docmonk_convert::registry(&cfg),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .unwrap();
    let (s, b) = f.store();
    assert!(s.links(&b).unwrap().is_empty(), "phase 1 wrote links");
    assert!(s.entity_links(&b, "anything").unwrap().is_empty());
    drop(s);
    let (mut s, b) = f.store();
    docmonk_knowledge::build_graph(&mut s, &f.root, &b, 2, None, &mut |_, _| {}).unwrap();
    assert!(!s.links(&b).unwrap().is_empty());
    manual::add(
        &mut s,
        &b,
        "web.orders.OrderController.cancelOrder",
        "orders.html",
        None,
        None,
        "t",
    )
    .unwrap()
    .expect("new link");
    // A full graph recomputation keeps the user's link.
    s.put_graph_state_with_files(&docmonk_storage::graph::GraphState::default())
        .unwrap();
    let g = docmonk_knowledge::build_graph(&mut s, &f.root, &b, 2, None, &mut |_, _| {}).unwrap();
    assert!(g.full);
    assert_eq!(
        s.links(&b)
            .unwrap()
            .iter()
            .filter(|l| l.resolver == manual::USER_RESOLVER)
            .count(),
        1
    );
}

#[test]
fn loading_only_the_touched_code_files_links_exactly_like_loading_the_build() {
    use docmonk_knowledge::linker::{link_touched_files_with, LinkLoad};
    let mut f = fixture();
    f.index();
    let (store, active) = f.store();
    let files: Vec<String> = store
        .files(&active)
        .unwrap()
        .into_iter()
        .map(|f| f.id)
        .collect();
    let mut narrow_passes = 0;
    let mut sets: Vec<Vec<String>> = files.iter().map(|f| vec![f.clone()]).collect();
    sets.push(files.clone());
    sets.push(files.iter().step_by(2).cloned().collect());
    for touched in sets {
        let (auto, stats) =
            link_touched_files_with(&store, &active, &touched, LinkLoad::Auto).unwrap();
        let (build, _) =
            link_touched_files_with(&store, &active, &touched, LinkLoad::Build).unwrap();
        assert_eq!(auto, build, "{touched:?}");
        narrow_passes += usize::from(stats.narrow);
    }
    assert!(narrow_passes > 0, "some single code files load narrowly");
}
