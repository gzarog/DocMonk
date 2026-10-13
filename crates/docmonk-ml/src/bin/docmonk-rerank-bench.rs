//! Cross-encoder latency benchmark. Scores every query in
//! fixtures/embeddings/queries.json against all 20 passages of texts.json,
//! 3 rounds, and reports p50/p95 per 20-pair call.
//!
//! `docmonk-rerank-bench <models root> [--out file]`

use std::time::Instant;

use docmonk_ml::manifest::DEFAULT_RERANKER_MODEL;
use docmonk_ml::reranker::CrossEncoder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = std::path::PathBuf::from(args.get(1).expect("models root"));
    let repo_root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/embeddings");
    let read = |f: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(repo_root.join(f)).unwrap()).unwrap()
    };
    let texts: Vec<String> = serde_json::from_value(read("texts.json")["texts"].clone()).unwrap();
    let queries: Vec<String> =
        serde_json::from_value(read("queries.json")["queries"].clone()).unwrap();
    let passages: Vec<&str> = texts.iter().map(String::as_str).collect();
    let t = Instant::now();
    let ce = CrossEncoder::load(
        &root.join(DEFAULT_RERANKER_MODEL.slug),
        DEFAULT_RERANKER_MODEL,
    )
    .unwrap();
    let load_s = t.elapsed().as_secs_f64();
    ce.score("warm", &passages[..2]).unwrap();
    let mut ms = Vec::new();
    for _ in 0..3 {
        for q in &queries {
            let t = Instant::now();
            ce.score(q, &passages).unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    ms.sort_by(f64::total_cmp);
    let p = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    let report = serde_json::json!({

        "model": DEFAULT_RERANKER_MODEL.hf_id,
        "pairs_per_call": passages.len(),
        "calls": ms.len(),
        "load_s": load_s,
        "p50_ms": p(0.5),
        "p95_ms": p(0.95),
    });
    let out = serde_json::to_string_pretty(&report).unwrap();
    match args.iter().position(|a| a == "--out") {
        Some(i) => std::fs::write(&args[i + 1], out + "\n").unwrap(),
        None => println!("{out}"),
    }
}
