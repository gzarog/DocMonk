//! Live filtered ANN recall (ADR 0041): on a disposable OpenSearch or
//! Elasticsearch cluster, a synthetic corpus of 384-dimension chunk
//! vectors is published twice (the second build replaces a tenth of the
//! vectors and garbage-collects the first), so the indexes hold deleted
//! documents and a retired generation, as after a real update. Then the
//! engine's filtered kNN is compared with exact cosine search over the
//! same visible, filtered vectors.
//!
//! Set `DOCMONK_TEST_OPENSEARCH_URL` / `DOCMONK_TEST_ELASTICSEARCH_URL`
//! (skipped otherwise), `DOCMONK_ANN_RECALL_N` for the corpus size
//! (default 3000) and `DOCMONK_ANN_RECALL_OUT` to write the report.

use std::time::{Duration, Instant};

use docmonk_backends::backend::ChunkDoc;
use docmonk_backends::bulk::BulkLimits;
use docmonk_backends::engine::{Engine, VectorSpec};
use docmonk_backends::reader::ServerReader;
use docmonk_backends::schema::IndexSettings;
use docmonk_backends::transport::{Auth, Client, Counting, HttpTransport, Method};
use docmonk_backends::ServerBackend;
use docmonk_core::ids::record;
use docmonk_storage::read::{KnowledgeRead, VectorFilter};
use serde_json::{json, Value};

const DIMS: usize = 384;
const K: usize = 10;
const QUERIES: usize = 40;
const FILES: usize = 40;

/// Floor of mean recall@10 for every filter and engine.
const MIN_RECALL: f64 = 0.95;

fn url(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|u| !u.is_empty())
}

/// Deterministic xorshift in [-1, 1).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in &mut v {
        *x /= n;
    }
    v
}

