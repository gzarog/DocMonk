//! Cross-encoder scores against fixtures/expected/reranker-ms-marco.json.
//!
//! Needs the pinned reranker under `RAGMONK_MODELS_DIR/<slug>`. Without it
//! the test is skipped, unless `RAGMONK_REQUIRE_MODELS=1` (set in CI).

use std::path::PathBuf;

use docmonk_ml::manifest::DEFAULT_RERANKER_MODEL;
use docmonk_ml::reranker::{rerank, CrossEncoder};
use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn model_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("RAGMONK_MODELS_DIR")
        .map(PathBuf::from)
        .map(|d| d.join(DEFAULT_RERANKER_MODEL.slug));
    match dir {
        Some(d) if d.join("model.safetensors").exists() => Some(d),
        _ if std::env::var("RAGMONK_REQUIRE_MODELS").as_deref() == Ok("1") => {
            panic!("RAGMONK_REQUIRE_MODELS=1 but the reranker model is not installed")
        }
        _ => {
            eprintln!("skipping: reranker model not installed (set RAGMONK_MODELS_DIR)");
            None
        }
    }
}

fn read(rel: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(repo_root().join(rel)).unwrap()).unwrap()
}

#[test]
fn logits_and_orderings_are_as_expected() {
    let Some(dir) = model_dir() else { return };
    let ce = CrossEncoder::load(&dir, DEFAULT_RERANKER_MODEL).expect("load");
    let texts: Vec<String> =
        serde_json::from_value(read("fixtures/embeddings/texts.json")["texts"].clone()).unwrap();
    let golden = read("fixtures/expected/reranker-ms-marco.json");
    let passages: Vec<&str> = texts.iter().map(String::as_str).collect();
    let mut worst = 0f32;
    for (q, expected) in golden["queries"]
        .as_array()
        .unwrap()
        .iter()
        .zip(golden["scores"].as_array().unwrap())
    {
        let q = q.as_str().unwrap();
        let got = ce.score(q, &passages).unwrap();
        let exp: Vec<f32> = expected
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        assert_eq!(got.len(), exp.len());
        for (i, (g, e)) in got.iter().zip(&exp).enumerate() {
            let d = (g - e).abs();
            worst = worst.max(d);
            assert!(d < 2e-3, "{q:?} passage {i}: {g} vs {e}");
        }
        // Same reranked order, except swaps between near-ties.
        let order = |s: &[f32]| {
            let mut o: Vec<usize> = (0..s.len()).collect();
            o.sort_by(|a, b| s[*b].total_cmp(&s[*a]));
            o
        };
        for (a, b) in order(&got).iter().zip(order(&exp)) {
            assert!(
                *a == b || (exp[*a] - exp[b]).abs() < 4e-3,
                "{q:?}: order differs"
            );
        }
    }
    eprintln!("worst logit difference vs expected: {worst}");
}

#[test]
fn batching_does_not_change_scores_and_rerank_uses_them() {
    let Some(dir) = model_dir() else { return };
    let ce = CrossEncoder::load(&dir, DEFAULT_RERANKER_MODEL).expect("load");
    let texts: Vec<String> =
        serde_json::from_value(read("fixtures/embeddings/texts.json")["texts"].clone()).unwrap();
    let passages: Vec<&str> = texts.iter().map(String::as_str).collect();
    let all = ce.score("cancel an order", &passages).unwrap();
    for (i, p) in passages.iter().enumerate() {
        let one = ce.score("cancel an order", &[p]).unwrap()[0];
        assert!(
            (one - all[i]).abs() < 1e-4,
            "passage {i}: {one} vs {}",
            all[i]
        );
    }
    let (out, scores) = rerank(
        passages.clone(),
        passages.len(),
        |s| s,
        |t| ce.score("cancel an order", t),
    );
    let scores = scores.unwrap();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]));
    assert_eq!(
        out[0],
        "export function cancelOrder(orderId: string): Promise<void>"
    );
}
