//! Held-out retrieval evaluation (local-model retrieval plan, phase 0).
//!
// clean-slate-audit: allow-start (indexed source languages)
//! A versioned corpus (`fixtures/eval/corpus`: Python, TypeScript, C# and
// clean-slate-audit: allow-end
//! Rust code with implementation details in function bodies, an
//! unsupported-language script, a file with syntax errors, English, Greek,
//! German, French, Japanese, Chinese and Arabic documents, a mixed-language
//! runbook, a CSV table and a scanned PDF) is indexed once per backend and
//! queried with the labels in `fixtures/eval/queries.json`.
//!
//! Every arm reads the same snapshot through the same service entry points
//! the CLI uses: `lexical` (plain search), `semantic`, `hybrid` (RRF),
//! `hybrid_reranked` (RRF + cross-encoder), `evidence` (routed, grounded)
//! and `explore`. Metrics per arm, category and query language: Recall@1/5/10,
//! MRR, NDCG@10, line provenance (the cited span covers the labelled line),
//! and for `evidence` citation validity, false support and abstention on
//! unanswerable queries. Latency p50/p95 per arm, peak resident memory,
//! clean-build and one-file update time and extraction coverage are
//! recorded with the model availability and every degraded stage.
//!
//! The labels were written by a model and are pending human review: their
//! metrics are reported, and the floors in `fixtures/eval/heldout-gates.json`
//! apply only to queries a reviewer marked `approved`. Hard gates hold for
//! every query regardless: two runs over the same snapshot rank identically,
//! the label file is never rewritten, evidence citations re-verify and hard
//! filters never leak.
//!
//! `DOCMONK_EVAL_OUT=path` writes the full report. Server backends run when
//! `DOCMONK_TEST_OPENSEARCH_URL` / `DOCMONK_TEST_ELASTICSEARCH_URL` point at
//! disposable clusters (see CONTRIBUTING.md); otherwise the report records
//! them as not run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::explore::Budget;
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use docmonk_service::query::{self, open_sources, SearchRequest};
use serde_json::{json, Value};

const K: usize = 10;
/// Longest cited span (lines) that counts as line provenance: a hit
/// covering a whole file (a module namespace) does not locate anything.
const MAX_PROVENANCE_SPAN: i64 = 80;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

fn sha256_file(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(std::fs::read(p).unwrap());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Peak resident set size of this process (`VmHWM`, KiB); `None` where
/// `/proc` is unavailable.
fn peak_rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

/// Which optional local models are installed (never downloaded).
#[derive(Debug, Clone, Copy)]
struct Models {
    embedding: bool,
    reranker: bool,
    ocr: bool,
}

impl Models {
    fn detect() -> Self {
        let root = std::env::var_os("DOCMONK_MODELS_DIR").map(PathBuf::from);
        let has = |slug: &str| {
            root.as_ref()
                .is_some_and(|r| r.join(slug).join("model.safetensors").is_file())
        };
        let ocr = std::env::var_os("DOCMONK_OCR_MODELS_DIR")
            .map(PathBuf::from)
            .is_some_and(|d| d.join("text-recognition.rten").is_file());
        Models {
            embedding: has(docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL.slug),
            reranker: has(docmonk_ml::manifest::DEFAULT_RERANKER_MODEL.slug),
            ocr,
        }
    }

    fn json(&self) -> Value {
        let state = |ok: bool, what: &str| {
            if ok {
                json!("available")
            } else {
                json!(format!("unavailable: {what} not installed"))
            }
        };
        json!({
            "embedding": state(self.embedding, "DOCMONK_MODELS_DIR embedding model"),
            "reranker": state(self.reranker, "DOCMONK_MODELS_DIR reranker model"),
            "ocr": state(self.ocr, "DOCMONK_OCR_MODELS_DIR"),
        })
    }
}

#[derive(Debug, Clone)]
struct Expected {
    path: String,
    line: Option<i64>,
}

#[derive(Debug, Clone)]
struct Query {
    id: String,
    category: String,
    language: String,
    text: String,
    expected: Vec<Expected>,
    requires: Vec<String>,
    approved: bool,
}

fn load_queries(path: &Path) -> Vec<Query> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| Query {
            id: q["id"].as_str().unwrap().into(),
            category: q["category"].as_str().unwrap().into(),
            language: q["query_language"].as_str().unwrap_or("und").into(),
            text: q["query"].as_str().unwrap().into(),
            expected: q["expected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| Expected {
                    path: e["path"].as_str().unwrap().into(),
                    line: e["line"].as_i64(),
                })
                .collect(),
            requires: q["requires"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            approved: q["review_status"] == "approved",
        })
        .collect()
}

