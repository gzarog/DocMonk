//! Retrieval for DocMonk.
//!
//! * [`lexical`]: exact / qualified / alias symbol lookups, document title
//!   and FTS hits, and path hits, tiered and merged in a fixed
//!   tier order.
//! * [`hybrid`]: merge and rerank: lexical and
//!   semantic candidates deduplicated per `(kind, id)`. Exact matches are
//!   pinned; everything else is ordered by Reciprocal Rank Fusion. An
//!   optional cross-encoder pass reorders the top of the list.
//!
//! Search runs over one or more [`Corpus`] values (a project store plus the
//! build to read), so multi-source search is the caller's list.

pub mod classify;
pub mod context;
pub mod decompose;
pub mod diversify;
pub mod explore;
pub mod graph;
pub mod grounding;
pub mod hybrid;
pub mod lexical;
pub mod route;

use docmonk_storage::read::KnowledgeRead;

/// One searchable source: its knowledge reader (local SQLite store or a
/// server snapshot) and the build to read (normally the active build).
#[derive(Clone, Copy)]
pub struct Corpus<'a> {
    pub store: &'a dyn KnowledgeRead,
    pub build_id: &'a str,
}

impl std::fmt::Debug for Corpus<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Corpus")
            .field("source_id", &self.store.source_id())
            .field("build_id", &self.build_id)
            .finish()
    }
}

/// A search failure (storage error); "nothing found" is an empty result.
#[derive(Debug, thiserror::Error)]
#[error("search failed: {0}")]
pub struct SearchError(pub String);

/// `Err` once the calling request's deadline has passed (F09): retrieval
/// loops call this between units of work so cancelled requests stop
/// scheduling further reads.
pub fn check_deadline() -> Result<(), SearchError> {
    docmonk_core::deadline::check().map_err(|m| SearchError(m.into()))
}

impl From<docmonk_storage::error::StorageError> for SearchError {
    fn from(e: docmonk_storage::error::StorageError) -> Self {
        Self(e.to_string())
    }
}
