//! Scans and step-by-step change counts against
//! `fixtures/expected/indexing.json`.

use crate::common;

use docmonk_indexing::classify::classify;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options, Registry};
use docmonk_indexing::ignore::IgnoreMatcher;
use docmonk_indexing::scan::{scan, ScanOptions};
use docmonk_storage::StorageLayout;
use serde_json::Value;

fn golden() -> Value {
    let p = format!(
        "{}/../../fixtures/expected/indexing.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn scan_and_change_counts_are_as_expected() {
    let g = golden();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("src_root");
    std::fs::create_dir_all(&root).unwrap();
    for (rel, content) in g["tree"].as_object().unwrap() {
        common::write(&root, rel, content.as_str().unwrap());
    }
    let include = strs(&g["include"]);
    let exclude = strs(&g["exclude"]);

    // Scan results.
    let resolved = docmonk_core::paths::resolve(&root).unwrap();
    let m = IgnoreMatcher::new(&resolved, &exclude, &include);
    let out = scan(&root, &m, &ScanOptions::default()).unwrap();
    let mut got: Vec<(String, String)> = out
        .files
        .iter()
        .map(|f| (f.rel_path.clone(), classify(&f.path).as_str().to_owned()))
        .collect();
    got.sort();
    let want: Vec<(String, String)> = g["scan"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["rel_path"].as_str().unwrap().to_owned(),
                e["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(got, want);

    // Step-by-step counts.
    let home = common::home(tmp.path());
    let mut cp = common::control(&home);
    let inc: Vec<&str> = include.iter().map(String::as_str).collect();
    let exc: Vec<&str> = exclude.iter().map(String::as_str).collect();
    let source = common::add_source(&mut cp, &root, &inc, &exc);
    let layout = StorageLayout::new(&home);
    let registry = Registry::raw();
    let opts = Options::from_config(&docmonk_config::DocMonkConfig::default());
    for step in g["steps"].as_array().unwrap() {
        if let Some(w) = step.get("write").and_then(Value::as_object) {
            for (rel, c) in w {
                common::write(&root, rel, c.as_str().unwrap());
            }
        }
        for rel in step
            .get("touch")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            common::bump_mtime(&root.join(rel.as_str().unwrap()), 10);
        }
        if let Some(r) = step.get("rename").and_then(Value::as_object) {
            for (old, new) in r {
                std::fs::rename(root.join(old), root.join(new.as_str().unwrap())).unwrap();
            }
        }
        for rel in step
            .get("delete")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            std::fs::remove_file(root.join(rel.as_str().unwrap())).unwrap();
        }
        let r = run_source(&layout, &mut cp, &source, &registry, &opts, &mut NoProgress).unwrap();
        let c = &step["counts"];
        let got = [
            r.counts.scanned,
            r.counts.new,
            r.counts.changed,
            r.counts.unchanged,
            r.counts.moved,
            r.counts.deleted,
        ];
        let want: Vec<usize> = ["scanned", "new", "changed", "unchanged", "moved", "deleted"]
            .iter()
            .map(|k| c[*k].as_u64().unwrap() as usize)
            .collect();
        assert_eq!(got.to_vec(), want, "step {}", step["name"]);
    }
}