/// One ranked citation, path relative to the corpus root.
#[derive(Debug, Clone, PartialEq)]
struct Hit {
    path: String,
    kind: String,
    line_start: Option<i64>,
    line_end: Option<i64>,
    valid: Option<bool>,
}

/// What one arm returned for one query.
#[derive(Debug, Clone, PartialEq)]
struct Answer {
    hits: Vec<Hit>,
    verdict: Option<String>,
    degraded: Vec<String>,
}

const ARMS: &[&str] = &[
    "lexical",
    "semantic",
    "hybrid",
    "hybrid_reranked",
    "evidence",
    "explore",
];

struct Backend {
    name: String,
    _dir: tempfile::TempDir,
    home: Home,
    root: PathBuf,
    cleanup: Option<(String, String)>,
    build_ms: f64,
    update_ms: f64,
    update_indexed: usize,
    coverage: Value,
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some((base, prefix)) = &self.cleanup {
            // By name: Elasticsearch refuses wildcard deletes by default.
            use docmonk_backends::schema::{index_name, IndexKind};
            let c = reqwest::blocking::Client::new();
            for kind in IndexKind::ALL {
                let _ = c
                    .delete(format!("{base}/{}", index_name(prefix, kind)))
                    .send();
            }
        }
    }
}

fn base_config(models: Models) -> DocMonkConfig {
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.search.semantic = models.embedding;
    cfg
}

/// One indexing run: its summary and the files each completed pass indexed.
fn index_all(home: &Home) -> (docmonk_service::indexing::RunSummary, usize) {
    use docmonk_service::indexing::SourceEvent;
    let sources = docmonk_service::indexing::selected_sources(home, None).unwrap();
    let mut indexed = 0;
    let summary = docmonk_service::indexing::index_sources(home, &sources, "index", |e| {
        if let SourceEvent::Completed { run, .. } = e {
            indexed += run.indexed;
        }
    })
    .unwrap();
    (summary, indexed)
}

/// Indexes a copy of the corpus into a fresh home on `backend`, then edits
/// one file and re-indexes (the one-file update cost).
fn setup(backend: &str, server: Option<(&str, &str)>, models: Models) -> Backend {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = base_config(models);
    let mut cleanup = None;
    if let Some((engine, url)) = server {
        let prefix = format!(
            "heldout{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000_000
        );
        cfg.storage.mode = "server".into();
        cfg.storage.server.engine = match engine {
            "opensearch" => docmonk_config::model::StorageEngine::OpenSearch,
            _ => docmonk_config::model::StorageEngine::Elasticsearch,
        };
        cfg.storage.server.url = url.into();
        cfg.storage.server.index_prefix = prefix.clone();
        cfg.storage.server.gc_grace_seconds = 0.0;
        cleanup = Some((url.to_owned(), prefix));
    }
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let root = dir.path().join("corpus");
    copy_dir(&repo().join("fixtures/eval/corpus"), &root);
    docmonk_service::sources::catalog(&home)
        .unwrap()
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap();
    let t = Instant::now();
    let (first, first_indexed) = index_all(&home);
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(first.failed_sources, 0, "{backend}: clean build failed");
    // One-file update: an appended comment changes one file's hash.
    let edited = root.join("ledger/src/reconcile.rs");
    let mut body = std::fs::read_to_string(&edited).unwrap();
    body.push_str("\n// touched by the one-file update measurement\n");
    std::fs::write(&edited, body).unwrap();
    let t = Instant::now();
    let (_, update_indexed) = index_all(&home);
    let update_ms = t.elapsed().as_secs_f64() * 1000.0;
    if let Some((url, prefix)) = &cleanup {
        settle_segments(url, prefix);
    }
    let coverage = coverage(&home, &first, first_indexed);
    Backend {
        name: backend.into(),
        _dir: dir,
        home,
        root,
        cleanup,
        build_ms,
        update_ms,
        update_indexed,
        coverage,
    }
}

