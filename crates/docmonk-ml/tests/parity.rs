//! Embedding vectors against fixtures/expected/embeddings-minilm.json
//! (cosine tolerance).
//!
//! Needs the pinned model under `RAGMONK_MODELS_DIR/<slug>`. Without it the
//! test is skipped, unless `RAGMONK_REQUIRE_MODELS=1` (set in CI).

use std::path::PathBuf;

use docmonk_ml::embedder::{cosine, Embedder};
use docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn model_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("RAGMONK_MODELS_DIR")
        .map(PathBuf::from)
        .map(|d| d.join(DEFAULT_EMBEDDING_MODEL.slug));
    match dir {
        Some(d) if d.join("model.safetensors").exists() => Some(d),
        _ if std::env::var("RAGMONK_REQUIRE_MODELS").as_deref() == Ok("1") => {
            panic!("RAGMONK_REQUIRE_MODELS=1 but the embedding model is not installed")
        }
        _ => {
            eprintln!("skipping: embedding model not installed (set RAGMONK_MODELS_DIR)");
            None
        }
    }
}

#[test]
fn vectors_are_as_expected_within_cosine_tolerance() {
    let Some(dir) = model_dir() else { return };
    let embedder = Embedder::load(&dir, DEFAULT_EMBEDDING_MODEL).expect("load");
    let texts: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/embeddings/texts.json")).unwrap(),
    )
    .unwrap();
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo_root().join("fixtures/expected/embeddings-minilm.json"))
            .unwrap(),
    )
    .unwrap();
    let texts: Vec<&str> = texts["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    let expected: Vec<Vec<f32>> = golden["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            v.as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap() as f32)
                .collect()
        })
        .collect();
    assert_eq!(golden["model"], DEFAULT_EMBEDDING_MODEL.hf_id);
    let actual = embedder.embed(&texts).expect("embed");
    assert_eq!(actual.len(), expected.len());
    let mut worst = 1.0f32;
    for (i, (a, e)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(a.len(), DEFAULT_EMBEDDING_MODEL.dims);
        let c = cosine(a, e);
        worst = worst.min(c);
        assert!(c >= 0.9999, "text {i}: cosine {c}");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "text {i}: norm {norm}");
    }
    eprintln!("worst cosine vs expected: {worst}");
    // Batch composition must not change a vector (padding is masked out).
    let single = embedder.embed(&texts[..1]).unwrap();
    assert!(cosine(&single[0], &actual[0]) > 0.99999);
}

#[test]
fn tampered_assets_are_rejected() {
    let Some(dir) = model_dir() else { return };
    let tmp = tempfile::tempdir().unwrap();
    for f in DEFAULT_EMBEDDING_MODEL.files {
        std::fs::copy(dir.join(f.name), tmp.path().join(f.name)).unwrap();
    }
    std::fs::write(tmp.path().join("config.json"), b"{}").unwrap();
    let err = Embedder::load(tmp.path(), DEFAULT_EMBEDDING_MODEL).unwrap_err();
    assert!(err.to_string().contains("integrity"), "{err}");
}
