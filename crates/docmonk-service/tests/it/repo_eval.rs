//! Retrieval evaluation over DocMonk's own repository (P0.2).
//!
//! `fixtures/search_quality/repo_queries.json` holds 157 author-reviewed
//! queries over two sources, `crates/` and `docs/`: exact symbols, the
//! same identifiers as natural-language phrases (camelCase and snake_case
//! split into words, plurals), path lookups, conceptual questions, hard
//! filters and negative (out-of-domain) queries. Unlike the generated P0
//! set, it is not saturated: identifiers phrased as prose are exactly
//! what plain FTS misses.
//!
//! Arms: `lexical` (`search`), `routed` (`evidence`, semantic per the
//! route), `hybrid` (`search --hybrid`) and `hybrid_rerank`. The semantic
//! arms run only with the pinned models (`DOCMONK_MODELS_DIR`). Metrics
//! per arm and intent: nDCG@10, MRR, recall@1/5/10 (positive queries,
//! file-level, binary relevance) and abstention (negative queries).
//!
//! Indexing the repository takes a while, so the test runs only with
//! `DOCMONK_REPO_EVAL=1` (nightly CI runs it in release mode).
//! `DOCMONK_EVAL_OUT=path` writes the report. Floors live in
//! `fixtures/search_quality/repo-eval-gates.json`; a metric more than the
//! file's `tolerance` below its floor fails. `DOCMONK_BLESS=1` rewrites
//! the floors from this run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_retrieval::route::SearchFilters;
use docmonk_service::evidence::{evidence_value, EvidenceRequest};
use docmonk_service::query::{open_sources, run_search, SearchRequest};
use serde_json::{json, Value};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repository root")
        .to_path_buf()
}

#[derive(Debug, Clone)]
struct Query {
    id: String,
    intent: String,
    text: String,
    expected: Vec<String>,
    filters: SearchFilters,
}

fn queries() -> Vec<Query> {
    let path = repo_root().join("fixtures/search_quality/repo_queries.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["queries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| {
            let list = |k: &str| -> Vec<String> {
                q["filters"][k]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            };
            Query {
                id: q["id"].as_str().unwrap().to_owned(),
                intent: q["intent"].as_str().unwrap().to_owned(),
                text: q["query"].as_str().unwrap().to_owned(),
                expected: q["expected"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e.as_str().unwrap().to_owned())
                    .collect(),
                filters: SearchFilters {
                    path_prefixes: list("path_prefixes"),
                    kinds: list("kinds"),
                    ..Default::default()
                },
            }
        })
        .collect()
}