/// Merges the test indexes' segments before measuring. The update's
/// garbage collection deletes the edited file's old records, and Lucene
/// counts deleted documents in its term statistics until a background
/// merge expunges them. That merge can happen between the two passes and
/// shift BM25 scores of near-tied hits, which says nothing about
/// DocMonk's determinism. Settled segments keep the reproducibility gate
/// about DocMonk alone.
fn settle_segments(url: &str, prefix: &str) {
    use docmonk_backends::schema::{index_name, IndexKind};
    let names: Vec<String> = IndexKind::ALL
        .iter()
        .map(|k| index_name(prefix, *k))
        .collect();
    let c = reqwest::blocking::Client::new();
    let list = names.join(",");
    for path in [
        format!("{url}/{list}/_forcemerge?max_num_segments=1&ignore_unavailable=true"),
        format!("{url}/{list}/_refresh?ignore_unavailable=true"),
    ] {
        let r = c.post(&path).send().expect("settle the test indexes");
        assert!(r.status().is_success(), "{path}: {}", r.status());
    }
}

/// What the build extracted: files by kind and status, and record counts
/// (local store only; a server build reports its publication counts).
fn coverage(home: &Home, run: &docmonk_service::indexing::RunSummary, indexed: usize) -> Value {
    let opened = open_sources(home, None).unwrap();
    let mut out = json!({});
    for o in &opened {
        if let Some(server) = o.store.server() {
            use docmonk_backends::schema::IndexKind;
            let count = |k: IndexKind| server.backend().count(k, Some(&o.source.id)).ok();
            out = json!({
                "files": count(IndexKind::Files),
                "entities": count(IndexKind::Code),
                "chunks": count(IndexKind::Chunks),
                "documents": count(IndexKind::Documents),
                "passages": count(IndexKind::Passages),
            });
        }
        if let Ok(store) = o.store.local() {
            let files = store.files(&o.build).unwrap();
            let mut by: BTreeMap<String, usize> = BTreeMap::new();
            for f in &files {
                *by.entry(format!("{}:{}", f.kind, f.status)).or_default() += 1;
            }
            let count = |t: &str| store.count(t, &o.build).unwrap_or(-1);
            out = json!({
                "files": by,
                "entities": count("entities"),
                "chunks": count("chunks"),
                "documents": count("documents"),
                "passages": count("code_passages"),
                "degraded_files": files
                    .iter()
                    .filter(|f| f.last_error.is_some())
                    .map(|f| json!({"path": f.rel_path, "status": f.status, "reason": f.last_error}))
                    .collect::<Vec<_>>(),
            });
        }
    }
    out["indexed_files"] = json!(indexed);
    out["failed_files"] = json!(run.failed_files);
    out
}

fn rel(root: &Path, p: &str) -> String {
    let p = p.replace('\\', "/");
    let r = root.to_string_lossy().replace('\\', "/");
    // Canonicalized roots may differ from the temp path (macOS /private).
    let canon = docmonk_core::paths::resolve(root)
        .map(|c| c.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| r.clone());
    for prefix in [canon, r] {
        if let Some(s) = p.strip_prefix(&format!("{prefix}/")) {
            return s.to_owned();
        }
    }
    p
}

