//! Document indexing benchmark.
//!
//! ```text
//! docmonk-doc-bench FIXTURE_DIR [--copies N] [--kinds a.pdf,b.md] [--out report.json] [--emit-only DIR]
//! ```
//!
//! Builds a corpus of N copies of each Markdown/HTML/TXT/DOCX/PPTX/XLSX
//! fixture, then times cold, warm and single-edit passes through the
//! coordinator with Rust-native conversion and chunking.

use std::path::{Path, PathBuf};
use std::time::Instant;

use docmonk_core::models::SourceType;
use docmonk_core::paths::Home;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_storage::control::{ControlPlane, NewSource};
use docmonk_storage::StorageLayout;
use serde_json::json;

const KINDS: &[&str] = &[
    "handbook.md",
    "status.html",
    "long.txt",
    "document.docx",
    "presentation.pptx",
    "spreadsheet.xlsx",
];

fn emit(fixtures: &Path, root: &Path, copies: usize, kinds: &[String]) {
    for i in 0..copies {
        let dir = root.join(format!("d{:02}", i % 20));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for k in kinds {
            let (stem, ext) = k.rsplit_once('.').expect("ext");
            std::fs::copy(fixtures.join(k), dir.join(format!("{stem}_{i}.{ext}"))).expect("copy");
        }
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
    let fixtures = PathBuf::from(args.get(1).expect("FIXTURE_DIR"));
    let copies: usize = arg("--copies").and_then(|v| v.parse().ok()).unwrap_or(100);
    let kinds: Vec<String> = arg("--kinds")
        .map(|k| k.split(',').map(str::to_owned).collect())
        .unwrap_or_else(|| KINDS.iter().map(|s| (*s).to_owned()).collect());
    if let Some(dir) = arg("--emit-only") {
        emit(&fixtures, Path::new(&dir), copies, &kinds);
        return;
    }
    let tmp = std::env::temp_dir().join(format!("docmonk-doc-bench-{}", std::process::id()));
    let root = tmp.join("source");
    emit(&fixtures, &root, copies, &kinds);
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
        .expect("source");
    let cfg = docmonk_config::DocMonkConfig::default();
    let opts_reg = docmonk_convert::RegistryOptions {
        ocr_models_dir: std::env::var_os("DOCMONK_OCR_MODELS_DIR").map(PathBuf::from),
        cache_dir: None,
        models_root: None,
        graph_facts: None,
    };
    let reg = docmonk_convert::registry_with(&cfg, &opts_reg);
    let opts = Options::from_config(&cfg);
    let mut results = Vec::new();
    let mut run = |name: &str, cp: &mut ControlPlane| {
        let t = Instant::now();
        let r = run_source(&layout, cp, &src, &reg, &opts, &mut NoProgress).expect("run");
        let wall = t.elapsed().as_secs_f64();
        eprintln!(
            "{name:<16} wall={wall:.4}s indexed={} failed={}",
            r.indexed, r.failed
        );
        results.push(json!({"scenario": name, "wall_time_s": wall, "indexed": r.indexed, "failed": r.failed, "timings": r.timings}));
    };
    run("cold_index", &mut cp);
    run("warm_unchanged", &mut cp);
    let first = &kinds[0];
    let (stem, ext) = first.rsplit_once('.').expect("ext");
    let src_edit = fixtures.join(first);
    let mut bytes = std::fs::read(&src_edit).expect("read");
    bytes.extend_from_slice(b"\n");
    std::fs::write(root.join(format!("d00/{stem}_0.{ext}")), bytes).expect("edit");
    run("single_edit", &mut cp);
    let report = json!({
        "files": copies * kinds.len(), "kinds": kinds,
        "workers": opts.workers, "scenarios": results,
    });
    let text = serde_json::to_string_pretty(&report).expect("json");
    match arg("--out") {
        Some(p) => std::fs::write(p, text + "\n").expect("write"),
        None => println!("{text}"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
