//! ANN benchmark: build time, query latency and recall@10 of the HNSW
//! index against exact search on deterministic unit vectors. `--ef
//! 64,96,200,400,800` measures each beam width over the same index (default:
//! the `search.ann.ef_search` default, 400), with mean, p50 and p95 query
//! latency. `--emit <file>` writes the vectors as little-endian f32
//! (n * dims) followed by the queries, so other indexes can be measured on
//! identical input.

use std::time::Instant;

use docmonk_ml::hnsw::{exact_top_k, Hnsw, HnswParams};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Clustered unit vectors (closer to real embeddings than uniform noise).
fn vectors(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % 20_001) as f32 / 10_000.0 - 1.0
    };
    let centers: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dims).map(|_| next()).collect())
        .collect();
    (0..n)
        .map(|i| {
            let c = &centers[i % centers.len()];
            let mut v: Vec<f32> = c.iter().map(|x| x + 0.6 * next()).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= norm);
            v
        })
        .collect()
}

fn main() {
    let n: usize = arg("--n").and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let dims: usize = arg("--dims").and_then(|v| v.parse().ok()).unwrap_or(384);
    let nq = 200;
    let data = vectors(n, dims, 7);
    let queries = vectors(nq, dims, 99);
    if let Some(path) = arg("--emit") {
        let mut out = Vec::with_capacity((n + nq) * dims * 4);
        for v in data.iter().chain(&queries) {
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        std::fs::write(path, out).unwrap();
        return;
    }
    let t = Instant::now();
    let mut index = Hnsw::new(dims, HnswParams::default());
    for (i, v) in data.iter().enumerate() {
        index.insert(&format!("chunk:{i}"), v);
    }
    let build = t.elapsed().as_secs_f64();
    let labels: Vec<String> = (0..n).map(|i| format!("chunk:{i}")).collect();
    let t = Instant::now();
    let truth: Vec<Vec<(String, f32)>> = queries
        .iter()
        .map(|q| {
            exact_top_k(
                q,
                labels
                    .iter()
                    .map(String::as_str)
                    .zip(data.iter().map(Vec::as_slice)),
                10,
            )
        })
        .collect();
    let exact_ms = t.elapsed().as_secs_f64() * 1000.0 / nq as f64;
    let efs: Vec<usize> = arg("--ef")
        .unwrap_or_else(|| "400".into())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let mut sweep = Vec::new();
    for &ef in &efs {
        // One warm-up pass so every beam width is timed on a warm index.
        for q in queries.iter().take(20) {
            index.search_ef(q, 10, ef);
        }
        let mut ms = Vec::with_capacity(nq);
        let mut hit = 0;
        for (q, t) in queries.iter().zip(&truth) {
            let started = Instant::now();
            let r = index.search_ef(q, 10, ef);
            ms.push(started.elapsed().as_secs_f64() * 1000.0);
            hit += t.iter().filter(|x| r.iter().any(|y| y.0 == x.0)).count();
        }
        let mean = ms.iter().sum::<f64>() / nq as f64;
        ms.sort_by(f64::total_cmp);
        let pct = |p: f64| ms[((nq as f64 * p).ceil() as usize).clamp(1, nq) - 1];
        sweep.push(serde_json::json!({
            "ef_search": ef,
            "recall_at_10": hit as f64 / (nq * 10) as f64,
            "query_ms": mean,
            "p50_ms": pct(0.50),
            "p95_ms": pct(0.95),
        }));
    }
    let path = std::env::temp_dir().join(format!("docmonk-ann-bench-{}.hnsw", std::process::id()));
    let meta = docmonk_ml::ann::IndexMeta {
        format_version: docmonk_ml::ann::FORMAT_VERSION,
        fingerprint: "bench".into(),
        build_id: "bench".into(),
        dims,
        content: String::new(),
    };
    let t = Instant::now();
    docmonk_ml::ann::save(&path, &index, &meta).unwrap();
    let save_s = t.elapsed().as_secs_f64();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let t = Instant::now();
    let loaded = docmonk_ml::ann::load(&path).unwrap();
    let load_s = t.elapsed().as_secs_f64();
    assert_eq!(loaded.0.len(), n);
    let _ = std::fs::remove_file(&path);
    let params = HnswParams::default();
    let report = serde_json::json!({
        "engine": format!("hnsw (m={}, ef_construction={})", params.m, params.ef_construction),
        "vectors": n,
        "dims": dims,
        "queries": nq,
        "build_s": build,
        "exact_query_ms": exact_ms,
        "save_s": save_s,
        "load_s": load_s,
        "file_bytes": size,
        "sweep": sweep,
    });
    let out = serde_json::to_string_pretty(&report).unwrap();
    match arg("--out") {
        Some(p) => std::fs::write(p, out + "\n").unwrap(),
        None => println!("{out}"),
    }
}