fn line(v: &Value) -> Option<i64> {
    v.as_i64()
}

/// Runs one arm for one query.
fn run_arm(b: &Backend, cfg: &DocMonkConfig, arm: &str, q: &Query) -> Answer {
    let opened = open_sources(&b.home, None).unwrap();
    let mut degraded = Vec::new();
    let hits: Vec<Hit>;
    let mut verdict = None;
    match arm {
        "lexical" | "hybrid" | "hybrid_reranked" => {
            let mut cfg = cfg.clone();
            cfg.search.reranker.enabled = arm == "hybrid_reranked";
            let run = query::run_search(
                &b.home,
                &cfg,
                &opened,
                &SearchRequest {
                    query: &q.text,
                    limit: K,
                    hybrid: arm != "lexical",
                    with_context: false,
                    filters: SearchFilters::default(),
                },
            )
            .unwrap();
            if arm != "lexical" {
                match &run.semantic {
                    Some(s) if !s.available => degraded.push(format!("semantic: {}", s.reason)),
                    None => degraded.push("semantic: not run".into()),
                    _ => {}
                }
            }
            if arm == "hybrid_reranked" && query::cross_encoder(&b.home).is_none() {
                degraded.push("reranker: model unavailable".into());
            }
            hits = match &run.ranked {
                Some(ranked) => ranked
                    .iter()
                    .map(|h| {
                        let l = h.candidate.location.as_ref();
                        Hit {
                            path: rel(&b.root, &h.candidate.path),
                            kind: h.candidate.kind.clone(),
                            line_start: l.and_then(|l| l.line_start),
                            line_end: l.and_then(|l| l.line_end),
                            valid: None,
                        }
                    })
                    .collect(),
                None => run
                    .results
                    .iter()
                    .map(|r| {
                        let l = r.location.as_ref();
                        Hit {
                            path: rel(&b.root, &r.path),
                            kind: r.kind.into(),
                            line_start: l.and_then(|l| l.line_start),
                            line_end: l.and_then(|l| l.line_end),
                            valid: None,
                        }
                    })
                    .collect(),
            };
        }
        "semantic" => {
            let v = query::semantic_value(&b.home, cfg, &opened, &q.text, K).unwrap();
            if v["available"] != true {
                degraded.push(format!("semantic: {}", v["reason"]));
            }
            hits = v["results"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| Hit {
                    path: rel(&b.root, r["path"].as_str().unwrap_or_default()),
                    kind: r["kind"].as_str().unwrap_or_default().into(),
                    line_start: line(&r["location"]["line_start"]),
                    line_end: line(&r["location"]["line_end"]),
                    valid: None,
                })
                .collect();
        }
        "evidence" => {
            let v = evidence_value(
                &b.home,
                cfg,
                &EvidenceRequest {
                    query: &q.text,
                    filters: SearchFilters::default(),
                    limit: K,
                },
            )
            .unwrap();
            verdict = v["verdict"].as_str().map(str::to_owned);
            for d in v["diagnostics"]["degraded"]
                .as_array()
                .into_iter()
                .flatten()
            {
                degraded.push(d.as_str().unwrap_or_default().to_owned());
            }
            hits = v["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| Hit {
                    path: rel(&b.root, e["path"].as_str().unwrap_or_default()),
                    kind: e["kind"].as_str().unwrap_or_default().into(),
                    line_start: line(&e["line_start"]),
                    line_end: line(&e["line_end"]),
                    valid: e["valid"].as_bool(),
                })
                .collect();
        }
        "explore" => {
            let v =
                query::explore_value(&b.home, cfg, &opened, &q.text, Budget::default()).unwrap();
            if v["semantic_available"] == false {
                degraded.push(format!("semantic: {}", v["semantic_reason"]));
            }
            hits = v["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| Hit {
                    path: rel(&b.root, e["path"].as_str().unwrap_or_default()),
                    kind: e["source"].as_str().unwrap_or_default().into(),
                    line_start: line(&e["location"]["line_start"]),
                    line_end: line(&e["location"]["line_end"]),
                    valid: None,
                })
                .collect();
        }
        other => panic!("unknown arm {other}"),
    }
    degraded.sort();
    degraded.dedup();
    Answer {
        hits,
        verdict,
        degraded,
    }
}

