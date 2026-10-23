//! Tree-sitter code intelligence: language detection, the
//! query-driven extractor, implementation passages, reference
//! resolution, framework heuristics, the code processor for the indexer,
//! the post-index graph stage and graph traversal.

pub mod extract;
pub mod framework;
pub mod graph;
pub mod graph_stage;
pub mod lang;
pub mod passages;
pub mod process;
pub mod resolve;

use std::sync::Arc;

use docmonk_indexing::coordinator::{RawProcessor, Registry};

/// The registry with Rust-native code extraction only (documents are
/// recorded raw). Code extraction yields entities only: relationships are
/// derived after publication by [`graph_stage`], so the relationship
/// setting never changes this identity.
pub fn registry() -> Registry {
    registry_with_facts(None)
}

/// [`registry`] whose code processor stores graph facts in `facts`
/// (`indexing.graph_fact_cache`; the registry's identity is unchanged).
pub fn registry_with_facts(facts: Option<Arc<docmonk_storage::facts::FactCache>>) -> Registry {
    let mut r = Registry::raw();
    r.code = Arc::new(process::CodeProcessor { facts });
    r.document = Arc::new(RawProcessor);
    r.versions.parser_version = code_parser_version();
    r
}

/// The code files' `parser_version`: the extraction identity and the
/// normalized lexical projection their FTS rows carry (ADR 0039). The
/// graph identity includes only the extraction: entity ids do not depend
/// on the projection.
pub fn code_parser_version() -> String {
    format!(
        "{}+{}",
        process::CODE_DERIVATION_VERSION,
        docmonk_core::lexnorm::LEXICAL_PROJECTION_VERSION
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_registry_has_no_graph_finalizers() {
        let r = registry();
        assert!(r.finalizers.is_empty());
        assert_eq!(r.versions.parser_version, "rust-code-3+lexnorm-1");
    }
}
