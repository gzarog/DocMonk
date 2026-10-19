//! Query-latency benchmark.
//!
//! ```text
//! docmonk-search-bench [--files N] [--docs M] [--corpus DIR]... [--work-dir DIR] [--reuse]
//!                      [--modes lexical,hybrid,hybrid_rerank,evidence,explore,impact]
//!                      [--conditions warm,cold] [--runs R] [--cold-queries K]
//!                      [--queries FILE] [--server] [--planner] [--label NAME]
//!                      [--out report.json]
//! ```
//!
//! Indexes a corpus once (the synthetic C# + Markdown corpus of
//! `docmonk-graph-bench` by default, or the given `--corpus` directories,
//! one source each) and runs a fixed query set in every mode:
//!
//! | Mode | Path |
//! |---|---|
//! | `lexical` | `search` (lexical stages and context only) |
//! | `hybrid` | `search --hybrid`, semantic always on |
//! | `hybrid_rerank` | as `hybrid`, plus the cross-encoder |
//! | `evidence` | `evidence` (routed, filtered, grounded) |
//! | `explore` | `explore` |
//! | `impact` | `impact` (symbol queries only) |
//!
//! Two conditions: `warm` runs every query `--runs` times in this process
//! after one warm-up pass (a long-lived MCP, UI or daemon process); `cold`
//! re-executes this binary once per query (a one-shot CLI invocation) and
//! times the whole process. Each reports p50/p95 per stage: `open_sources`,
//! the lexical stages, the semantic sub-stages (`semantic.model_load`,
//! `semantic.query_embed`, `semantic.ann_load`, `semantic.ann_freshness`,
//! `semantic.ann_search`, `semantic.meta_join`, `semantic.attachments`),
//! `neural_rerank`, `context` and `total`.
//!
//! `explore` is broken into its stages (`explore.exact_occurrences`,
//! `explore.lexical`, `explore.symbols`, `explore.graph.*`,
//! `explore.semantic`, `explore.plan`, `explore.graph_freshness`,
//! `explore.absolutize`) from its `--explain` diagnostics, not timed as
//! one opaque block.
//!
//! `--planner` times the lexical planner alone over the retrieval plan's
//! long questions (`planner.*`, p50/p95/p99 over `--runs` x 100 calls).
//!
//! `--server` also runs the lexical plan and caller traversal against an
//! in-process engine that answers every read with no hits. It yields
//! request **counts** per query and source (precise tiers are prefetched;
//! loose tiers run on demand, so counts are an upper bound), never
//! cluster timings.
//!
//! Without the pinned models (`DOCMONK_MODELS_DIR`) the semantic and
//! rerank stages report the model as unavailable and measure nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::Engine;
use docmonk_backends::reader::ServerReader;
use docmonk_backends::transport::{Counting, Method, Response, Transport};
use docmonk_backends::ServerBackend;
use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::graph::{self, Direction};
use docmonk_retrieval::route::SearchFilters;
use docmonk_retrieval::{lexical, Corpus};
use docmonk_service::bench::{corpus, percentile};
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use docmonk_service::query::{self, open_sources, SearchRequest};
use serde_json::{json, Value};

const MODES: &[&str] = &[
    "lexical",
    "hybrid",
    "hybrid_rerank",
    "evidence",
    "explore",
    "impact",
];

/// `(text, symbol)`: `symbol` queries also run in `impact`.
fn default_queries(groups: usize) -> Vec<(String, bool)> {
    let g = |k: usize| k % groups.max(1);
    vec![
        (format!("OrderService{}", g(7)), true),
        (format!("Orders{}Controller", g(3)), true),
        (format!("Shop.Services.OrderService{}.Save", g(11)), true),
        (format!("Mapper{}", g(5)), true),
        ("Audit".into(), true),
        (format!("OrderService{}.Find", g(2)), true),
        ("order service".into(), false),
        ("audits every write".into(), false),
        ("which controller serves the orders route".into(), false),
        ("how are orders saved".into(), false),
        (format!("api/orders{}", g(9)), false),
        (format!("Orders{}Controller.cs", g(4)), false),
        ("design note".into(), false),
        ("entity base class".into(), false),
        (
            format!(
                "Is the URL path /api/orders{} used or referenced? Trace the callers.",
                g(9)
            ),
            false,
        ),
    ]
}