/// Distinct paths in rank order.
fn ranked_paths(a: &Answer) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    a.hits
        .iter()
        .filter(|h| seen.insert(h.path.clone()))
        .map(|h| h.path.clone())
        .collect()
}

#[derive(Default, Clone, Copy)]
struct Acc {
    n: f64,
    pos: f64,
    r1: f64,
    r5: f64,
    r10: f64,
    mrr: f64,
    ndcg: f64,
    lines: f64,
    lines1: f64,
    line_n: f64,
    cited: f64,
    cited_valid: f64,
    neg: f64,
    abstained: f64,
    false_support: f64,
    false_abstain: f64,
    skipped: f64,
}

impl Acc {
    fn add(&mut self, o: &Acc) {
        self.n += o.n;
        self.pos += o.pos;
        self.r1 += o.r1;
        self.r5 += o.r5;
        self.r10 += o.r10;
        self.mrr += o.mrr;
        self.ndcg += o.ndcg;
        self.lines += o.lines;
        self.lines1 += o.lines1;
        self.line_n += o.line_n;
        self.cited += o.cited;
        self.cited_valid += o.cited_valid;
        self.neg += o.neg;
        self.abstained += o.abstained;
        self.false_support += o.false_support;
        self.false_abstain += o.false_abstain;
        self.skipped += o.skipped;
    }

    fn json(&self, evidence: bool) -> Value {
        let r = |v: f64, n: f64| {
            if n == 0.0 {
                Value::Null
            } else {
                json!(((v / n) * 1000.0).round() / 1000.0)
            }
        };
        let mut out = json!({
            "n": self.n as u64,
            "answerable": self.pos as u64,
            "skipped": self.skipped as u64,
            "recall@1": r(self.r1, self.pos),
            "recall@5": r(self.r5, self.pos),
            "recall@10": r(self.r10, self.pos),
            "mrr": r(self.mrr, self.pos),
            "ndcg@10": r(self.ndcg, self.pos),
            "line_provenance@1": r(self.lines1, self.line_n),
            "line_provenance@10": r(self.lines, self.line_n),
        });
        if evidence {
            out["citation_validity"] = r(self.cited_valid, self.cited);
            out["abstention_unanswerable"] = r(self.abstained, self.neg);
            out["false_support_rate"] = r(self.false_support, self.neg);
            out["false_abstention_rate"] = r(self.false_abstain, self.pos);
        }
        out
    }
}

