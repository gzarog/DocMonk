//! Full rebuilds of registered sources.

use std::path::Path;

use docmonk_core::errors::RagMonkError;
use docmonk_core::paths::Home;
use docmonk_service::indexing::{index_sources_with, selected_sources, RunOptions, SourceEvent};
use serde_json::{json, Value};

/// Rebuilds one or every enabled source under the `index` lock and
/// returns one `{id, path, scanned, indexed, failed, linked[, error]}` per
/// source; a source whose pass fails carries `error` and the run moves on.
/// `each` sees every outcome as it completes.
pub fn rebuild_sources(
    home: &Home,
    source_id: Option<&str>,
    fresh: bool,
    mut each: impl FnMut(&Value),
) -> Result<Vec<Value>, RagMonkError> {
    let sources = selected_sources(home, source_id)?;
    if sources.is_empty() {
        return Err(RagMonkError::usage("no sources to rebuild"));
    }
    if fresh {
        let unreachable: Vec<String> = sources
            .iter()
            .filter(|s| !Path::new(&s.path).exists())
            .map(|s| format!("{} ({})", s.id, s.path))
            .collect();
        if !unreachable.is_empty() {
            return Err(RagMonkError::usage(format!(
                "cannot rebuild --fresh: source root(s) not reachable: {}. Reconnect them (or remove the sources) and try again; the existing index was left untouched.",
                unreachable.join(", ")
            )));
        }
    }
    // Same bounded parallel executor as `index`, every source forced to a
    // full rebuild; per-source locks (and, in server mode, writer leases)
    // keep each source's previous build visible until its new one
    // publishes.
    let mut out: Vec<serde_json::Value> = Vec::new();
    index_sources_with(
        home,
        &sources,
        "rebuild",
        &RunOptions {
            force_full: true,
            ..RunOptions::default()
        },
        |event| {
            let o = match event {
                SourceEvent::Started { .. } => return,
                SourceEvent::Dependencies { source, outcome } => {
                    if let Some(row) = out.iter_mut().find(|o| o["id"] == source.id.as_str()) {
                        row["dependencies"] = json!(outcome);
                    }
                    return;
                }
                SourceEvent::Relationships { source, outcome } => {
                    // Phase 2 ends after every rebuild: annotate the row.
                    if let Some(row) = out.iter_mut().find(|o| o["id"] == source.id.as_str()) {
                        if let (Some(row), Some(extra)) =
                            (row.as_object_mut(), outcome.json().as_object())
                        {
                            row.extend(extra.clone());
                        }
                    }
                    return;
                }
                SourceEvent::Completed { source, run } => json!({
                    "id": source.id,
                    "path": source.path,
                    "scanned": run.counts.scanned,
                    "indexed": run.indexed,
                    "failed": run.failed,
                    "linked": run.linked,
                }),
                SourceEvent::Blocked { source, error } | SourceEvent::Failed { source, error } => {
                    json!({
                        "id": source.id,
                        "path": source.path,
                        "scanned": 0,
                        "indexed": 0,
                        "failed": 0,
                        "linked": 0,
                        "error": docmonk_telemetry::redact::redact_urls_in_text(error.message()),
                    })
                }
            };
            each(&o);
            out.push(o);
        },
    )?;
    // Keep the per-source report in registration order.
    let order: Vec<&str> = sources.iter().map(|s| s.id.as_str()).collect();
    out.sort_by_key(|o| order.iter().position(|id| o["id"] == *id));
    Ok(out)
}
