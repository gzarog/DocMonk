//! Chunking benchmark over the golden normalized documents.
//!
//! ```text
//! ragmonk-chunk-bench GOLDEN_JSON [--iterations N]
//! ```
//!
//! Loads every normalized document and configuration from
//! `fixtures/expected/documents.json`, warms the tokenizer, then times
//! chunking only (all documents x all configurations) and prints JSON.

use std::time::Instant;

use docmonk_config::model::{ChunkingConfig, MaxTokens};
use docmonk_documents::chunker::chunk_document;
use docmonk_documents::model::NormalizedDocument;
use serde_json::{json, Value};

fn config(v: &Value) -> ChunkingConfig {
    let mut c = ChunkingConfig::default();
    if let Some(n) = v.get("max_tokens").and_then(Value::as_i64) {
        c.max_tokens = MaxTokens::Value(n);
    }
    for (key, slot) in [
        ("min_tokens", &mut c.min_tokens),
        ("overlap_tokens", &mut c.overlap_tokens),
        ("safety_tokens", &mut c.safety_tokens),
    ] {
        if let Some(n) = v.get(key).and_then(Value::as_i64) {
            *slot = n;
        }
    }
    if let Some(b) = v.get("merge_peers").and_then(Value::as_bool) {
        c.merge_peers = b;
    }
    c
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .expect("usage: ragmonk-chunk-bench GOLDEN_JSON [--iterations N]");
    let iterations: usize = args
        .iter()
        .position(|a| a == "--iterations")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let g: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json");
    let configs: Vec<ChunkingConfig> = g["configs"]
        .as_object()
        .expect("configs")
        .values()
        .map(config)
        .collect();
    let docs: Vec<(NormalizedDocument, String)> = g["documents"]
        .as_object()
        .expect("documents")
        .values()
        .map(|v| {
            (
                serde_json::from_value(v["normalized"].clone()).expect("normalized"),
                v["doc_title"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let _ = chunk_document(&docs[0].0, &configs[0], &docs[0].1, None);
    let mut best = f64::MAX;
    let mut chunks = 0;
    for _ in 0..iterations {
        let started = Instant::now();
        chunks = 0;
        for (doc, title) in &docs {
            for cfg in &configs {
                chunks += chunk_document(doc, cfg, title, None).len();
            }
        }
        best = best.min(started.elapsed().as_secs_f64());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({

            "documents": docs.len(),
            "configs": configs.len(),
            "chunks": chunks,
            "best_wall_time_s": best,
            "iterations": iterations,
        }))
        .expect("json")
    );
}
