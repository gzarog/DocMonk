//! Semantic search quality gate on 72 golden queries
//! (fixtures/search_quality), with and without the cross-encoder.
//!
//! Metrics are Recall@1/3/5/10, MRR and NDCG@10 over `kind:path` keys;
//! each must reach its floor in `fixtures/search_quality/gates.json`.
//! `RAGMONK_QUALITY_OUT=<file>` also writes the measured numbers as a
//! benchmark record.

mod common;

use std::collections::{BTreeMap, HashSet};

use common::{fixture_of, models_root, repo_root};
use docmonk_ml::embedder::Embedder;
use docmonk_ml::manifest::{DEFAULT_EMBEDDING_MODEL, DEFAULT_RERANKER_MODEL};
use docmonk_ml::reranker::{rerank, CrossEncoder};
use docmonk_ml::semantic;
use serde_json::{json, Value};

const TOP_N: usize = 20;

fn recall(got: &[String], rel: &HashSet<String>, k: usize) -> f64 {
    let top: HashSet<&String> = got.iter().take(k).collect();
    rel.iter().filter(|r| top.contains(r)).count() as f64 / rel.len() as f64
}

fn rr(got: &[String], rel: &HashSet<String>) -> f64 {
    got.iter()
        .position(|g| rel.contains(g))
        .map_or(0.0, |i| 1.0 / (i + 1) as f64)
}

fn ndcg10(got: &[String], rel: &HashSet<String>) -> f64 {
    let mut seen = HashSet::new();
    let mut dcg = 0.0;
    for (i, g) in got.iter().take(10).enumerate() {
        if rel.contains(g) && seen.insert(g) {
            dcg += 1.0 / ((i + 2) as f64).log2();
        }
    }
    let ideal: f64 = (0..rel.len().min(10))
        .map(|i| 1.0 / ((i + 2) as f64).log2())
        .sum();
    dcg / ideal
}

fn summarize(ranked: &[(Vec<String>, HashSet<String>)]) -> BTreeMap<String, f64> {
    let n = ranked.len() as f64;
    let mut m = BTreeMap::new();
    for k in [1, 3, 5, 10] {
        m.insert(
            format!("recall@{k}"),
            ranked.iter().map(|(g, r)| recall(g, r, k)).sum::<f64>() / n,
        );
    }
    m.insert(
        "mrr".into(),
        ranked.iter().map(|(g, r)| rr(g, r)).sum::<f64>() / n,
    );
    m.insert(
        "ndcg@10".into(),
        ranked.iter().map(|(g, r)| ndcg10(g, r)).sum::<f64>() / n,
    );
    m
}

#[test]
fn semantic_and_reranked_quality_meet_the_gates() {
    let Some(models) = models_root() else { return };
    let rerank_dir = models.join(DEFAULT_RERANKER_MODEL.slug);
    if !rerank_dir.join("model.safetensors").exists() {
        assert_ne!(
            std::env::var("RAGMONK_REQUIRE_MODELS").as_deref(),
            Ok("1"),
            "reranker model not installed"
        );
        return;
    }
    let read = |rel: &str| -> Value {
        serde_json::from_str(&std::fs::read_to_string(repo_root().join(rel)).unwrap()).unwrap()
    };
    let golden = read("fixtures/search_quality/queries.json");
    let gates = read("fixtures/search_quality/gates.json");
    let mut fx = fixture_of("fixtures/search_quality/project");
    fx.index(&models);
    let emb = Embedder::load(
        &models.join(DEFAULT_EMBEDDING_MODEL.slug),
        DEFAULT_EMBEDDING_MODEL,
    )
    .unwrap();
    let ce = CrossEncoder::load(&rerank_dir, DEFAULT_RERANKER_MODEL).unwrap();
    let (store, build) = fx.store();
    let dir = store.project_dir().unwrap();

    let mut plain = Vec::new();
    let mut reranked = Vec::new();
    let mut rerank_ms = Vec::new();
    for item in golden["queries"].as_array().unwrap() {
        let q = item["query"].as_str().unwrap();
        let rel: HashSet<String> = item["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{}:{}",
                    e["kind"].as_str().unwrap(),
                    e["path"].as_str().unwrap()
                )
            })
            .collect();
        let r = semantic::search(&store, &dir, &build, Some(&emb), q, TOP_N, Some(50)).unwrap();
        assert!(r.available, "{q}: {}", r.reason);
        let key = |h: &semantic::SemanticHit| format!("{}:{}", h.kind, h.path);
        plain.push((
            r.hits.iter().take(10).map(key).collect::<Vec<_>>(),
            rel.clone(),
        ));
        let started = std::time::Instant::now();
        let text = |h: &semantic::SemanticHit| -> String {
            if h.snippet.is_empty() {
                h.title.clone()
            } else {
                h.snippet.clone()
            }
        };
        let items: Vec<(semantic::SemanticHit, String)> =
            r.hits.iter().map(|h| (h.clone(), text(h))).collect();
        let (hits, scores) = rerank(items, TOP_N, |(_, t)| t.as_str(), |t| ce.score(q, t));
        assert!(scores.is_some() || hits.len() < 2);
        rerank_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        reranked.push((
            hits.iter()
                .take(10)
                .map(|(h, _)| key(h))
                .collect::<Vec<_>>(),
            rel,
        ));
    }
    let ours = json!({
        "semantic": summarize(&plain),
        "semantic_rerank": summarize(&reranked),
    });
    rerank_ms.sort_by(f64::total_cmp);
    eprintln!("measured: {ours:#}");
    for arm in ["semantic", "semantic_rerank"] {
        for (metric, floor) in gates[arm].as_object().unwrap() {
            let got = ours[arm][metric].as_f64().unwrap();
            let floor = floor.as_f64().unwrap();
            assert!(
                got >= floor,
                "{arm} {metric}: {got:.4} is below the gate {floor:.2}"
            );
        }
    }
    if let Some(out) = std::env::var_os("RAGMONK_QUALITY_OUT") {
        let round = |m: &Value| -> Value {
            m.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        json!((v.as_f64().unwrap() * 10_000.0).round() / 10_000.0),
                    )
                })
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        let p = |q: f64| rerank_ms[((rerank_ms.len() - 1) as f64 * q).round() as usize];
        let rec = json!({
            "queries": plain.len(),
            "rerank_top_n": TOP_N,
            "semantic": round(&ours["semantic"]),
            "semantic_rerank": round(&ours["semantic_rerank"]),
            "rerank_stage_ms": {"p50": p(0.5), "p95": p(0.95)},
        });
        std::fs::write(out, serde_json::to_string_pretty(&rec).unwrap() + "\n").unwrap();
    }
}
