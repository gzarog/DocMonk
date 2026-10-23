//! Embedding throughput benchmark.
//!
//! Generates a deterministic mix of code-signature and document-chunk texts
//! and times `Embedder::embed` over them. `--emit-only <file>` writes the
//! texts as JSON so other embedders can be timed on the same input.
//! `--model <slug>` times another pinned model (`search.embedding_model`).

use std::time::Instant;

use docmonk_ml::embedder::{models_root, Embedder};
use docmonk_ml::manifest::{evaluated_embedding_model, DEFAULT_EMBEDDING_MODEL};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn texts(n: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "invoice", "ledger", "payment", "refund", "customer", "order", "balance", "account",
        "report", "export", "schedule", "retry", "timeout", "cache", "index", "search",
    ];
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                let a = WORDS[i % WORDS.len()];
                let b = WORDS[(i / 3) % WORDS.len()];
                format!("def {a}_{b}_{i}(self, {b}_id: str, amount: Decimal) -> {a}Result")
            } else {
                let len = 20 + (i * 37) % 160;
                let body: Vec<&str> = (0..len).map(|j| WORDS[(i + j * 7) % WORDS.len()]).collect();
                format!("Guide > Section {i}\n\n{}.", body.join(" "))
            }
        })
        .collect()
}

/// The process's peak resident memory (Linux `VmHWM`), when known.
fn peak_rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
}

fn main() {
    let n: usize = arg("--texts").and_then(|v| v.parse().ok()).unwrap_or(1000);
    let batch: i64 = arg("--batch").and_then(|v| v.parse().ok()).unwrap_or(16);
    let corpus = texts(n);
    if let Some(path) = arg("--emit-only") {
        std::fs::write(path, serde_json::json!({ "texts": corpus }).to_string()).unwrap();
        return;
    }
    let spec = match arg("--model") {
        Some(slug) => evaluated_embedding_model(&slug).expect("a pinned model slug"),
        None => DEFAULT_EMBEDDING_MODEL,
    };
    let dir = models_root(None).join(spec.slug);
    let t = Instant::now();
    let embedder = Embedder::load(&dir, spec)
        .expect("model (set DOCMONK_MODELS_DIR)")
        .with_batch_size(batch);
    let load = t.elapsed().as_secs_f64();
    let refs: Vec<&str> = corpus.iter().map(String::as_str).collect();
    let t = Instant::now();
    let vectors = embedder.embed(&refs).expect("embed");
    let wall = t.elapsed().as_secs_f64();
    assert_eq!(vectors.len(), n);
    let report = serde_json::json!({
        "model": spec.hf_id,
        "peak_rss_kib": peak_rss_kib(),
        "texts": n,
        "batch_size": batch,
        "model_load_s": load,
        "embed_wall_s": wall,
        "texts_per_s": n as f64 / wall,
        "threads": std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1),
    });
    let out = serde_json::to_string_pretty(&report).unwrap();
    match arg("--out") {
        Some(p) => std::fs::write(p, out + "\n").unwrap(),
        None => println!("{out}"),
    }
}
