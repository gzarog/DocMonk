//! Indexing benchmark harness.
//!
//! ```text
//! docmonk-index-bench [--files N] [--out path.json]
//! ```
//!
//! Generates a synthetic corpus (N source-code modules plus N/20 text
//! documents), then measures in-process wall time for cold,
//! warm-unchanged, single-edit, 1%-change, burst, rename and delete passes
//! through the indexing coordinator. Processing uses the raw registry, so
//! only scan, diff, hashing, storage and publication costs are measured.

use std::path::Path;
use std::time::Instant;

use docmonk_core::models::SourceType;
use docmonk_core::paths::Home;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options, Registry, SourceResult};
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::StorageLayout;
use serde_json::json;

fn module(i: usize) -> String {
    format!(
        "\"\"\"Generated fixture module {i}.\"\"\"\nclass Service{i}:\n    def handle(self, payload):\n        return payload\n\ndef process_{i}(items):\n    return sum(items)\n"
    )
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, content).expect("write");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let n: usize = arg("--files").and_then(|v| v.parse().ok()).unwrap_or(50);
    let out = arg("--out");

    let tmp = std::env::temp_dir().join(format!("docmonk-index-bench-{}", std::process::id()));
    let root = tmp.join("source");
    for i in 0..n {
        write(&root, &format!("pkg{}/module_{i}.py", i % 10), &module(i));
    }
    for i in 0..(n / 20).max(5) {
        write(
            &root,
            &format!("docs/doc_{i}.md"),
            &format!("# Doc {i}\n\nSome text.\n"),
        );
    }
    let home = Home::new(tmp.join("home"));
    let layout = StorageLayout::new(&home);
    let mut cp = ControlPlane::open(&layout, 64).expect("control plane");
    let canonical = docmonk_core::paths::resolve(&root)
        .expect("resolve")
        .to_string_lossy()
        .into_owned();
    let (src, _) = cp
        .add_source(&NewSource {
            canonical_path: canonical,
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
        })
        .expect("add source");
    let reg = Registry::raw();
    let opts = Options::from_config(&docmonk_config::DocMonkConfig::default());
    let mut results = Vec::new();
    let mut run = |name: &str, cp: &mut ControlPlane| {
        let started = Instant::now();
        let r: SourceResult =
            run_source(&layout, cp, &src, &reg, &opts, &mut NoProgress).expect("run");
        let wall = started.elapsed().as_secs_f64();
        eprintln!(
            "{name:<20} wall={wall:>8.4}s scanned={} new={} changed={} unchanged={} moved={} deleted={} hash_calls={}",
            r.counts.scanned, r.counts.new, r.counts.changed, r.counts.unchanged, r.counts.moved, r.counts.deleted, r.timings.hash_calls
        );
        results.push(json!({ "scenario": name, "wall_time_s": wall, "counts": r.counts, "timings": r.timings }));
    };
    run("cold_index", &mut cp);
    run("warm_unchanged", &mut cp);
    write(
        &root,
        "pkg0/module_0.py",
        &format!("{}# edited\n", module(0)),
    );
    run("single_edit", &mut cp);
    for i in 0..(n / 100).max(1) {
        write(
            &root,
            &format!("pkg{}/module_{i}.py", i % 10),
            &format!("{}# one percent\n", module(i)),
        );
    }
    run("one_percent_change", &mut cp);
    for i in 0..(n / 20).max(2) {
        write(
            &root,
            &format!("pkg{}/module_{i}.py", i % 10),
            &format!("{}# burst\n", module(i)),
        );
    }
    run("burst_change", &mut cp);
    std::fs::rename(
        root.join("pkg1/module_1.py"),
        root.join("pkg1/module_1_renamed.py"),
    )
    .expect("rename");
    run("rename", &mut cp);
    std::fs::remove_file(root.join("pkg2/module_2.py")).expect("delete");
    run("delete", &mut cp);

    let report = json!({
        "plan_id": "docmonk-1.0-clean-slate",

        "files": n,
        "processing": "raw (scan/diff/hash/storage/publication only)",
        "scenarios": results,
    });
    let text = serde_json::to_string_pretty(&report).expect("json");
    match out {
        Some(p) => std::fs::write(p, text + "\n").expect("write report"),
        None => println!("{text}"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
