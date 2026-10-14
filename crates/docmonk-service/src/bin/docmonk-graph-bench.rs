//! Relationship-graph benchmark.
//!
//! ```text
//! docmonk-graph-bench [--files N] [--docs M] [--runs R] [--warmup W]
//!                     [--workers K] [--scenarios a,b,...] [--server all|none]
//!                     [--label NAME] [--out report.json] [--emit-only DIR]
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
//! `--scenarios` measures only the named scenarios (the others still apply
//! their mutation, index and build the graph, unmeasured and without a
//! server publication); `--server none` skips server publication.
//!
//! The server side runs against an in-process engine that implements only
//! the requests graph publication sends: it yields request, record and
//! copy-forward **counts**, never cluster timings. The report records the
//! graph fingerprint per scenario, so runs are compared for equivalence as
//! well as speed. CPU time and peak RSS are read from `/proc` (Linux only;
//! `null` elsewhere).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::graph::{GraphManifest, GraphPublishInput, GraphPublishReport};
use docmonk_backends::transport::{Counting, Method, Response, Transport};
use docmonk_backends::ServerBackend;
use docmonk_code::graph_stage::{GraphOutcome, GraphReport};
use docmonk_core::models::SourceType;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::coordinator::{run_source, NoProgress, Options, SourceResult};
use docmonk_storage::control::{ControlPlane, NewSource, SourceRecord};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

// ------------------------------------------------------------- corpus ---

fn controller(i: usize, n: usize) -> String {
    let dep = (i + 1) % n;
    format!(
        r#"using System;
using Microsoft.AspNetCore.Mvc;
using Shop.Services;

namespace Shop.Controllers.Area{area}
{{
    [ApiController]
    public class Orders{i}Controller : ControllerBase
    {{
        private readonly IOrderService{i} _service;

        public Orders{i}Controller(IOrderService{i} service)
        {{
            _service = service;
        }}

        [HttpGet("api/orders{i}/{{id}}")]
        public IActionResult Get(int id)
        {{
            var order = _service.Find(id);
            return Ok(Mapper{dep}.ToDto(order));
        }}

        [HttpPost("api/orders{i}")]
        public IActionResult Create(Order{i} order)
        {{
            Guard.NotNull(order);
            _service.Save(order);
            return Created("", order);
        }}
    }}
}}
"#,
        area = i % 25
    )
}

fn service(i: usize, find: &str, body: &str) -> String {
    format!(
        r#"using System.Collections.Generic;
using Shop.Models;

namespace Shop.Services
{{
    public interface IOrderService{i}
    {{
        Order{i} {find}(int id);
        void Save(Order{i} order);
    }}

    public class OrderService{i} : IOrderService{i}
    {{
        private readonly List<Order{i}> _items = new List<Order{i}>();

        public Order{i} {find}(int id) {{ {body} }}
        public void Save(Order{i} order) {{ _items.Add(order); Audit.Log("save"); }}
    }}

    public static class Mapper{i}
    {{
        public static object ToDto(Order{i} order) {{ return order; }}
    }}
}}
"#
    )
}

const FIND_BODY: &str = "return _items.Find(o => o.Id == id);";

fn model(i: usize) -> String {
    format!(
        r#"namespace Shop.Models
{{
    public class Order{i} : EntityBase
    {{
        public int Id {{ get; set; }}
        public decimal Total;
    }}
}}
"#
    )
}

fn note(d: usize, groups: usize, extra: &str) -> String {
    let mut s = format!("# Design note {d}\n\n{extra}");
    for k in 0..12 {
        let i = (d * 37 + k * 101) % groups.max(1);
        s.push_str(&format!(
            "## Orders {i}\n\nThe Orders{i}Controller delegates to OrderService{i} \
             (see OrderService{i}.cs); Shop.Services.OrderService{i}.Save audits every write, \
             and OrderService{i}.Find serves GET api/orders{i}/{{id}}.\n\n"
        ));
    }
    s
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, content).expect("write");
}

/// `files` code files in groups of three plus one shared file, and `docs`
/// notes.
fn emit(root: &Path, files: usize, docs: usize) -> usize {
    let groups = (files.saturating_sub(1) / 3).max(2);
    for i in 0..groups {
        write(
            root,
            &format!("src/Controllers/Area{}/Orders{i}Controller.cs", i % 25),
            &controller(i, groups),
        );
        write(
            root,
            &format!("src/Services/OrderService{i}.cs"),
            &service(i, "Find", FIND_BODY),
        );
        write(root, &format!("src/Models/Order{i}.cs"), &model(i));
    }
    write(
        root,
        "src/Common/Common.cs",
        "namespace Shop.Common\n{\n    public abstract class EntityBase { }\n    public static class Guard { public static void NotNull(object o) { } }\n    public static class Audit { public static void Log(string m) { } }\n}\n",
    );
    for d in 0..docs {
        write(root, &format!("docs/note_{d:05}.md"), &note(d, groups, ""));
    }
    groups
}

// ------------------------------------------------------ server engine ---

/// The graph-publication subset of a search engine, in memory.
#[derive(Default)]
struct Engine0 {
    state: Value,
    seq: i64,
    /// `(generations, file_id)` of every graph record.
    records: Vec<(Vec<String>, String)>,
}