fn score(q: &Query, a: &Answer, arm: &str) -> Acc {
    let mut acc = Acc {
        n: 1.0,
        ..Acc::default()
    };
    let paths = ranked_paths(a);
    let relevant = |p: &str| q.expected.iter().any(|e| e.path == p);
    if q.expected.is_empty() {
        acc.neg = 1.0;
        if arm == "evidence" {
            let abstained =
                a.verdict.as_deref() == Some("insufficient_evidence") || a.hits.is_empty();
            acc.abstained = f64::from(u8::from(abstained));
            acc.false_support = f64::from(u8::from(a.verdict.as_deref() == Some("supported")));
        }
    } else {
        acc.pos = 1.0;
        let found = |k: usize| {
            let top: Vec<&String> = paths.iter().take(k).collect();
            q.expected
                .iter()
                .filter(|e| top.iter().any(|p| **p == e.path))
                .count() as f64
                / q.expected.len() as f64
        };
        acc.r1 = found(1);
        acc.r5 = found(5);
        acc.r10 = found(10);
        acc.mrr = paths
            .iter()
            .position(|p| relevant(p))
            .map_or(0.0, |i| 1.0 / (i as f64 + 1.0));
        let dcg: f64 = paths
            .iter()
            .take(K)
            .enumerate()
            .filter(|(_, p)| relevant(p))
            .map(|(i, _)| 1.0 / (i as f64 + 2.0).log2())
            .sum();
        let ideal: f64 = (0..q.expected.len().min(K))
            .map(|i| 1.0 / (i as f64 + 2.0).log2())
            .sum();
        acc.ndcg = (dcg / ideal).min(1.0);
        for e in q.expected.iter().filter(|e| e.line.is_some()) {
            let want = e.line.unwrap_or_default();
            acc.line_n += 1.0;
            let covers = |h: &Hit| {
                h.path == e.path
                    && matches!((h.line_start, h.line_end), (Some(s), Some(t))
                        if s <= want && want <= t && t - s < MAX_PROVENANCE_SPAN)
            };
            acc.lines += f64::from(u8::from(a.hits.iter().take(K).any(covers)));
            acc.lines1 += f64::from(u8::from(a.hits.first().is_some_and(covers)));
        }
        if arm == "evidence" {
            acc.false_abstain = f64::from(u8::from(
                a.verdict.as_deref() == Some("insufficient_evidence"),
            ));
        }
    }
    if arm == "evidence" {
        for h in a.hits.iter().take(K) {
            acc.cited += 1.0;
            acc.cited_valid += f64::from(u8::from(h.valid == Some(true)));
        }
    }
    acc
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    (v[((v.len() - 1) as f64 * p).round() as usize] * 10.0).round() / 10.0
}

type Answers = BTreeMap<(String, String), Answer>;

/// Every arm over every query once. Queries whose required capability is
/// missing are skipped (and reported), never scored as failures.
fn run_all(
    b: &Backend,
    models: Models,
    queries: &[Query],
) -> (Answers, BTreeMap<String, Vec<f64>>) {
    let cfg = docmonk_service::load(&b.home).unwrap();
    let mut answers = Answers::new();
    let mut latency: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for arm in ARMS {
        if *arm == "semantic" && !models.embedding {
            continue;
        }
        for q in queries {
            if q.requires.iter().any(|r| r == "ocr") && !models.ocr {
                continue;
            }
            let t = Instant::now();
            let a = run_arm(b, &cfg, arm, q);
            latency
                .entry((*arm).to_owned())
                .or_default()
                .push(t.elapsed().as_secs_f64() * 1000.0);
            answers.insert(((*arm).to_owned(), q.id.clone()), a);
        }
    }
    (answers, latency)
}

fn metrics(queries: &[Query], answers: &Answers, models: Models, only_approved: bool) -> Value {
    let mut out = json!({});
    for arm in ARMS {
        let evidence = *arm == "evidence";
        let mut by_cat: BTreeMap<String, Acc> = BTreeMap::new();
        let mut by_lang: BTreeMap<String, Acc> = BTreeMap::new();
        let mut all = Acc::default();
        let mut skipped_arm = false;
        for q in queries.iter().filter(|q| !only_approved || q.approved) {
            let acc = match answers.get(&((*arm).to_owned(), q.id.clone())) {
                Some(a) => score(q, a, arm),
                None => {
                    skipped_arm |= *arm == "semantic" && !models.embedding;
                    Acc {
                        n: 1.0,
                        skipped: 1.0,
                        ..Acc::default()
                    }
                }
            };
            by_cat.entry(q.category.clone()).or_default().add(&acc);
            by_lang.entry(q.language.clone()).or_default().add(&acc);
            all.add(&acc);
        }
        out[*arm] = json!({
            "all": all.json(evidence),
            "by_category": by_cat.iter().map(|(k, a)| (k.clone(), a.json(evidence))).collect::<serde_json::Map<_, _>>(),
            "by_query_language": by_lang.iter().map(|(k, a)| (k.clone(), a.json(evidence))).collect::<serde_json::Map<_, _>>(),
        });
        if skipped_arm {
            out[*arm]["skipped_reason"] = json!("embedding model unavailable");
        }
    }
    out
}