/// `n` vectors around 30 cluster centres (real embeddings cluster too, so
/// near neighbours are close calls), spread over `FILES` files in four
/// directories.
fn corpus(n: usize, seed: u64) -> Vec<(String, String, Vec<f32>)> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..30)
        .map(|_| unit((0..DIMS).map(|_| rng.next()).collect()))
        .collect();
    (0..n)
        .map(|i| {
            let c = &centres[i % centres.len()];
            let v = unit(c.iter().map(|x| x + 0.35 * rng.next()).collect());
            let rel = format!("dir{}/f{}.md", i % 4, i % FILES);
            (format!("c{i}"), rel, v)
        })
        .collect()
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn publish(b: &ServerBackend, src: &str, build: &str, docs: &[(String, String, Vec<f32>)]) {
    let g = b.begin_build(src, "/repo", build).unwrap();
    let mut w = b.build_writer(src, build, g);
    for (i, (id, rel, v)) in docs.iter().enumerate() {
        let file_id = record::file_id(src, rel);
        w.chunk(&ChunkDoc {
            chunk_id: id.clone(),
            document_id: record::document_id(&file_id, None),
            file_id,
            rel_path: rel.clone(),
            kind: "paragraph".into(),
            ordinal: i as i64,
            search_text: format!("chunk {id}"),
            text: format!("chunk {id}"),
            embedding: Some(v.clone()),
            embedding_fingerprint: Some("fp".into()),
            ..Default::default()
        })
        .unwrap();
    }
    w.finish().unwrap();
    b.publish_build(src, build, &json!({}), true).unwrap();
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn run(base: &str, engine: Engine, n: usize) -> Value {
    let prefix = format!(
        "annrecall{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000_000
    );
    let client: Client = Counting::new(Box::new(
        HttpTransport::new(base, Auth::None, Duration::from_secs(120), true).unwrap(),
    ));
    let b = std::sync::Arc::new(
        ServerBackend::with_client(
            client,
            engine,
            &prefix,
            Some(VectorSpec {
                dims: DIMS as u32,
                m: 16,
                ef_construction: 100,
                model_id: "ann-recall".into(),
            }),
            BulkLimits {
                max_actions: 500,
                max_bytes: 20_000_000,
                max_retries: 3,
                base_backoff: Duration::from_millis(50),
            },
        )
        .with_settings(IndexSettings {
            shards: 1,
            replicas: 0,
        }),
    );
    b.init().unwrap();
    let src = "src_ann";
    let first = corpus(n, 7);
    // The update: a tenth of the vectors change, the rest are rewritten
    // as they were (a full build closes and collects the first one).
    let replacement = corpus(n, 11);
    let second: Vec<_> = first
        .iter()
        .zip(&replacement)
        .enumerate()
        .map(|(i, (a, r))| {
            if i % 10 == 0 {
                (a.0.clone(), a.1.clone(), r.2.clone())
            } else {
                a.clone()
            }
        })
        .collect();
    let t = Instant::now();
    publish(&b, src, "annb1", &first);
    publish(&b, src, "annb2", &second);
    let publish_s = t.elapsed().as_secs_f64();
    let r = ServerReader::new(b.clone(), src);
    let mut rng = Rng(23);
    let queries: Vec<Vec<f32>> = (0..QUERIES)
        .map(|i| {
            let base = &second[(i * 97) % second.len()].2;
            unit(base.iter().map(|x| x + 0.2 * rng.next()).collect())
        })
        .collect();
    let filters: [(&str, Vec<String>); 3] = [
        ("unfiltered", vec![]),
        ("one directory (25%)", vec!["dir1/".into()]),
        ("one file (2.5%)", vec!["dir2/f2.md".into()]),
    ];
    let mut out = serde_json::Map::new();
    for (label, prefixes) in filters {
        let vf = VectorFilter {
            path_prefixes: prefixes.clone(),
            documents: true,
            ..Default::default()
        };
        let mut recalls = Vec::new();
        let mut ms = Vec::new();
        for q in &queries {
            let mut exact: Vec<(&str, f32)> = second
                .iter()
                .filter(|(_, rel, _)| {
                    prefixes.is_empty() || prefixes.iter().any(|p| rel.starts_with(p.as_str()))
                })
                .map(|(id, _, v)| (id.as_str(), cos(q, v)))
                .collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1));
            let want: Vec<&str> = exact.iter().take(K).map(|(id, _)| *id).collect();
            let t = Instant::now();
            let got = r
                .vector_search_filtered("annb2", "fp", q, K, 0, &vf)
                .unwrap()
                .expect("the server pushes filters into kNN");
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
            let hits = got
                .iter()
                .filter(|(_, id, _)| want.contains(&id.as_str()))
                .count();
            recalls.push(hits as f64 / want.len().max(1) as f64);
        }
        ms.sort_by(f64::total_cmp);
        let recall = recalls.iter().sum::<f64>() / recalls.len() as f64;
        assert!(
            recall >= MIN_RECALL,
            "{engine:?} {label}: recall@{K} {recall:.3} < {MIN_RECALL}"
        );
        out.insert(
            label.into(),
            json!({
                "recall@10": (recall * 1000.0).round() / 1000.0,
                "p50_ms": (percentile(&ms, 0.5) * 10.0).round() / 10.0,
                "p95_ms": (percentile(&ms, 0.95) * 10.0).round() / 10.0,
            }),
        );
    }
    let c: Client = Counting::new(Box::new(
        HttpTransport::new(base, Auth::None, Duration::from_secs(60), true).unwrap(),
    ));
    for kind in docmonk_backends::schema::IndexKind::ALL {
        let _ = c.send(
            Method::Delete,
            &format!("/{}", docmonk_backends::schema::index_name(&prefix, kind)),
            None,
        );
    }
    json!({
        "vectors": n,
        "dims": DIMS,
        "queries": QUERIES,
        "k": K,
        "publish_two_builds_s": (publish_s * 10.0).round() / 10.0,
        "filters": out,
    })
}

#[test]
fn filtered_ann_recall_after_publication_and_gc() {
    let n = std::env::var("DOCMONK_ANN_RECALL_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let mut report = serde_json::Map::new();
    for (name, var, engine) in [
        (
            "opensearch",
            "DOCMONK_TEST_OPENSEARCH_URL",
            Engine::OpenSearch,
        ),
        (
            "elasticsearch",
            "DOCMONK_TEST_ELASTICSEARCH_URL",
            Engine::Elasticsearch,
        ),
    ] {
        match url(var) {
            Some(u) => {
                report.insert(name.into(), run(&u, engine, n));
            }
            None => {
                eprintln!("skipping {name}: {var} not set");
                report.insert(name.into(), json!({"not_run": format!("{var} unset")}));
            }
        }
    }
    if let Ok(out) = std::env::var("DOCMONK_ANN_RECALL_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap() + "\n").unwrap();
    }
}
