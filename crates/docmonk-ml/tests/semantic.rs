//! Semantic search: rankings against the exact cosine ranking in
//! fixtures/expected/semantic.json, and ANN crash/delete/rebuild behaviour.

mod common;

use std::path::Path;

use common::{fixture, models_root, repo_root, Fx};
use docmonk_ml::ann::{self, Engine};
use docmonk_ml::embedder::Embedder;
use docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;
use docmonk_ml::semantic::{self, SemanticHit};
use serde_json::{json, Value};

fn embedder(models: &Path) -> Embedder {
    Embedder::load(
        &models.join(DEFAULT_EMBEDDING_MODEL.slug),
        DEFAULT_EMBEDDING_MODEL,
    )
    .unwrap()
}

fn key(h: &SemanticHit) -> Value {
    if h.kind == "entity" {
        json!(["entity", h.path, h.title, h.position])
    } else {
        json!(["section", h.path, h.position])
    }
}

fn run(fx: &Fx, emb: &Embedder, q: &str, k: usize) -> (Engine, Vec<SemanticHit>) {
    let (s, b) = fx.store();
    let dir = s.project_dir().unwrap();
    let r = semantic::search(&s, &dir, &b, Some(emb), q, k, None).unwrap();
    assert!(r.available, "{}", r.reason);
    (r.engine.unwrap(), r.hits)
}

/// Same ranking as expected, allowing swaps only between near-ties.
fn assert_matches(golden: &Value, hits: &[SemanticHit], q: &str) {
    let expected = golden["hits"].as_array().unwrap();
    assert_eq!(hits.len(), expected.len(), "{q}");
    for (i, (h, e)) in hits.iter().zip(expected).enumerate() {
        let es = e["score"].as_f64().unwrap() as f32;
        assert!((h.score - es).abs() < 1e-3, "{q} #{i}: {} vs {es}", h.score);
        if key(h) != e["key"] {
            let near = expected.iter().any(|o| {
                o["key"] == key(h) && (o["score"].as_f64().unwrap() as f32 - es).abs() < 1e-4
            });
            assert!(near, "{q} #{i}: got {} expected {}", key(h), e["key"]);
        }
    }
}

#[test]
fn hnsw_and_exact_rankings_are_as_expected() {
    let Some(models) = models_root() else { return };
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/semantic.json")).unwrap(),
    )
    .unwrap();
    let mut fx = fixture();
    fx.index(&models);
    let emb = embedder(&models);
    let (s, b) = fx.store();
    assert_eq!(
        s.embeddings(&b).unwrap().len() as u64,
        golden["subjects"].as_u64().unwrap(),
        "same subjects embedded as expected"
    );
    let dir = s.project_dir().unwrap();
    drop(s);
    for g in golden["results"].as_array().unwrap() {
        let q = g["query"].as_str().unwrap();
        let (engine, hits) = run(&fx, &emb, q, 10);
        assert_eq!(engine, Engine::Hnsw);
        assert_matches(g, &hits, q);
    }
    std::fs::remove_file(ann::index_path(&dir)).unwrap();
    for g in golden["results"].as_array().unwrap() {
        let q = g["query"].as_str().unwrap();
        let (engine, hits) = run(&fx, &emb, q, 10);
        assert_eq!(engine, Engine::Exact);
        assert_matches(g, &hits, q);
    }
}

#[test]
fn corrupt_or_stale_indexes_fall_back_and_are_rebuilt() {
    let Some(models) = models_root() else { return };
    let mut fx = fixture();
    fx.index(&models);
    let emb = embedder(&models);
    let (s, _) = fx.store();
    let path = ann::index_path(&s.project_dir().unwrap());
    drop(s);
    let (_, baseline) = run(&fx, &emb, "cancel an order", 5);

    // Crash mid-write leaves a truncated file: never trusted.
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
    let (engine, hits) = run(&fx, &emb, "cancel an order", 5);
    assert_eq!(engine, Engine::Exact);
    assert_eq!(hits, baseline);
    // The next (warm) pass rebuilds it.
    fx.index(&models);
    let (engine, hits) = run(&fx, &emb, "cancel an order", 5);
    assert_eq!(engine, Engine::Hnsw);
    assert_eq!(hits, baseline);

    // Vectors changed behind the index (crash between write and sync):
    // the content digest no longer matches, so search is exact.
    let (s, b) = fx.store();
    s.connection()
        .execute(
            "DELETE FROM embeddings WHERE build_id = ?1 AND subject_type = 'chunk'",
            [&b],
        )
        .unwrap();
    let (engine, hits) = run(&fx, &emb, "cancel an order", 5);
    assert_eq!(engine, Engine::Exact);
    assert!(hits.iter().all(|h| h.kind == "entity"));
    drop(s);
    fx.index(&models);
    let (engine, hits) = run(&fx, &emb, "cancel an order", 5);
    assert_eq!(engine, Engine::Hnsw);
    assert_eq!(hits, baseline);
}

#[test]
fn deleted_files_leave_the_index_and_model_changes_rebuild_it() {
    let Some(models) = models_root() else { return };
    let mut fx = fixture();
    fx.index(&models);
    let emb = embedder(&models);
    let (_, before) = run(&fx, &emb, "cancel an order", 32);
    assert!(before.iter().any(|h| h.path == "docs/orders.html"));

    std::fs::remove_file(fx.root.join("docs/orders.html")).unwrap();
    fx.index(&models);
    let (engine, after) = run(&fx, &emb, "cancel an order", 32);
    assert_eq!(engine, Engine::Hnsw);
    assert!(after.iter().all(|h| h.path != "docs/orders.html"));
    assert!(!after.is_empty());

    // A different model fingerprint never reuses the old graph.
    let (mut s, b) = fx.store();
    let dir = s.project_dir().unwrap();
    let mut spec = DEFAULT_EMBEDDING_MODEL;
    spec.max_chars = 2000;
    let changed = Embedder::load(&models.join(spec.slug), spec).unwrap();
    docmonk_ml::embed_build(&mut s, &b, &changed, "2").unwrap();
    let stats = ann::sync(&s, &dir, &b, &spec.fingerprint(), spec.dims).unwrap();
    assert!(stats.rebuilt);
    assert_eq!(stats.live, s.embeddings(&b).unwrap().len());
    let again = ann::sync(&s, &dir, &b, &spec.fingerprint(), spec.dims).unwrap();
    assert!(!again.rebuilt && again.added == 0 && again.removed == 0);
}

#[test]
fn unavailable_model_and_empty_query_are_reported_not_raised() {
    let mut fx = fixture();
    let empty = tempfile::tempdir().unwrap();
    fx.index(empty.path());
    let (s, b) = fx.store();
    let dir = s.project_dir().unwrap();
    let r = semantic::search(&s, &dir, &b, None, "orders", 5, None).unwrap();
    assert!(!r.available && r.hits.is_empty());
    assert_eq!(r.reason, "embedding model unavailable");
    let Some(models) = models_root() else { return };
    let emb = embedder(&models);
    let r = semantic::search(&s, &dir, &b, Some(&emb), "   ", 5, None).unwrap();
    assert!(r.available && r.hits.is_empty());
    assert_eq!(r.reason, "empty query");
}