/// Queries an arm answered wrongly, with the top paths (the report's
/// failure examples).
fn failures(queries: &[Query], answers: &Answers, arm: &str) -> Vec<Value> {
    queries
        .iter()
        .filter_map(|q| {
            let a = answers.get(&(arm.to_owned(), q.id.clone()))?;
            let paths = ranked_paths(a);
            let ok = if q.expected.is_empty() {
                arm != "evidence" || a.verdict.as_deref() == Some("insufficient_evidence")
            } else {
                q.expected
                    .iter()
                    .all(|e| paths.iter().take(K).any(|p| *p == e.path))
            };
            (!ok).then(|| {
                json!({
                    "id": q.id, "category": q.category, "query": q.text,
                    "expected": q.expected.iter().map(|e| &e.path).collect::<Vec<_>>(),
                    "top": paths.iter().take(5).collect::<Vec<_>>(),
                    "verdict": a.verdict, "degraded": a.degraded,
                })
            })
        })
        .collect()
}

fn evaluate(b: &Backend, models: Models, queries: &[Query]) -> Value {
    let (first, mut latency) = run_all(b, models, queries);
    // Reproducibility: a second pass over the same snapshot must return
    // the same rankings, citations and verdicts.
    let (second, _) = run_all(b, models, queries);
    let top = |a: Option<&Answer>| -> Vec<String> {
        a.map(|a| {
            a.hits
                .iter()
                .map(|h| format!("{}:{:?}", h.path, h.line_start))
                .collect()
        })
        .unwrap_or_default()
    };
    let mut diverged = Vec::new();
    for (k, a) in &first {
        if second.get(k) != Some(a) {
            diverged.push(format!(
                "{}/{}: {:?} then {:?}",
                k.0,
                k.1,
                top(Some(a)),
                top(second.get(k))
            ));
        }
    }
    assert!(
        diverged.is_empty(),
        "{}: rankings changed between two runs over the same snapshot:\n{}",
        b.name,
        diverged.join("\n")
    );
    // Evidence citations returned as evidence must re-verify.
    let invalid: Vec<String> = first
        .iter()
        .filter(|((arm, _), _)| arm == "evidence")
        .flat_map(|((_, id), a)| {
            a.hits
                .iter()
                .filter(|h| h.valid == Some(false))
                .map(move |h| format!("{id}: {}", h.path))
        })
        .collect();
    assert!(
        invalid.is_empty(),
        "{}: invalid citations {invalid:?}",
        b.name
    );
    let mut degraded: BTreeMap<String, usize> = BTreeMap::new();
    for a in first.values() {
        for d in &a.degraded {
            *degraded.entry(d.clone()).or_default() += 1;
        }
    }
    let skipped: Vec<&str> = queries
        .iter()
        .filter(|q| q.requires.iter().any(|r| r == "ocr") && !models.ocr)
        .map(|q| q.id.as_str())
        .collect();
    json!({
        "metrics": metrics(queries, &first, models, false),
        "metrics_approved_only": if queries.iter().any(|q| q.approved) {
            metrics(queries, &first, models, true)
        } else {
            Value::Null
        },
        "latency_ms": latency.iter_mut().map(|(arm, v)| (arm.clone(), json!({
            "p50": percentile(v, 0.5), "p95": percentile(v, 0.95), "n": v.len(),
        }))).collect::<serde_json::Map<_, _>>(),
        "degraded_stages": degraded,
        "skipped_queries": skipped,
        "reproducible": true,
        "indexing": {
            "clean_build_ms": (b.build_ms * 10.0).round() / 10.0,
            "one_file_update_ms": (b.update_ms * 10.0).round() / 10.0,
            "one_file_update_files_indexed": b.update_indexed,
        },
        "extraction": b.coverage,
        "failures": ARMS.iter().map(|arm| ((*arm).to_owned(), json!(failures(queries, &first, arm)))).collect::<serde_json::Map<_, _>>(),
    })
}

