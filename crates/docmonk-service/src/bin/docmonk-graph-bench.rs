//! Relationship-graph benchmark.
//!
//! ```text
//! docmonk-graph-bench [--files N] [--docs M] [--runs R] [--warmup W]
//!                     [--workers K] [--scenarios a,b,...] [--server all|none]
//!                     [--fact-cache] [--graph-batch N] [--work-dir DIR] [--label NAME] [--out report.json]
//!                     [--emit-only DIR]
//! ```
//!
//! Generates a synthetic ASP.NET-style C# repository (controllers with
//! routes, services, models, cross-file calls) plus Markdown design notes
//! that mention its entities by bare name, qualified name, alias, filename
//! and route. No real source is used. Each run indexes it in a fresh home
//! and walks the scenarios below, timing the index pass, the local graph
//! stage and a server publication of the same staged graph:
//!
//! | Scenario | Mutation | Expected plan |
//! |---|---|---|
//! | `cold` | first index | full |
//! | `no_change` | none (graph stage only) | skip |
//! | `equivalent_republish` | full rebuild, identical content | rebind |
//! | `body_edit` | one method body | incremental, 1 file |
//! | `rename_definition` | one referenced method renamed | incremental + dependents |
//! | `add_file` | one new service | incremental |
//! | `remove_file` | one defining model deleted | incremental + dependents |
//! | `doc_edit` | one note rewritten | incremental, relinked |
//!
//! Ids derive from the corpus path, so every run uses the same work
//! directory (default `<temp>/docmonk-graph-bench`, emptied first): the
//! fingerprints of two runs, or of two builds of this benchmark, are then
//! directly comparable. Do not run two instances on one work directory.
//!
//! `--scenarios` measures only the named scenarios (the others still apply
//! their mutation, index and build the graph, unmeasured and without a
//! server publication); `--server none` skips server publication.
//!
//! The server side runs against the in-process engine of
//! `docmonk_backends::memory` (the requests graph publication sends,
//! evaluated like an engine would): it yields request, written and closed
//! record **counts**, never cluster timings. The report records the
//! graph fingerprint per scenario, so runs are compared for equivalence as
//! well as speed. CPU time and peak RSS are read from `/proc` (Linux only;
//! `null` elsewhere).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::graph::{GraphManifest, GraphPublishInput, GraphPublishReport};
use docmonk_backends::memory::Memory;
use docmonk_backends::transport::Counting;
use docmonk_backends::ServerBackend;
use docmonk_code::graph_stage::{GraphOutcome, GraphReport};
use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options, SourceResult};
use docmonk_storage::control::{ControlPlane, NewSource, SourceRecord};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};
use sha2::Digest;

use docmonk_service::bench::corpus::{emit, note, service, write, FIND_BODY};

use docmonk_service::bench::{cpu_seconds, peak_rss_mb, percentile, reset_peak_rss};

// --------------------------------------------------------------- runs ---

struct Bench {
    tmp: PathBuf,
    root: PathBuf,
    layout: StorageLayout,
    cp: ControlPlane,
    src: SourceRecord,
    canonical: String,
    opts: Options,
    workers: usize,
    registry: docmonk_indexing::coordinator::Registry,
    server: ServerBackend,
    memory: Memory,
    groups: usize,
    selected: Option<std::collections::BTreeSet<String>>,
    with_server: bool,
    facts: Option<Arc<docmonk_storage::facts::FactCache>>,
    batch: usize,
}

impl Bench {
    fn new(files: usize, docs: usize, workers: usize, sel: &Selection) -> Self {
        let tmp = sel.work_dir.clone();
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("source");
        let groups = emit(&root, files, docs);
        let home = Home::new(tmp.join("home"));
        let layout = StorageLayout::new(&home);
        let mut cp = ControlPlane::open(&layout, 64).expect("control plane");
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
            .expect("add source");
        let mut cfg = docmonk_config::DocMonkConfig::default();
        cfg.indexing.graph_fact_cache = sel.fact_cache;
        let facts = docmonk_convert::open_graph_facts(&cfg, Some(&tmp.join("graph-facts.db")));
        let registry = docmonk_convert::registry_with(
            &cfg,
            &docmonk_convert::RegistryOptions {
                graph_facts: Some(tmp.join("graph-facts.db")),
                ..Default::default()
            },
        );
        let mut opts = Options::from_config(&cfg);
        opts.workers = workers;
        let memory = Memory::new();
        let server = ServerBackend::with_client(
            Counting::new(Box::new(memory.clone())),
            Engine::OpenSearch,
            "bench",
            None,
            BulkLimits::from_config(&cfg.storage.server.bulk),
        );
        Self {
            tmp,
            root,
            layout,
            cp,
            src,
            canonical,
            opts,
            workers,
            registry,
            server,
            memory,
            groups,
            selected: sel.scenarios.clone(),
            with_server: sel.server,
            facts,
            batch: sel.batch,
        }
    }