#[derive(Clone, Default)]
struct Memory(Arc<Mutex<Engine0>>);

fn reply(v: Value) -> docmonk_backends::error::Result<Response> {
    Ok(Response {
        status: 200,
        body: serde_json::to_vec(&v).expect("json"),
    })
}

impl Transport for Memory {
    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(&[u8], &str)>,
    ) -> docmonk_backends::error::Result<Response> {
        let mut e = self.0.lock().expect("engine");
        let base = path.split('?').next().unwrap_or(path);
        let doc = || -> Value {
            body.and_then(|(b, _)| serde_json::from_slice(b).ok())
                .unwrap_or(Value::Null)
        };
        if base.starts_with("/bench-source-state/_doc/") {
            if method == Method::Get {
                return reply(json!({"_source": e.state, "_seq_no": e.seq, "_primary_term": 1}));
            }
            if !path.contains(&format!("if_seq_no={}", e.seq)) {
                return Ok(Response {
                    status: 409,
                    body: b"conflict".to_vec(),
                });
            }
            e.state = doc();
            e.seq += 1;
            return reply(json!({"result": "updated"}));
        }
        if base == "/_bulk" {
            let text = body
                .map(|(b, _)| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            for line in text.lines().filter(|l| !l.is_empty()) {
                let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
                if let Some(b) = v["build_id"].as_str() {
                    let f = v["file_id"].as_str().unwrap_or_default().to_owned();
                    e.records.push((vec![b.to_owned()], f));
                }
            }
            return reply(json!({"errors": false, "items": []}));
        }
        if base.ends_with("/_refresh") {
            return reply(json!({}));
        }
        if base.ends_with("/_update_by_query") {
            let v = doc();
            let to = v["script"]["params"]["b"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let filter = v["query"]["bool"]["filter"].clone();
            let from = filter[1]["term"]["build_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if let Some(keep) = v["script"]["params"]["keep"].as_array() {
                let keep: Vec<String> = keep
                    .iter()
                    .filter_map(|k| k.as_str().map(str::to_owned))
                    .collect();
                let files: std::collections::HashSet<&str> = filter[2]["terms"]["file_id"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                let mut updated = 0;
                for (builds, f) in &mut e.records {
                    if builds.contains(&from) && files.contains(f.as_str()) {
                        builds.retain(|x| keep.contains(x));
                        if !builds.contains(&to) {
                            builds.push(to.clone());
                        }
                        updated += 1;
                    }
                }
                return reply(json!({"updated": updated, "failures": []}));
            }
            let before = e.records.len();
            for (builds, _) in &mut e.records {
                builds.retain(|x| *x != to);
            }
            e.records.retain(|(builds, _)| !builds.is_empty());
            return reply(json!({"deleted": before - e.records.len(), "failures": []}));
        }
        Ok(Response {
            status: 400,
            body: format!("unsupported request {method:?} {base}").into_bytes(),
        })
    }
}

// ----------------------------------------------------------- resources ---

/// User+system CPU seconds of this process (`/proc/self/stat`, 100 Hz).
fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

/// Peak resident set since the last [`reset_peak_rss`], in MB.
fn peak_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024.0)
}

fn reset_peak_rss() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

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
}

impl Bench {
    fn new(files: usize, docs: usize, workers: usize, run: usize, sel: &Selection) -> Self {
        let tmp =
            std::env::temp_dir().join(format!("docmonk-graph-bench-{}-{run}", std::process::id()));
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
        let cfg = docmonk_config::DocMonkConfig::default();
        let mut opts = Options::from_config(&cfg);
        opts.workers = workers;
        let memory = Memory::default();
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
            registry: docmonk_convert::registry(&cfg),
            server,
            memory,
            groups,
            selected: sel.scenarios.clone(),
            with_server: sel.server,
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
            let mut e = self.memory.0.lock().expect("engine");
            if e.state.is_null() {
                e.state = json!({"source_id": self.src.id, "state": "ready", "versions": {}});
            }
            e.state["active_build_id"] = json!(build);
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
        v["action"] = json!(if r.published { "publish" } else { "unchanged" });
        v["http_requests"] = json!(self.server.stats().requests - before);
        v["records_live"] = json!(self.memory.0.lock().expect("engine").records.len());
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
        let graph = docmonk_knowledge::build_graph_phased(
            &mut store,
            &self.root,
            &build,
            self.workers,
            None,
            &mut |_, _| {},
            Some(&phase),
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
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
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
        let results = Bench::new(files, docs, workers, run, &selection).run();
        if run >= warmup {
            measured.push(results);
        }
    }
    let report = json!({
        "benchmark": "relationships",
        "label": arg("--label"),
        "measurement": "measured (SQLite, synthetic); server figures are request counts against an in-process engine",
        "metadata": {
            "git_sha": option_env!("DOCMONK_GIT_SHA"),
            "host_os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cpus": cpus,
            "worker_count": workers,
            "graph_batch": docmonk_code::graph_stage::GRAPH_BATCH,
            "code_files": files,
            "documents": docs,
            "runs": runs,
            "warmup": warmup,
            "scenarios": selection.scenarios,
            "server": selection.server,
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
