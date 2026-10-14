//! Knowledge linker: cross-domain links between code entities and
//! document chunks, and explicit (manual) links.

pub mod canonical;
pub mod linker;
pub mod manual;
pub mod matcher;

use docmonk_code::graph_stage::{GraphLinker, LinkStats};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageError;

/// The graph stage's linker: links the touched files, then re-applies
/// manual link intent. It never runs while the base index is built.
pub struct KnowledgeLinker;

impl GraphLinker for KnowledgeLinker {
    fn link(
        &self,
        store: &mut ProjectStore,
        build_id: &str,
        touched: &[String],
        _full: bool,
    ) -> Result<LinkStats, StorageError> {
        let started = std::time::Instant::now();
        let (candidates, mut stats) =
            linker::link_touched_files_with_stats(store, build_id, touched)?;
        let writing = std::time::Instant::now();
        let rows: Vec<_> = candidates.iter().map(linker::Candidate::row).collect();
        for batch in rows.chunks(linker::LINK_BATCH_SIZE) {
            docmonk_indexing::progress::heartbeat();
            store.put_links(build_id, batch)?;
        }
        stats.write_seconds = writing.elapsed().as_secs_f64();
        let applying = std::time::Instant::now();
        let manual = manual::apply(store, build_id)?;
        stats.manual_seconds = applying.elapsed().as_secs_f64();
        stats.links = rows.len();
        stats.manual = manual;
        tracing::info!(
            component = "linker",
            event = "links_published",
            links = rows.len(),
            manual,
            seconds = started.elapsed().as_secs_f64()
        );
        Ok(stats)
    }
}

/// Builds (or confirms) the relationship graph of a source's published
/// `build_id`: code relationships, cross-file resolution, code-document
/// links and manual link projection (see [`docmonk_code::graph_stage`]).
pub fn build_graph(
    store: &mut ProjectStore,
    root: &std::path::Path,
    build_id: &str,
    workers: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: &mut docmonk_code::graph_stage::GraphProgressFn<'_>,
) -> Result<docmonk_code::graph_stage::GraphReport, docmonk_code::graph_stage::GraphError> {
    build_graph_phased(store, root, build_id, workers, cancel, progress, None, None)
}

/// [`build_graph`], also reporting each phase the build enters and
/// reusing graph facts stored by the base index.
#[allow(clippy::too_many_arguments)]
pub fn build_graph_phased(
    store: &mut ProjectStore,
    root: &std::path::Path,
    build_id: &str,
    workers: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: &mut docmonk_code::graph_stage::GraphProgressFn<'_>,
    phase: Option<&docmonk_code::graph_stage::GraphPhaseFn<'_>>,
    facts: Option<&docmonk_storage::facts::FactCache>,
) -> Result<docmonk_code::graph_stage::GraphReport, docmonk_code::graph_stage::GraphError> {
    docmonk_code::graph_stage::build_graph(
        store,
        root,
        build_id,
        &docmonk_code::graph_stage::GraphOptions {
            workers,
            linker: Some(&KnowledgeLinker),
            cancel,
            phase,
            facts,
            batch: 0,
        },
        progress,
    )
}