    fn store(&self) -> ProjectStore {
        ProjectStore::open(
            &self.layout,
            &project_id_for_canonical(&self.canonical),
            &self.src.id,
            64,
        )
        .expect("store")
    }

    fn index(&mut self) -> SourceResult {
        run_source(
            &self.layout,
            &mut self.cp,
            &self.src,
            &self.registry,
            &self.opts,
            &mut NoProgress,
        )
        .expect("index")
    }

    fn active(&self) -> String {
        self.cp
            .state(&self.src.id)
            .expect("state")
            .active_build_id
            .expect("published")
    }

    /// The server publication `docmonk index` would make of the staged
    /// graph (server mode stages into exactly this kind of store).
    fn publish(&self, store: &ProjectStore, build: &str, graph: &GraphReport) -> Value {
        {
            let mut st = self
                .memory
                .docs("bench-source-state")
                .into_iter()
                .find(|d| d["source_id"] == self.src.id.as_str())
                .unwrap_or_else(
                    || json!({"source_id": self.src.id, "state": "ready", "versions": {}}),
                );
            if let Some(o) = st.as_object_mut() {
                o.remove("_id");
            }
            st["active_build_id"] = json!(build);
            self.memory.put_doc("bench-source-state", &self.src.id, st);
        }
        let started = Instant::now();
        let before = self.server.stats().requests;
        let staged = store.graph_state_with_files().expect("graph state");
        let current = self
            .server
            .graph(&self.src.id)
            .expect("graph")
            .map(|(g, _)| g)
            .unwrap_or_default();
        if graph.outcome == GraphOutcome::UpToDate
            && current.visible_generation(Some(build)).is_some()
            && current.input_digest == staged.input_digest
        {
            return json!({"action": "skip", "http_requests": 0, "seconds": 0.0});
        }
        if let (Some(generation), Some(digest)) = (
            current.generation.as_deref(),
            staged.input_digest.as_deref(),
        ) {
            if current.input_digest.as_deref() == Some(digest)
                && current.base_build_id.as_deref() != Some(build)
                && self
                    .server
                    .rebind_graph(&self.src.id, None, generation, build, digest)
                    .expect("rebind")
            {
                return json!({
                    "action": "rebind",
                    "http_requests": self.server.stats().requests - before,
                    "seconds": started.elapsed().as_secs_f64(),
                });
            }
        }
        let derivation = docmonk_code::process::graph_derivation();
        let generation = format!("g-{}", staged.generation);
        let r: GraphPublishReport = self
            .server
            .publish_graph(&GraphPublishInput {
                source_id: &self.src.id,
                store,
                local_build: build,
                base_build_id: build,
                lease: None,
                generation: &generation,
                input_digest: staged.input_digest.as_deref(),
                manifest: Some(GraphManifest {
                    derivation: &derivation,
                    version: u64::from(docmonk_storage::graph::MANIFEST_VERSION),
                    file_keys: &staged.files,
                }),
                phase: None,
            })
            .expect("publish");
        let mut v = serde_json::to_value(&r).expect("json");
        // Counted before the benchmark's own state read below.
        v["http_requests"] = json!(self.server.stats().requests - before);
        // The promoted generation's per-file record digests (the overall
        // digest also covers the base build id, which differs per run):
        // equal hashes mean the server holds byte-identical graph records.
        let promoted = self
            .server
            .graph(&self.src.id)
            .expect("graph")
            .map(|(g, _)| g)
            .unwrap_or_default();
        let mut h = sha2::Sha256::new();
        for (file, digest) in &promoted.file_digests {
            h.update(file.as_bytes());
            h.update([0]);
            h.update(digest.as_bytes());
            h.update([b'\n']);
        }
        let hash: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        v["graph_digest"] = json!(hash);
        v["file_digests"] = json!(promoted.file_digests.len());
        v["action"] = json!(match (r.published, r.rebound) {
            (true, true) => "repoint",
            (true, false) => "publish",
            _ => "unchanged",
        });
        v["records_live"] = json!(self.memory.docs("bench-relationships").len());
        v
    }

