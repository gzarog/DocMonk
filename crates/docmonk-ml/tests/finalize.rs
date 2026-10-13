//! Embedding finalizer: completeness, cache reuse, explicit rebuild on a
//! model change, crash recovery and the model-unavailable path.

mod common;

use common::{fixture, models_root};
use docmonk_ml::embedder::Embedder;
use docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;
use docmonk_storage::knowledge::ProjectStore;

fn subjects(s: &ProjectStore, b: &str) -> i64 {
    s.count("entities", b).unwrap() + s.count("chunks", b).unwrap()
}

#[test]
fn missing_model_leaves_vectors_pending_until_it_is_installed() {
    let mut fx = fixture();
    let empty = tempfile::tempdir().unwrap();
    // RAGMONK_MODELS_DIR would override the explicit root; this test only
    // runs the unavailable path when it is unset.
    if std::env::var_os("RAGMONK_MODELS_DIR").is_some() {
        return;
    }
    fx.index(empty.path());
    let (s, b) = fx.store();
    assert!(subjects(&s, &b) > 0);
    assert!(s.embeddings(&b).unwrap().is_empty());
}

#[test]
fn every_subject_gets_a_current_vector_and_reruns_reuse_the_cache() {
    let Some(models) = models_root() else { return };
    let mut fx = fixture();
    fx.index(&models);
    let (s, b) = fx.store();
    let fp = DEFAULT_EMBEDDING_MODEL.fingerprint();
    let stored = s.embeddings(&b).unwrap();
    let pending = s.pending_embedding_subjects(&b, &fp).unwrap();
    assert!(pending.is_empty(), "{pending:?}");
    assert!(!stored.is_empty());
    assert!(stored
        .iter()
        .all(|e| e.model_fingerprint == fp && e.vector.len() == DEFAULT_EMBEDDING_MODEL.dims));
    let cache = s.embedding_cache_len().unwrap();
    assert!(cache > 0 && cache <= stored.len() as i64);
    drop(s);

    // Touch one document: only its chunks are re-embedded, and unchanged
    // texts come from the cache (no new cache rows for them).
    let doc = fx.root.join("docs/billing.md");
    let mut text = std::fs::read_to_string(&doc).unwrap();
    text.push_str("\n\n## Appendix\n\nA brand new paragraph about ledgers.\n");
    std::fs::write(&doc, text).unwrap();
    common::bump_mtime(&doc, 5);
    fx.index(&models);
    let (s, b2) = fx.store();
    assert!(s.pending_embedding_subjects(&b2, &fp).unwrap().is_empty());
    let after = s.embeddings(&b2).unwrap();
    let new_cache = s.embedding_cache_len().unwrap();
    assert!(new_cache > cache, "the new paragraph is a new text");
    assert!(new_cache - cache <= 2, "unchanged chunks reuse the cache");
    // Vectors of untouched subjects are identical across builds.
    let touched = s
        .files(&b2)
        .unwrap()
        .into_iter()
        .find(|f| f.rel_path == "docs/billing.md")
        .map(|f| f.id)
        .unwrap();
    for e in stored.iter().filter(|e| e.file_id != touched) {
        let a = after.iter().find(|a| a.subject_id == e.subject_id).unwrap();
        assert_eq!(a.vector, e.vector);
    }
}

#[test]
fn a_model_change_rebuilds_every_vector_explicitly() {
    let Some(models) = models_root() else { return };
    let mut fx = fixture();
    fx.index(&models);
    let (mut s, b) = fx.store();
    let total = s.embeddings(&b).unwrap().len();
    // Same weights, different preprocessing contract => new fingerprint.
    let mut spec = DEFAULT_EMBEDDING_MODEL;
    spec.max_chars = 2000;
    let changed = Embedder::load(&models.join(spec.slug), spec).unwrap();
    let stats = docmonk_ml::embed_build(&mut s, &b, &changed, "2").unwrap();
    assert_eq!(stats.stale_dropped, total);
    assert_eq!(stats.subjects, total);
    let fp = spec.fingerprint();
    let after = s.embeddings(&b).unwrap();
    assert_eq!(after.len(), total);
    assert!(after.iter().all(|e| e.model_fingerprint == fp));
}

#[test]
fn vectors_lost_in_a_crash_are_recomputed_on_the_next_run() {
    let Some(models) = models_root() else { return };
    let mut fx = fixture();
    fx.index(&models);
    let (s, b) = fx.store();
    let total = s.embeddings(&b).unwrap().len();
    s.connection()
        .execute(
            "DELETE FROM embeddings WHERE rowid IN
                (SELECT rowid FROM embeddings WHERE build_id = ?1 LIMIT 5)",
            [&b],
        )
        .unwrap();
    assert_eq!(s.embeddings(&b).unwrap().len(), total - 5);
    drop(s);
    fx.index(&models);
    let (s, b) = fx.store();
    assert_eq!(s.embeddings(&b).unwrap().len(), total);
}