/// Search modes reuse the service functions the CLI, MCP server and UI
/// call; each returns `(stage, milliseconds)` pairs (`total` included).
fn run_mode(
    home: &Home,
    cfg: &DocMonkConfig,
    mode: &str,
    query: &str,
) -> Result<Vec<(String, f64)>, String> {
    let started = Instant::now();
    let mut out = Vec::new();
    let t = Instant::now();
    let opened = open_sources(home, None).map_err(|e| e.to_string())?;
    out.push(("open_sources".to_owned(), ms(t)));
    let mut cfg = cfg.clone();
    match mode {
        "lexical" | "hybrid" | "hybrid_rerank" => {
            cfg.search.semantic = mode != "lexical";
            cfg.search.lazy_semantic = false;
            cfg.search.reranker.enabled = mode == "hybrid_rerank";
            let run = query::run_search(
                home,
                &cfg,
                &opened,
                &SearchRequest {
                    query,
                    limit: lexical::DEFAULT_LIMIT,
                    hybrid: mode != "lexical",
                    with_context: true,
                    filters: SearchFilters::default(),
                },
            )
            .map_err(|e| e.to_string())?;
            for t in &run.timings {
                let stage = t["stage"].as_str().unwrap_or("?");
                out.push((stage.to_owned(), t["duration_ms"].as_f64().unwrap_or(0.0)));
                for s in t["substages"].as_array().into_iter().flatten() {
                    out.push((
                        format!("{stage}.{}", s["stage"].as_str().unwrap_or("?")),
                        s["duration_ms"].as_f64().unwrap_or(0.0),
                    ));
                }
            }
        }
        "evidence" => {
            let v = evidence_value(
                home,
                &cfg,
                &EvidenceRequest {
                    query,
                    filters: SearchFilters::default(),
                    limit: 10,
                },
            )
            .map_err(|e| e.to_string())?;
            for t in v["diagnostics"]["timings"].as_array().into_iter().flatten() {
                let stage = t["stage"].as_str().unwrap_or("?");
                if stage != "total" {
                    out.push((stage.to_owned(), t["ms"].as_f64().unwrap_or(0.0)));
                }
            }
        }
        "explore" => {
            let t = Instant::now();
            let v = query::explore_value_with(
                home,
                &cfg,
                &opened,
                &query::ExploreRequest {
                    query,
                    budget: query::config_budget(&cfg),
                    explain: true,
                },
            )
            .map_err(|e| e.to_string())?;
            out.push(("explore".into(), ms(t)));
            let d = &v["diagnostics"];
            for key in ["stages", "service_stages"] {
                for st in d[key].as_array().into_iter().flatten() {
                    out.push((
                        format!("explore.{}", st["stage"].as_str().unwrap_or("?")),
                        st["duration_ms"].as_f64().unwrap_or(0.0),
                    ));
                }
            }
            out.push((
                "explore.backend_requests".into(),
                d["backend"]["requests"].as_f64().unwrap_or(0.0),
            ));
            out.push((
                "explore.sources_searched".into(),
                d["source_count_searched"].as_f64().unwrap_or(0.0),
            ));
        }
        "impact" => {
            let t = Instant::now();
            query::impact_value(
                &opened,
                query,
                graph::DEFAULT_MAX_DEPTH,
                graph::DEFAULT_LIMIT,
            )
            .map_err(|e| e.to_string())?;
            out.push(("impact".into(), ms(t)));
        }
        other => return Err(format!("unknown mode {other}")),
    }
    out.push(("total".into(), ms(started)));
    Ok(out)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// The lexical planner alone: p50/p95/p99 microseconds per question over
/// `runs` x 100 calls, plus the worst case (P01 regression guard).
fn planner_bench(runs: usize) -> Value {
    let words: Vec<String> = (0..120).map(|i| format!("term{i}x")).collect();
    // clean-slate-audit: allow-start (API URL paths with a version segment are user data)
    let cases = [
        (
            "url_question_25_terms",
            "Is the URL path /sb/v1/content/groups/image used, referenced, or related to any \
             code or documentation in the indexed DocMonk project? Trace matching \
             route/client/API references and relevant call paths."
                .to_owned(),
        ),
        (
            "stress_120_terms",
            format!(
                "{} /sb/v1/content/groups/image \"Quoted.Literal\"",
                words.join(" ")
            ),
        ),
        (
            "short_4_terms",
            "how is the hnsw index persisted".to_owned(),
        ),
    ];
    let mut out = serde_json::Map::new();
    for (name, q) in cases {
        let mut v = Vec::new();
        for _ in 0..runs.max(1) * 100 {
            let t = Instant::now();
            std::hint::black_box(lexical::query_plan(std::hint::black_box(&q)));
            v.push(t.elapsed().as_secs_f64() * 1e6);
        }
        v.sort_by(f64::total_cmp);
        let round = |x: f64| (x * 1000.0).round() / 1000.0;
        out.insert(
            format!("planner.{name}"),
            json!({
                "unit": "us",
                "p50": round(percentile(&v, 0.5)),
                "p95": round(percentile(&v, 0.95)),
                "p99": round(percentile(&v, 0.99)),
                "max": round(v.last().copied().unwrap_or(0.0)),
                "n": v.len(),
            }),
        );
    }
    Value::Object(out)
}

/// `stage -> {p50, p95, n}` over every sample.
fn summarize(samples: &[Vec<(String, f64)>]) -> Value {
    let mut by: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for s in samples {
        for (stage, v) in s {
            by.entry(stage.as_str()).or_default().push(*v);
        }
    }
    let round = |v: f64| (v * 1000.0).round() / 1000.0;
    Value::Object(
        by.into_iter()
            .map(|(k, mut v)| {
                v.sort_by(f64::total_cmp);
                (
                    k.to_owned(),
                    json!({"p50": round(percentile(&v, 0.5)), "p95": round(percentile(&v, 0.95)), "p99": round(percentile(&v, 0.99)), "n": v.len()}),
                )
            })
            .collect(),
    )
}

// ---------------------------------------------------------- server ---

/// Answers every read with no hits and counts it.
struct EmptyEngine;

impl Transport for EmptyEngine {
    fn send(
        &self,
        _method: Method,
        path: &str,
        body: Option<(&[u8], &str)>,
    ) -> docmonk_backends::error::Result<Response> {
        let base = path.split('?').next().unwrap_or(path);
        let v = if base.ends_with("/_msearch") {
            let n = body
                .map(|(b, _)| String::from_utf8_lossy(b).lines().count() / 2)
                .unwrap_or(0);
            json!({"responses": vec![json!({"hits": {"hits": []}}); n]})
        } else if base.contains("/_doc/") {
            return Ok(Response {
                status: 404,
                body: b"{\"found\":false}".to_vec(),
            });
        } else {
            json!({"hits": {"hits": [], "total": {"value": 0}}})
        };
        Ok(Response {
            status: 200,
            body: serde_json::to_vec(&v).expect("json"),
        })
    }
}

/// Requests per query and source for the lexical plan and for a caller
/// traversal, against [`EmptyEngine`].
fn server_counts(queries: &[(String, bool)]) -> Value {
    let server = Arc::new(ServerBackend::with_client(
        Counting::new(Box::new(EmptyEngine)),
        Engine::OpenSearch,
        "bench",
        None,
        BulkLimits::from_config(&DocMonkConfig::default().storage.server.bulk),
    ));
    let reader = ServerReader::new(server.clone(), "s1");
    let corpora = [Corpus {
        store: &reader,
        build_id: "b1",
    }];
    let mut lexical_counts = Vec::new();
    let mut endpoints: BTreeMap<String, u64> = BTreeMap::new();
    for (q, _) in queries {
        let before = server.stats();
        let _ = lexical::search(&corpora, q, lexical::DEFAULT_LIMIT, 32);
        let after = server.stats();
        lexical_counts.push((after.requests - before.requests) as f64);
        for (k, v) in &after.by_endpoint {
            let prev = before.by_endpoint.get(k).copied().unwrap_or(0);
            *endpoints.entry(k.clone()).or_default() += v - prev;
        }
    }
    let mut callers = Vec::new();
    for (q, symbol) in queries {
        if !symbol {
            continue;
        }
        let before = server.stats().requests;
        let _ = graph::traverse_symbol(
            &corpora,
            q,
            Direction::Incoming,
            &graph::CALL_TYPES,
            graph::DEFAULT_MAX_DEPTH,
            graph::DEFAULT_LIMIT,
        );
        callers.push((server.stats().requests - before) as f64);
    }
    let stat = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        json!({"p50": percentile(&v, 0.5), "max": v.last().copied().unwrap_or(0.0)})
    };
    json!({
        "note": "in-process engine with no hits: request counts per query and source (upper bound), not cluster timings",
        "lexical_requests_per_query": stat(lexical_counts),
        "lexical_requests_by_endpoint": endpoints,
        "caller_traversal_requests_per_query": stat(callers),
    })
}