    /// One scenario: optional index pass, the graph stage, the server
    /// publication; resources measured around the whole. An unselected
    /// scenario only advances the source (`None`).
    fn scenario(&mut self, name: &str, reindex: bool) -> Option<Value> {
        if self.selected.as_ref().is_some_and(|s| !s.contains(name)) {
            if reindex {
                self.index();
            }
            let build = self.active();
            let mut store = self.store();
            docmonk_knowledge::build_graph(
                &mut store,
                &self.root,
                &build,
                self.workers,
                None,
                &mut |_, _| {},
            )
            .expect("graph");
            return None;
        }
        reset_peak_rss();
        let cpu0 = cpu_seconds();
        let started = Instant::now();
        let mut index_seconds = 0.0;
        let mut index = Value::Null;
        if reindex {
            let t = Instant::now();
            let r = self.index();
            index_seconds = t.elapsed().as_secs_f64();
            index = json!({
                "indexed": r.indexed, "failed": r.failed, "counts": r.counts,
                "timings": r.timings,
            });
        }
        let build = self.active();
        let mut store = self.store();
        let t = Instant::now();
        let timeline = std::cell::RefCell::new(Vec::<(&'static str, Instant)>::new());
        let phase = |p: &'static str| timeline.borrow_mut().push((p, Instant::now()));
        let graph = docmonk_code::graph_stage::build_graph(
            &mut store,
            &self.root,
            &build,
            &docmonk_code::graph_stage::GraphOptions {
                workers: self.workers,
                linker: Some(&docmonk_knowledge::KnowledgeLinker),
                cancel: None,
                phase: Some(&phase),
                facts: self.facts.as_deref(),
                batch: self.batch,
            },
            &mut |_, _| {},
        )
        .expect("graph");
        let end = Instant::now();
        let timeline = timeline.into_inner();
        let mut phases: BTreeMap<&'static str, f64> = BTreeMap::new();
        for (k, (name, at)) in timeline.iter().enumerate() {
            let until = timeline.get(k + 1).map_or(end, |n| n.1);
            *phases.entry(name).or_default() += until.duration_since(*at).as_secs_f64();
        }
        let graph_seconds = t.elapsed().as_secs_f64();
        let server = if self.with_server {
            self.publish(&store, &build, &graph)
        } else {
            Value::Null
        };
        let wall = started.elapsed().as_secs_f64();
        let fp = docmonk_knowledge::canonical::fingerprint(&store, &build).expect("fingerprint");
        let entities = store.count("entities", &build).expect("count");
        let files = store.count("files", &build).expect("count");
        eprintln!(
            "{name:<22} {:<11} index={index_seconds:>7.3}s graph={graph_seconds:>7.3}s server={:>7.3}s files={files} entities={entities} edges={} links={}",
            graph.action.as_str(),
            server["seconds"].as_f64().unwrap_or(0.0),
            fp.edges,
            fp.links
        );
        Some(json!({
            "scenario": name,
            "plan_action": graph.action.as_str(),
            "plan_reason": graph.reason,
            "files_total": files,
            "entities_total": entities,
            "index_seconds": index_seconds,
            "graph_seconds": graph_seconds,
            "full_pipeline_seconds": wall,
            "cpu_seconds": cpu0.zip(cpu_seconds()).map(|(a, b)| b - a),
            "peak_rss_mb": peak_rss_mb(),
            "phase_seconds": phases,
            "graph": graph,
            "server": server,
            "index": index,
            "fingerprint": fp,
        }))
    }

    fn run(&mut self) -> Vec<Value> {
        let mut out = vec![self.scenario("cold", true)];
        out.push(self.scenario("no_change", false));
        self.cp
            .require_full_rebuild(&self.src.id, "benchmark")
            .expect("full rebuild");
        out.push(self.scenario("equivalent_republish", true));
        write(
            &self.root,
            "src/Services/OrderService0.cs",
            &service(
                0,
                "Find",
                "var hit = _items.Find(o => o.Id == id); return hit;",
            ),
        );
        out.push(self.scenario("body_edit", true));
        write(
            &self.root,
            "src/Services/OrderService1.cs",
            &service(1, "FindById", FIND_BODY),
        );
        out.push(self.scenario("rename_definition", true));
        write(
            &self.root,
            "src/Services/Extra/ReportService.cs",
            "namespace Shop.Services.Extra\n{\n    public class ReportService\n    {\n        public void Run() { Audit.Log(\"report\"); Mapper0.ToDto(null); }\n    }\n}\n",
        );
        out.push(self.scenario("add_file", true));
        std::fs::remove_file(self.root.join("src/Models/Order2.cs")).expect("remove");
        out.push(self.scenario("remove_file", true));
        write(
            &self.root,
            "docs/note_00000.md",
            &note(
                0,
                self.groups,
                "Orders3Controller and Shop.Services.ReportService.Run are new here.\n\n",
            ),
        );
        out.push(self.scenario("doc_edit", true));
        out.into_iter().flatten().collect()
    }
}

/// Which scenarios to measure, and whether to publish to the server.
struct Selection {
    scenarios: Option<std::collections::BTreeSet<String>>,
    server: bool,
    fact_cache: bool,
    batch: usize,
    work_dir: PathBuf,
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn summarize(runs: &[Vec<Value>]) -> Value {
    let mut out = serde_json::Map::new();
    let Some(first) = runs.first() else {
        return Value::Object(out);
    };
    for (i, s) in first.iter().enumerate() {
        let name = s["scenario"].as_str().unwrap_or_default().to_owned();
        let mut row = serde_json::Map::new();
        for key in [
            "index_seconds",
            "graph_seconds",
            "full_pipeline_seconds",
            "cpu_seconds",
            "peak_rss_mb",
        ] {
            let mut v: Vec<f64> = runs.iter().filter_map(|r| r[i][key].as_f64()).collect();
            v.sort_by(f64::total_cmp);
            row.insert(
                key.into(),
                json!({"median": percentile(&v, 0.5), "p95": percentile(&v, 0.95)}),
            );
        }
        let digests: std::collections::BTreeSet<&str> = runs
            .iter()
            .filter_map(|r| r[i]["fingerprint"]["digest"].as_str())
            .collect();
        row.insert("deterministic".into(), json!(digests.len() == 1));
        for key in ["plan_action", "fingerprint"] {
            row.insert(key.into(), s[key].clone());
        }
        let staged: std::collections::BTreeSet<String> = runs
            .iter()
            .map(|r| r[i]["server"]["graph_digest"].to_string())
            .collect();
        row.insert("server_deterministic".into(), json!(staged.len() == 1));
        row.insert(
            "server_graph_digest".into(),
            s["server"]["graph_digest"].clone(),
        );
        row.insert(
            "sql_statements".into(),
            s["graph"]["counters"]["sql_statements"].clone(),
        );
        row.insert(
            "server_http_requests".into(),
            s["server"]["http_requests"].clone(),
        );
        out.insert(name, Value::Object(row));
    }
    Value::Object(out)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let num =
        |name: &str, default: usize| arg(name).and_then(|v| v.parse().ok()).unwrap_or(default);
    let files = num("--files", 8100);
    let docs = num("--docs", 105);
    if let Some(dir) = arg("--emit-only") {
        emit(Path::new(&dir), files, docs);
        return;
    }
    let runs = num("--runs", 1).max(1);
    let warmup = num("--warmup", 0);
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = num("--workers", cpus).max(1);
    let selection = Selection {
        scenarios: arg("--scenarios").map(|s| s.split(',').map(str::to_owned).collect()),
        server: arg("--server").as_deref() != Some("none"),
        fact_cache: args.iter().any(|a| a == "--fact-cache"),
        batch: num("--graph-batch", 0),
        work_dir: arg("--work-dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("docmonk-graph-bench")),
    };
    let mut measured = Vec::new();
    for run in 0..warmup + runs {
        eprintln!(
            "-- {} {} of {}",
            if run < warmup { "warmup" } else { "run" },
            if run < warmup {
                run + 1
            } else {
                run - warmup + 1
            },
            if run < warmup { warmup } else { runs }
        );
        let results = Bench::new(files, docs, workers, &selection).run();
        if run >= warmup {
            measured.push(results);
        }
    }
    let report = json!({
        "benchmark": "relationships",
        "label": arg("--label"),
        "measurement": "measured (SQLite, synthetic); server figures are request counts against an in-process engine",
        "metadata": {
            "git_sha": std::env::var("DOCMONK_GIT_SHA")
                .ok()
                .or(option_env!("DOCMONK_GIT_SHA").map(str::to_owned)),
            "host_os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cpus": cpus,
            "worker_count": workers,
            "graph_batch": if selection.batch == 0 {
                docmonk_code::graph_stage::GRAPH_BATCH
            } else {
                selection.batch
            },
            "code_files": files,
            "documents": docs,
            "runs": runs,
            "warmup": warmup,
            "scenarios": selection.scenarios,
            "server": selection.server,
            "fact_cache": selection.fact_cache,
        },
        "summary": summarize(&measured),
        "runs": measured,
    });
    let text = serde_json::to_string_pretty(&report).expect("json");
    match arg("--out") {
        Some(p) => std::fs::write(p, text + "\n").expect("write report"),
        None => println!("{text}"),
    }
}
