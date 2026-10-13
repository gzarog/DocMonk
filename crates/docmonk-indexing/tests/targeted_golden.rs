//! Targeted (watcher-triggered) passes against
//! fixtures/expected/watcher.json: pass counters and the files left in the
//! index after each step.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use docmonk_core::paths::project_id_for_canonical;
use docmonk_indexing::coordinator::{run_source, run_source_with, NoProgress, Options, Registry};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

fn golden() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/expected/watcher.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn strs(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|s| s.as_str().unwrap())
}

#[test]
fn targeted_passes_are_as_expected() {
    let g = golden();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("src_root");
    for (rel, text) in g["tree"].as_object().unwrap() {
        common::write(&root, rel, text.as_str().unwrap());
    }
    let outside = tmp.path().join("outside.py");
    std::fs::write(&outside, "O = 1\n").unwrap();
    let home = common::home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = common::control(&home);
    let src = common::add_source(&mut cp, &root, &[], &[]);
    let reg = Registry::raw();
    let mut opts = Options::from_config(&docmonk_config::DocMonkConfig::default());
    opts.workers = 2;
    run_source(&layout, &mut cp, &src, &reg, &opts, &mut NoProgress).unwrap();

    for step in g["targeted"].as_array().unwrap() {
        let name = step["name"].as_str().unwrap();
        for (rel, text) in step["write"].as_object().into_iter().flatten() {
            common::write(&root, rel, text.as_str().unwrap());
        }
        for rel in strs(&step["delete"]) {
            std::fs::remove_file(root.join(rel)).unwrap();
        }
        for mv in step["move"].as_array().into_iter().flatten() {
            let (a, b) = (mv[0].as_str().unwrap(), mv[1].as_str().unwrap());
            std::fs::rename(root.join(a), root.join(b)).unwrap();
        }
        let targets: BTreeSet<PathBuf> = strs(&step["targets"])
            .map(|t| {
                if t == "@outside" {
                    outside.clone()
                } else {
                    root.join(t)
                }
            })
            .collect();
        let r = run_source_with(
            &layout,
            &mut cp,
            &src,
            &reg,
            &opts,
            Some(&targets),
            &mut NoProgress,
        )
        .unwrap();
        assert!(r.targeted, "{name}");
        let c = &r.counts;
        let want = &step["counts"];
        let got = json!({
            "scanned": c.scanned,
            "new": c.new,
            "changed": c.changed,
            "unchanged": c.unchanged,
            "deleted": c.deleted,
            "moved": c.moved,
        });
        assert_eq!(&got, want, "{name} counts");

        let state = cp.state(&src.id).unwrap();
        let store =
            ProjectStore::open(&layout, &project_id_for_canonical(&src.path), &src.id, 8).unwrap();
        let mut files: Vec<Value> = store
            .files(&state.active_build_id.unwrap())
            .unwrap()
            .into_iter()
            .map(|f| json!([f.rel_path, f.status]))
            .collect();
        files.sort_by_key(|v| v[0].as_str().unwrap().to_owned());
        assert_eq!(Value::Array(files), step["files"], "{name} files");
    }
}

#[test]
fn network_diff_is_as_expected() {
    use docmonk_indexing::watcher::network::{diff, Fingerprint};
    let fp = |v: &Value| -> Fingerprint {
        v.as_object()
            .unwrap()
            .iter()
            .map(|(k, s)| (k.clone(), (s[0].as_i64().unwrap(), s[1].as_f64().unwrap())))
            .collect()
    };
    for c in golden()["network_diff"].as_array().unwrap() {
        let got: Vec<String> = diff(&fp(&c["previous"]), &fp(&c["current"]))
            .into_iter()
            .collect();
        assert_eq!(json!(got), c["changed"], "{c}");
    }
}