// ------------------------------------------------------------- main ---

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let all = |name: &str| -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == name)
            .map(|w| w[1].clone())
            .collect()
    };
    let flag = |name: &str| args.iter().any(|a| a == name);
    let list = |name: &str, default: &[&str]| -> Vec<String> {
        arg(name)
            .map(|v| v.split(',').map(str::to_owned).collect())
            .unwrap_or_else(|| default.iter().map(|s| (*s).to_owned()).collect())
    };

    // Child process of the `cold` condition: one query, then exit.
    if let Some(mode) = arg("--one") {
        let home = Home::new(PathBuf::from(arg("--home").expect("--home")));
        let cfg = docmonk_service::load(&home).expect("config");
        let q = arg("--query").expect("--query");
        let stages = run_mode(&home, &cfg, &mode, &q).expect("query");
        println!("{}", json!(stages));
        return;
    }

    let files: usize = arg("--files").map_or(2000, |v| v.parse().expect("--files"));
    let docs: usize = arg("--docs").map_or(400, |v| v.parse().expect("--docs"));
    let runs: usize = arg("--runs").map_or(3, |v| v.parse().expect("--runs"));
    let cold_k: usize = arg("--cold-queries").map_or(6, |v| v.parse().expect("--cold-queries"));
    let modes = list("--modes", MODES);
    let conditions = list("--conditions", &["warm", "cold"]);
    let work = arg("--work-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("docmonk-search-bench"));
    let reuse = flag("--reuse") && work.join("home").is_dir();
    let mut corpora: Vec<PathBuf> = all("--corpus").into_iter().map(PathBuf::from).collect();
    let mut groups = 0;
    if !reuse {
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).expect("work dir");
    }
    if corpora.is_empty() {
        let root = work.join("corpus");
        if !reuse {
            groups = corpus::emit(&root, files, docs);
        } else {
            groups = (files.saturating_sub(1) / 3).max(2);
        }
        corpora.push(root);
    }
    let mut queries: Vec<(String, bool)> = match arg("--queries") {
        Some(p) => {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(&p).expect("read --queries"))
                    .expect("--queries is JSON");
            v.as_array()
                .expect("a JSON array")
                .iter()
                .map(|q| match q {
                    Value::String(s) => (s.clone(), false),
                    o => (
                        o["query"].as_str().expect("query").to_owned(),
                        o["symbol"].as_bool().unwrap_or(false),
                    ),
                })
                .collect()
        }
        None => default_queries(groups),
    };
    queries.retain(|(q, _)| !q.trim().is_empty());

    let home = Home::new(work.join("home"));
    home.ensure_layout().expect("home");
    let index_s = if reuse {
        None
    } else {
        let mut cfg = DocMonkConfig::default();
        cfg.updates.enabled = false;
        // Index-time embeddings (query modes choose semantic per mode).
        cfg.search.semantic = true;
        docmonk_config::write_user_config(&cfg, &home).expect("config");
        let mut cat = docmonk_service::sources::catalog(&home).expect("catalog");
        for c in &corpora {
            let canonical = std::fs::canonicalize(c).expect("corpus exists");
            cat.add(canonical.to_str().expect("utf-8 path"), vec![], vec![])
                .expect("add source");
        }
        let t = Instant::now();
        let sources = docmonk_service::indexing::selected_sources(&home, None).expect("sources");
        docmonk_service::indexing::index_sources(&home, &sources, "index", |_| {})
            .expect("index")
            .into_result()
            .expect("index result");
        Some(t.elapsed().as_secs_f64())
    };
    let cfg = docmonk_service::load(&home).expect("config");
    let shape = corpus_shape(&home);
    eprintln!("indexed: {shape}");

    let mut report = json!({
        "label": arg("--label"),
        "corpus": shape,
        "index_seconds": index_s,
        "queries": queries.iter().map(|(q, s)| json!({"query": q, "symbol": s})).collect::<Vec<_>>(),
        "models_dir": std::env::var("DOCMONK_MODELS_DIR").ok().map(|_| "set"),
        "conditions": {},
    });
    for cond in &conditions {
        let mut by_mode = serde_json::Map::new();
        for mode in &modes {
            let qs: Vec<&String> = queries
                .iter()
                .filter(|(_, sym)| mode != "impact" || *sym)
                .map(|(q, _)| q)
                .collect();
            let mut samples = Vec::new();
            let counters = docmonk_ml::counters::snapshot();
            match cond.as_str() {
                "warm" => {
                    // One warm-up pass, then `runs` measured passes.
                    for q in &qs {
                        let _ = run_mode(&home, &cfg, mode, q);
                    }
                    let warm_counters = docmonk_ml::counters::snapshot();
                    for _ in 0..runs {
                        for q in &qs {
                            samples.push(run_mode(&home, &cfg, mode, q).expect("query"));
                        }
                    }
                    let c = docmonk_ml::counters::snapshot();
                    by_mode.insert(
                        mode.clone(),
                        json!({
                            "stages": summarize(&samples),
                            "counters_measured": c.since(&warm_counters),
                            "counters_warmup": warm_counters.since(&counters),
                        }),
                    );
                }
                "cold" => {
                    let exe = std::env::current_exe().expect("exe");
                    for q in qs.iter().take(cold_k) {
                        let t = Instant::now();
                        let o = std::process::Command::new(&exe)
                            .args(["--one", mode, "--home"])
                            .arg(home.root())
                            .args(["--query", q])
                            .output()
                            .expect("spawn");
                        let wall = ms(t);
                        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
                        let mut s: Vec<(String, f64)> =
                            serde_json::from_slice(&o.stdout).expect("child json");
                        s.push(("process_wall".into(), wall));
                        samples.push(s);
                    }
                    by_mode.insert(mode.clone(), json!({"stages": summarize(&samples)}));
                }
                other => panic!("unknown condition {other}"),
            }
            eprintln!(
                "{cond}/{mode}: {}",
                by_mode[mode.as_str()]["stages"]["total"]
            );
        }
        report["conditions"][cond.as_str()] = Value::Object(by_mode);
    }
    if flag("--server") {
        report["server"] = server_counts(&queries);
    }
    if flag("--planner") {
        report["planner"] = planner_bench(runs);
    }
    let text = serde_json::to_string_pretty(&report).expect("json");
    match arg("--out") {
        Some(p) => std::fs::write(p, text + "\n").expect("write report"),
        None => println!("{text}"),
    }
}

/// Files, entities, chunks, edges and vectors of every indexed source.
fn corpus_shape(home: &Home) -> Value {
    let opened = open_sources(home, None).expect("open");
    let mut out = Vec::new();
    for o in &opened {
        let Ok(store) = o.store.local() else {
            continue;
        };
        let count = |table: &str| -> i64 { store.count(table, &o.build).unwrap_or(-1) };
        out.push(json!({
            "source_id": o.source.id,
            "files": count("files"),
            "entities": count("entities"),
            "chunks": count("chunks"),
            "edges": count("relationships"),
            "links": count("cross_links"),
            "vectors": store.embeddings(&o.build).map_or(-1, |v| v.len() as i64),
        }));
    }
    Value::Array(out)
}