/// Ranked file paths (absolute, deduplicated) one arm returns for `q`.
fn ranked(home: &Home, cfg: &DocMonkConfig, arm: &str, q: &Query) -> Vec<String> {
    let mut cfg = cfg.clone();
    let paths: Vec<String> = match arm {
        "routed" => {
            let v = evidence_value(
                home,
                &cfg,
                &EvidenceRequest {
                    query: &q.text,
                    filters: q.filters.clone(),
                    limit: 10,
                },
            )
            .unwrap();
            if v["verdict"] == "insufficient_evidence" {
                Vec::new()
            } else {
                v["evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|e| e["path"].as_str().map(str::to_owned))
                    .collect()
            }
        }
        _ => {
            cfg.search.semantic = arm != "lexical";
            cfg.search.lazy_semantic = false;
            cfg.search.reranker.enabled = arm == "hybrid_rerank";
            let opened = open_sources(home, None).unwrap();
            let run = run_search(
                home,
                &cfg,
                &opened,
                &SearchRequest {
                    query: &q.text,
                    limit: 10,
                    hybrid: arm != "lexical",
                    with_context: false,
                    filters: q.filters.clone(),
                },
            )
            .unwrap();
            match run.ranked {
                Some(hits) => hits.into_iter().map(|h| h.candidate.path).collect(),
                None => run.results.into_iter().map(|r| r.path).collect(),
            }
        }
    };
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

#[derive(Default, Clone)]
struct Acc {
    n: usize,
    ndcg: f64,
    mrr: f64,
    r1: f64,
    r5: f64,
    r10: f64,
    negatives: usize,
    abstained: usize,
}

impl Acc {
    fn add(&mut self, got: &[String], expected: &[String]) {
        if expected.is_empty() {
            self.negatives += 1;
            self.abstained += usize::from(got.is_empty());
            return;
        }
        let hit = |p: &String| expected.iter().any(|e| p.ends_with(&format!("/{e}")));
        let rel: Vec<bool> = got.iter().take(10).map(hit).collect();
        let first = rel.iter().position(|r| *r);
        let found = |k: usize| {
            let n = rel.iter().take(k).filter(|r| **r).count() as f64;
            n / expected.len().min(k) as f64
        };
        let dcg: f64 = rel
            .iter()
            .enumerate()
            .filter(|(_, r)| **r)
            .map(|(i, _)| 1.0 / (i as f64 + 2.0).log2())
            .sum();
        let ideal: f64 = (0..expected.len().min(10))
            .map(|i| 1.0 / (i as f64 + 2.0).log2())
            .sum();
        self.n += 1;
        self.ndcg += dcg / ideal;
        self.mrr += first.map_or(0.0, |i| 1.0 / (i as f64 + 1.0));
        self.r1 += found(1);
        self.r5 += found(5);
        self.r10 += found(10);
    }

    fn json(&self) -> Value {
        let n = self.n.max(1) as f64;
        let r = |v: f64| (v * 10_000.0).round() / 10_000.0;
        let mut v = json!({
            "queries": self.n,
            "ndcg@10": r(self.ndcg / n),
            "mrr": r(self.mrr / n),
            "recall@1": r(self.r1 / n),
            "recall@5": r(self.r5 / n),
            "recall@10": r(self.r10 / n),
        });
        if self.negatives > 0 {
            v["abstention"] = json!(r(self.abstained as f64 / self.negatives as f64));
        }
        v
    }
}

#[test]
fn repo_retrieval_eval() {
    if std::env::var("DOCMONK_REPO_EVAL").as_deref() != Ok("1") {
        eprintln!("skipped: set DOCMONK_REPO_EVAL=1 to index the repository and run the eval");
        return;
    }
    let root = repo_root();
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let models = std::env::var_os("DOCMONK_MODELS_DIR").is_some();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.search.semantic = models;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let mut cat = docmonk_service::sources::catalog(&home).unwrap();
    for sub in ["crates", "docs"] {
        let p = std::fs::canonicalize(root.join(sub)).unwrap();
        cat.add(p.to_str().unwrap(), vec![], vec![]).unwrap();
    }
    let t = Instant::now();
    let sources = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &sources, "index", |_| {})
        .unwrap()
        .into_result()
        .unwrap();
    let index_s = t.elapsed().as_secs_f64();
    let cfg = docmonk_service::load(&home).unwrap();

    let qs = queries();
    let mut arms = vec!["lexical", "routed"];
    if models {
        arms.extend(["hybrid", "hybrid_rerank"]);
    }
    let mut report = json!({
        "dataset": {"queries": qs.len(), "labels": "author-reviewed (not independently reviewed)", "sources": ["crates", "docs"]},
        "index_seconds": index_s,
        "models": models,
        "arms": {},
    });
    let mut failures: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for arm in &arms {
        let mut by: BTreeMap<String, Acc> = BTreeMap::new();
        let mut lat = Vec::new();
        for q in &qs {
            let t = Instant::now();
            let got = ranked(&home, &cfg, arm, q);
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
            by.entry("all".into()).or_default().add(&got, &q.expected);
            by.entry(q.intent.clone())
                .or_default()
                .add(&got, &q.expected);
            let ok = if q.expected.is_empty() {
                got.is_empty()
            } else {
                got.iter()
                    .take(5)
                    .any(|p| q.expected.iter().any(|e| p.ends_with(&format!("/{e}"))))
            };
            if !ok {
                failures.entry(arm.to_string()).or_default().push(json!({
                    "id": q.id, "intent": q.intent, "query": q.text,
                    "expected": q.expected,
                    "top5": got.iter().take(5).map(|p| p.strip_prefix(root.to_str().unwrap()).unwrap_or(p)).collect::<Vec<_>>(),
                }));
            }
        }
        lat.sort_by(f64::total_cmp);
        report["arms"][*arm] = json!({
            "metrics": by.iter().map(|(k, v)| (k.clone(), v.json())).collect::<serde_json::Map<_, _>>(),
            "latency_ms": {"p50": lat[lat.len() / 2], "p95": lat[lat.len() * 95 / 100]},
        });
    }
    report["failures_top5"] = json!(failures);
    if let Ok(out) = std::env::var("DOCMONK_EVAL_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    }

    // The set must not be saturated.
    for arm in &arms {
        let r5 = report["arms"][*arm]["metrics"]["all"]["recall@5"]
            .as_f64()
            .unwrap();
        assert!(r5 < 1.0, "{arm} is saturated (recall@5 = 1.0)");
    }

    let gates_path = root.join("fixtures/search_quality/repo-eval-gates.json");
    const KEYS: [&str; 5] = ["ndcg@10", "mrr", "recall@1", "recall@5", "recall@10"];
    if std::env::var("DOCMONK_BLESS").as_deref() == Ok("1") {
        let mut floors = serde_json::Map::new();
        for arm in &arms {
            let mut per = serde_json::Map::new();
            for (intent, m) in report["arms"][*arm]["metrics"].as_object().unwrap() {
                let mut f = serde_json::Map::new();
                for k in KEYS {
                    if let Some(v) = m[k].as_f64() {
                        f.insert(k.into(), json!(v));
                    }
                }
                per.insert(intent.clone(), Value::Object(f));
            }
            floors.insert(arm.to_string(), Value::Object(per));
        }
        let gates = json!({
            "description": "Measured repo-eval metrics (fixtures/search_quality/repo_queries.json). A metric more than `tolerance` below its recorded value fails crates/docmonk-service/tests/repo_eval.rs. Rewrite with DOCMONK_BLESS=1 when a change is meant to move them, and say why in the PR.",
            "tolerance": 0.02,
            "arms": floors,
        });
        std::fs::write(
            &gates_path,
            serde_json::to_string_pretty(&gates).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let gates: Value =
        serde_json::from_str(&std::fs::read_to_string(&gates_path).unwrap()).unwrap();
    let tol = gates["tolerance"].as_f64().unwrap();
    let mut regressions = Vec::new();
    for arm in &arms {
        let Some(per) = gates["arms"][*arm].as_object() else {
            continue;
        };
        for (intent, floors) in per {
            for k in KEYS {
                let (Some(floor), Some(got)) = (
                    floors[k].as_f64(),
                    report["arms"][*arm]["metrics"][intent][k].as_f64(),
                ) else {
                    continue;
                };
                if got + tol < floor {
                    regressions.push(format!("{arm}/{intent}/{k}: {got} < {floor} - {tol}"));
                }
            }
        }
    }
    assert!(
        regressions.is_empty(),
        "eval regressions:\n{}",
        regressions.join("\n")
    );
}
