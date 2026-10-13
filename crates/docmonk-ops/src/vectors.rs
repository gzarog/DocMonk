//! ANN index maintenance: rebuild from stored vectors, and backfill
//! vectors that were never computed.

use docmonk_core::errors::RagMonkError;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_service::load;
use docmonk_service::query::open_sources;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

use crate::{dberr, generic, index_lock, require_local};

/// Rebuilds each selected source's ANN index from its stored vectors.
/// One `{source_id, backend, vectors}` per source; `backend` is null when
/// the source has no vectors yet.
pub fn rebuild(home: &Home, source: Option<&str>) -> Result<Vec<Value>, RagMonkError> {
    // Server mode: vectors live in the server indexes and are written by
    // every build (`ragmonk rebuild` recomputes them); there is no local
    // ANN index to maintain.
    require_local(home, "vectors rebuild")?;
    let layout = StorageLayout::new(home);
    let spec = docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;
    let fp = spec.fingerprint();
    let lock = index_lock(home, "vectors-rebuild")?;
    let mut rebuilt = Vec::new();
    for o in open_sources(home, source)? {
        let store = o.store.local()?;
        let n = store.embedding_keys(&o.build, &fp).map_err(dberr)?.len();
        if n == 0 {
            rebuilt.push(json!({"source_id": o.source.id, "backend": null, "vectors": 0}));
            continue;
        }
        let dir = layout.project_dir(&project_id_for_canonical(&o.source.path));
        let _ = std::fs::remove_file(docmonk_ml::ann::index_path(&dir));
        let stats =
            docmonk_ml::ann::sync(store, &dir, &o.build, &fp, spec.dims).map_err(generic)?;
        rebuilt.push(json!({"source_id": o.source.id, "backend": "hnsw", "vectors": stats.live}));
    }
    lock.release();
    Ok(rebuilt)
}

/// Computes missing vectors for each selected source and syncs its ANN
/// index. One `{source_id, embedded}` per source.
pub fn backfill(home: &Home, source: Option<&str>) -> Result<Vec<Value>, RagMonkError> {
    require_local(home, "vectors backfill")?;
    let layout = StorageLayout::new(home);
    let spec = docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;
    let cfg = load(home)?;
    let lazy = docmonk_ml::LazyEmbedder::new(
        docmonk_ml::embedder::models_root(Some(&home.models_dir())),
        spec,
        cfg.indexing.embedding_batch_size,
    );
    let embedder = lazy.get().map_err(|e| {
        RagMonkError::usage(format!(
            "the embedding model is not available ({e}); install it first"
        ))
    })?;
    let lock = index_lock(home, "vectors-backfill")?;
    let mut done = Vec::new();
    for mut o in open_sources(home, source)? {
        let build = o.build.clone();
        let stats = docmonk_ml::embed_build(
            o.store.local_mut()?,
            &build,
            embedder,
            docmonk_documents::chunker::EMBEDDING_TEXT_VERSION,
        )
        .map_err(|e| generic(format!("{}: {}", e.code, e.message)))?;
        let embedded = stats.inferred + stats.cache_reused;
        if embedded > 0 {
            let dir = layout.project_dir(&project_id_for_canonical(&o.source.path));
            docmonk_ml::ann::sync(
                o.store.local()?,
                &dir,
                &o.build,
                embedder.fingerprint(),
                embedder.spec().dims,
            )
            .map_err(generic)?;
        }
        done.push(json!({"source_id": o.source.id, "embedded": embedded}));
    }
    lock.release();
    Ok(done)
}
