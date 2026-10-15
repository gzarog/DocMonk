//! Local ML for DocMonk: pinned model assets, pure-Rust
//! (Candle) embeddings and the build finalizer that keeps every build's
//! vectors complete under the current model.

pub mod ann;
pub mod bert;
pub mod counters;
pub mod embedder;
pub mod fusion;
pub mod hnsw;
pub mod manifest;
pub mod reranker;
pub mod semantic;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use docmonk_indexing::coordinator::{BuildFinalizer, ProcessError};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::vectors::{EmbeddingRow, SUBJECT_ENTITY};
use sha2::{Digest, Sha256};

use embedder::Embedder;
use manifest::{EmbeddingModelSpec, CODE_EMBEDDING_TEXT_VERSION};

/// Texts embedded (and written) per window; bounds memory independently of
/// build size.
pub const EMBED_WINDOW: usize = 256;

/// Exact-text cache key (sha256 hex of the text as stored, before capping).
pub fn text_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Loads the embedder on first use; a load failure is logged once and
/// leaves semantic vectors pending (indexing never fails because the model
/// is unavailable).
pub struct LazyEmbedder {
    dir: PathBuf,
    spec: EmbeddingModelSpec,
    batch_size: i64,
    cell: OnceLock<Result<Arc<Embedder>, String>>,
}

impl LazyEmbedder {
    /// `models_root/<spec.slug>`.
    pub fn new(models_root: PathBuf, spec: EmbeddingModelSpec, batch_size: i64) -> Self {
        Self {
            dir: models_root.join(spec.slug),
            spec,
            batch_size,
            cell: OnceLock::new(),
        }
    }

    pub fn get(&self) -> Result<&Arc<Embedder>, &str> {
        self.cell
            .get_or_init(|| {
                Embedder::load(&self.dir, self.spec)
                    .map(|e| Arc::new(e.with_batch_size(self.batch_size)))
                    .map_err(|e| {
                        tracing::warn!(
                            component = "embedder",
                            event = "model_unavailable",
                            error = %e
                        );
                        e.to_string()
                    })
            })
            .as_ref()
            .map_err(String::as_str)
    }
}

impl LazyEmbedder {
    /// The model was tried and could not be loaded.
    pub fn failed(&self) -> bool {
        matches!(self.cell.get(), Some(Err(_)))
    }
}

type SharedEmbedders = Mutex<HashMap<(PathBuf, i64), Arc<LazyEmbedder>>>;

/// The process-wide embedder for `models_root/<spec.slug>` at
/// `batch_size`: loaded on first use and reused by every later request,
/// so a long-lived process (MCP, UI, daemon) loads the model once. A
/// failed load is retried on the next call (the model may have been
/// installed since).
pub fn shared_embedder(
    models_root: PathBuf,
    spec: EmbeddingModelSpec,
    batch_size: i64,
) -> Arc<LazyEmbedder> {
    static CACHE: OnceLock<SharedEmbedders> = OnceLock::new();
    let key = (models_root.join(spec.slug), batch_size);
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    match cache.get(&key) {
        Some(e) if !e.failed() => e.clone(),
        _ => {
            let e = Arc::new(LazyEmbedder::new(models_root, spec, batch_size));
            cache.insert(key, e.clone());
            e
        }
    }
}

/// The process-wide cross-encoder for `dir`, loaded once; `None` while it
/// cannot be loaded (retried on the next call).
pub fn shared_cross_encoder(
    dir: &std::path::Path,
    spec: manifest::RerankerModelSpec,
) -> Option<Arc<reranker::CrossEncoder>> {
    type Encoders = Mutex<HashMap<PathBuf, Arc<reranker::CrossEncoder>>>;
    static CACHE: OnceLock<Encoders> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(e) = cache.get(dir) {
        return Some(e.clone());
    }
    let e = Arc::new(reranker::CrossEncoder::load(dir, spec).ok()?);
    cache.insert(dir.to_path_buf(), e.clone());
    Some(e)
}

/// Embeds every entity and chunk of the build that lacks a vector under the
/// current model fingerprint.
pub struct EmbeddingFinalizer {
    embedder: LazyEmbedder,
    document_text_version: String,
}

impl EmbeddingFinalizer {
    pub fn new(embedder: LazyEmbedder) -> Self {
        Self {
            embedder,
            document_text_version: docmonk_documents::chunker::EMBEDDING_TEXT_VERSION.into(),
        }
    }
}

/// Outcome of one embedding pass (for telemetry and tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedStats {
    pub subjects: usize,
    pub unique_texts: usize,
    pub cache_reused: usize,
    pub inferred: usize,
    pub stale_dropped: usize,
}

