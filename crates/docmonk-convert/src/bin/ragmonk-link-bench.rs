//! Cold-linking benchmark.
//!
//! ```text
//! ragmonk-link-bench [--code N] [--docs M] [--out report.json] [--emit-only DIR]
//! ```
//!
//! Generates N source-code modules and M Markdown documents that mention
//! entities by bare name, qualified name, alias and filename, indexes them
//! with the full processor registry, then
//! times a full relink (every file touched) of the built corpus.

use std::path::Path;
use std::time::Instant;

use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::json;

fn module(i: usize) -> String {
    format!(
        "\"\"\"Generated fixture module {i}.\"\"\"\n\nfrom __future__ import annotations\n\n\nclass Service{i}:\n    \"\"\"A small synthetic service class used only for benchmark load.\"\"\"\n\n    def __init__(self, name: str = \"service_{i}\") -> None:\n        self.name = name\n        self._counter = 0\n\n    def handle(self, payload: dict) -> dict:\n        self._counter += 1\n        return {{\"name\": self.name, \"seq\": self._counter, \"payload\": payload}}\n\n    def reset(self) -> None:\n        self._counter = 0\n\n\ndef process_{i}(items: list[int]) -> int:\n    total = 0\n    for item in items:\n        if item % 2 == 0:\n            total += item\n        else:\n            total -= item\n    return total\n\n\ndef helper_{i}(value: str) -> str:\n    return value.strip().lower()\n"
    )
}

fn doc(d: usize, n_code: usize) -> String {
    let mut s = format!("# Design note {d}\n\n");
    for k in 0..20 {
        let i = (d * 37 + k * 101) % n_code.max(1);
        let pkg = i % (n_code / 200).max(1);
        s.push_str(&format!(
            "## Section {k}\n\nThe Service{i} class calls process_{i} and helper_{i} \
             (see module_{i:06}.py). Its handler is src.pkg_{pkg:04}.module_{i:06}.Service{i}.handle, \
             also written Service{i}.reset in short.\n\n"
        ));
    }
    s
}

fn emit(root: &Path, n_code: usize, n_docs: usize) {
    for i in 0..n_code {
        let dir = root.join(format!("src/pkg_{:04}", i % (n_code / 200).max(1)));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("module_{i:06}.py")), module(i)).expect("write");
    }
    std::fs::create_dir_all(root.join("docs")).expect("mkdir");
    for d in 0..n_docs {
        std::fs::write(root.join(format!("docs/doc_{d:06}.md")), doc(d, n_code)).expect("write");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |n: &str| {
        args.iter()
            .position(|a| a == n)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let n_code: usize = arg("--code").and_then(|v| v.parse().ok()).unwrap_or(2000);
    let n_docs: usize = arg("--docs").and_then(|v| v.parse().ok()).unwrap_or(100);
    if let Some(dir) = arg("--emit-only") {
        emit(Path::new(&dir), n_code, n_docs);
        return;
    }
    let tmp = std::env::temp_dir().join(format!("ragmonk-link-bench-{}", std::process::id()));
    let root = tmp.join("source");
    emit(&root, n_code, n_docs);
    let home = Home::new(tmp.join("home"));
    let layout = StorageLayout::new(&home);
    let mut cp = ControlPlane::open(&layout, 64).expect("control");
    let canonical = docmonk_core::paths::resolve(&root)
        .expect("resolve")
        .to_string_lossy()
        .into_owned();
    let (src, _) = cp
        .add_source(&NewSource {
            canonical_path: canonical.clone(),
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
        })
        .expect("source");
    let cfg = docmonk_config::RagMonkConfig::default();
    let t = Instant::now();
    let r = run_source(
        &layout,
        &mut cp,
        &src,
        &docmonk_convert::registry(&cfg),
        &Options::from_config(&cfg),
        &mut NoProgress,
    )
    .expect("index");
    let cold_index = t.elapsed().as_secs_f64();
    let build = r.build_id.expect("build");
    let store = ProjectStore::open(&layout, &project_id_for_canonical(&canonical), &src.id, 64)
        .expect("store");
    let touched: Vec<String> = store
        .files(&build)
        .expect("files")
        .into_iter()
        .map(|f| f.id)
        .collect();
    let mut best = f64::MAX;
    let mut links = 0;
    for _ in 0..3 {
        let t = Instant::now();
        links = docmonk_knowledge::linker::link_touched_files(&store, &build, &touched)
            .expect("link")
            .len();
        best = best.min(t.elapsed().as_secs_f64());
    }
    let entities = store.count("entities", &build).expect("count");
    let units = store.count("chunks", &build).expect("count");
    eprintln!("cold_index={cold_index:.3}s full_relink={best:.4}s links={links} entities={entities} units={units}");
    let report = json!({
        "code_files": n_code, "documents": n_docs,
        "entities": entities, "units": units, "links": links,
        "cold_index_wall_s": cold_index, "full_relink_wall_s": best,
    });
    let text = serde_json::to_string_pretty(&report).expect("json");
    match arg("--out") {
        Some(p) => std::fs::write(p, text + "\n").expect("write"),
        None => println!("{text}"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