fn server_url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            assert!(
                std::env::var("DOCMONK_REQUIRE_LIVE_SERVER").as_deref() != Ok("1"),
                "{var} is required when DOCMONK_REQUIRE_LIVE_SERVER=1"
            );
            None
        }
    }
}

#[test]
fn heldout_retrieval_eval() {
    let labels = repo().join("fixtures/eval/queries.json");
    let labels_hash = sha256_file(&labels);
    let queries = load_queries(&labels);
    assert!(
        queries.len() >= 100,
        "the held-out set has at least 100 queries"
    );
    let models = Models::detect();
    let mut report = json!({
        "harness": "crates/docmonk-service/tests/heldout_eval.rs",
        "corpus": "fixtures/eval/corpus",
        "labels": {
            "file": "fixtures/eval/queries.json",
            "sha256": labels_hash,
            "queries": queries.len(),
            "approved": queries.iter().filter(|q| q.approved).count(),
            "origin": "model_authored (pending human review)",
        },
        "models": models.json(),
        "build_profile": if cfg!(debug_assertions) { "debug (latency is not representative)" } else { "release" },
        "backends": {},
    });
    let mut backends = vec![("sqlite".to_owned(), None)];
    for (engine, var) in [
        ("opensearch", "DOCMONK_TEST_OPENSEARCH_URL"),
        ("elasticsearch", "DOCMONK_TEST_ELASTICSEARCH_URL"),
    ] {
        match server_url(var) {
            Some(u) => backends.push((engine.to_owned(), Some(u))),
            None => {
                report["backends"][engine] = json!({"not_run": format!("{var} unset")});
            }
        }
    }
    for (name, url) in &backends {
        let b = setup(name, url.as_deref().map(|u| (name.as_str(), u)), models);
        report["backends"][name] = evaluate(&b, models, &queries);
    }
    report["peak_rss_kib"] = json!(peak_rss_kib());
    // Labels are inputs only: running the evaluation never rewrites them.
    assert_eq!(
        sha256_file(&labels),
        labels_hash,
        "the evaluation must not modify fixtures/eval/queries.json"
    );
    if let Ok(out) = std::env::var("DOCMONK_EVAL_OUT") {
        std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    }
    // Floors apply to reviewer-approved queries only.
    let gates: Value = serde_json::from_str(
        &std::fs::read_to_string(repo().join("fixtures/eval/heldout-gates.json")).unwrap(),
    )
    .unwrap();
    let mut regressions = Vec::new();
    for (backend, arms) in gates["floors"].as_object().into_iter().flatten() {
        let Some(got) = report["backends"].get(backend.as_str()) else {
            continue;
        };
        if got.get("not_run").is_some() {
            continue;
        }
        for (arm, metrics) in arms.as_object().into_iter().flatten() {
            for (metric, floor) in metrics.as_object().into_iter().flatten() {
                let v = got["metrics_approved_only"][arm]["all"][metric]
                    .as_f64()
                    .unwrap_or(0.0);
                if v + 1e-9 < floor.as_f64().unwrap_or(0.0) {
                    regressions.push(format!("{backend}/{arm}/{metric}: {v} < {floor}"));
                }
            }
        }
    }
    assert!(
        regressions.is_empty(),
        "held-out regressions on approved queries:\n{}",
        regressions.join("\n")
    );
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&report["backends"]["sqlite"]["metrics"]["evidence"]["all"])
            .unwrap()
    );
}