/// Brings `build_id`'s vectors up to date with `embedder`.
pub fn embed_build(
    store: &mut ProjectStore,
    build_id: &str,
    embedder: &Embedder,
    document_text_version: &str,
) -> Result<EmbedStats, ProcessError> {
    let storage = |e: docmonk_storage::StorageError| ProcessError {
        code: "embedding_error".into(),
        message: e.to_string(),
        transient: false,
    };
    let fingerprint = embedder.fingerprint().to_owned();
    let mut stats = EmbedStats::default();
    let stale = store
        .stale_embedding_count(build_id, &fingerprint)
        .map_err(storage)?;
    if stale > 0 {
        tracing::info!(
            component = "embedder",
            event = "model_changed_rebuild",
            stale,
            fingerprint = %fingerprint
        );
        stats.stale_dropped = store
            .delete_stale_embeddings(build_id, &fingerprint)
            .map_err(storage)?;
    }
    let subjects = store
        .pending_embedding_subjects(build_id, &fingerprint)
        .map_err(storage)?;
    stats.subjects = subjects.len();
    for window in subjects.chunks(EMBED_WINDOW) {
        docmonk_indexing::progress::heartbeat();
        let keyed: Vec<(String, String)> = window
            .iter()
            .map(|s| {
                let version = if s.subject_type == SUBJECT_ENTITY {
                    CODE_EMBEDDING_TEXT_VERSION
                } else {
                    document_text_version
                };
                (text_hash(&s.text), version.to_owned())
            })
            .collect();
        let mut unique: Vec<(String, String)> = Vec::new();
        let mut texts: HashMap<(String, String), &str> = HashMap::new();
        for (key, s) in keyed.iter().zip(window) {
            if texts.insert(key.clone(), s.text.as_str()).is_none() {
                unique.push(key.clone());
            }
        }
        stats.unique_texts += unique.len();
        let mut vectors = store
            .cached_embeddings(&fingerprint, &unique)
            .map_err(storage)?;
        stats.cache_reused += vectors.len();
        let missing: Vec<&(String, String)> = unique
            .iter()
            .filter(|k| !vectors.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            let inputs: Vec<&str> = missing.iter().map(|k| texts[*k]).collect();
            // Model batches are bounded process-wide (governor order:
            // embedding < cpu), whatever number of sources is indexing.
            let gov = docmonk_indexing::governor::global();
            let _model = gov.acquire(docmonk_indexing::governor::Class::Embedding, 1);
            let _cpu = gov.acquire(docmonk_indexing::governor::Class::Cpu, 1);
            let computed = embedder.embed(&inputs).map_err(|e| ProcessError {
                code: "embedding_error".into(),
                message: e.to_string(),
                transient: false,
            })?;
            stats.inferred += computed.len();
            for (k, v) in missing.into_iter().zip(computed) {
                vectors.insert(k.clone(), v);
            }
        }
        let rows: Vec<EmbeddingRow> = window
            .iter()
            .zip(&keyed)
            .map(|(s, key)| EmbeddingRow {
                subject_type: s.subject_type,
                subject_id: s.subject_id.clone(),
                file_id: s.file_id.clone(),
                text_hash: key.0.clone(),
                text_version: key.1.clone(),
                vector: vectors[key].clone(),
            })
            .collect();
        store
            .put_embeddings(build_id, &fingerprint, &rows)
            .map_err(storage)?;
    }
    Ok(stats)
}

impl BuildFinalizer for EmbeddingFinalizer {
    fn finalize(
        &self,
        store: &mut ProjectStore,
        build_id: &str,
        _touched: &[String],
    ) -> Result<docmonk_indexing::coordinator::FinalizeReport, ProcessError> {
        Ok(docmonk_indexing::coordinator::FinalizeReport {
            linked: 0,
            embedded: self.run(store, build_id)?,
        })
    }

    fn on_warm_pass(
        &self,
        store: &mut ProjectStore,
        build_id: &str,
    ) -> Result<docmonk_indexing::coordinator::FinalizeReport, ProcessError> {
        Ok(docmonk_indexing::coordinator::FinalizeReport {
            linked: 0,
            embedded: self.run(store, build_id)?,
        })
    }
}

impl EmbeddingFinalizer {
    /// Returns the number of vectors computed (cache reuse included).
    fn run(&self, store: &mut ProjectStore, build_id: &str) -> Result<usize, ProcessError> {
        let Ok(embedder) = self.embedder.get() else {
            return Ok(0);
        };
        let started = std::time::Instant::now();
        let stats = embed_build(store, build_id, embedder, &self.document_text_version)?;
        if let Some(dir) = store.project_dir() {
            match ann::sync(
                store,
                &dir,
                build_id,
                embedder.fingerprint(),
                embedder.spec().dims,
            ) {
                Ok(s) => tracing::info!(
                    component = "ann",
                    event = "index_synced",
                    rebuilt = s.rebuilt,
                    added = s.added,
                    removed = s.removed,
                    compacted = s.compacted,
                    live = s.live
                ),
                // The index is a cache: queries fall back to exact search.
                Err(e) => {
                    tracing::warn!(component = "ann", event = "index_sync_failed", error = %e)
                }
            }
        }
        tracing::info!(
            component = "embedder",
            event = "embeddings_published",
            subjects = stats.subjects,
            unique = stats.unique_texts,
            cache_reused = stats.cache_reused,
            inferred = stats.inferred,
            stale_dropped = stats.stale_dropped,
            seconds = started.elapsed().as_secs_f64()
        );
        Ok(stats.inferred + stats.cache_reused)
    }
}
