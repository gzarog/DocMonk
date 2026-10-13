//! Tree-sitter code intelligence: language detection, the
//! query-driven extractor, reference resolution, framework heuristics,
//! the code processor for the indexer, the post-index graph stage and
//! graph traversal.

pub mod extract;
pub mod framework;
pub mod graph;
pub mod graph_stage;
pub mod lang;
pub mod process;
pub mod resolve;

use std::sync::Arc;

use docmonk_indexing::coordinator::{RawProcessor, Registry};

/// The registry with Rust-native code extraction only (documents are
/// recorded raw). Code extraction yields entities only: relationships are
/// derived after publication by [`graph_stage`], so the relationship
/// setting never changes this identity.
pub fn registry() -> Registry {
    let mut r = Registry::raw();
    r.code = Arc::new(process::CodeProcessor);
    r.document = Arc::new(RawProcessor);
    r.versions.parser_version = process::CODE_DERIVATION_VERSION.into();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_registry_has_no_graph_finalizers() {
        let r = registry();
        assert!(r.finalizers.is_empty());
        assert_eq!(r.versions.parser_version, process::CODE_DERIVATION_VERSION);
    }
}
