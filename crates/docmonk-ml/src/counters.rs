//! Process-wide counters for the query path (`search --explain`, the
//! search benchmark): how often a model or an ANN index was loaded and how
//! often a cached one was reused. Counters only ever grow.

use std::sync::atomic::{AtomicU64, Ordering};

static EMBEDDER_LOADS: AtomicU64 = AtomicU64::new(0);
static RERANKER_LOADS: AtomicU64 = AtomicU64::new(0);
static ANN_LOADS: AtomicU64 = AtomicU64::new(0);
static ANN_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static QUERY_EMBEDS: AtomicU64 = AtomicU64::new(0);

/// A snapshot of every counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counters {
    pub embedder_loads: u64,
    pub reranker_loads: u64,
    pub ann_loads: u64,
    pub ann_cache_hits: u64,
    pub query_embeds: u64,
}

impl Counters {
    /// `self - earlier`, per counter.
    pub fn since(&self, earlier: &Counters) -> Counters {
        Counters {
            embedder_loads: self.embedder_loads - earlier.embedder_loads,
            reranker_loads: self.reranker_loads - earlier.reranker_loads,
            ann_loads: self.ann_loads - earlier.ann_loads,
            ann_cache_hits: self.ann_cache_hits - earlier.ann_cache_hits,
            query_embeds: self.query_embeds - earlier.query_embeds,
        }
    }
}

pub fn snapshot() -> Counters {
    Counters {
        embedder_loads: EMBEDDER_LOADS.load(Ordering::Relaxed),
        reranker_loads: RERANKER_LOADS.load(Ordering::Relaxed),
        ann_loads: ANN_LOADS.load(Ordering::Relaxed),
        ann_cache_hits: ANN_CACHE_HITS.load(Ordering::Relaxed),
        query_embeds: QUERY_EMBEDS.load(Ordering::Relaxed),
    }
}

pub(crate) fn embedder_loaded() {
    EMBEDDER_LOADS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn reranker_loaded() {
    RERANKER_LOADS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn ann_loaded() {
    ANN_LOADS.fetch_add(1, Ordering::Relaxed);
}
#[allow(dead_code)]
pub(crate) fn ann_cache_hit() {
    ANN_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn query_embedded() {
    QUERY_EMBEDS.fetch_add(1, Ordering::Relaxed);
}
