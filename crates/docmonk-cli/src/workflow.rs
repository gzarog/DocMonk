//! Core workflow commands: `init`, `source …`, `index` and `docs`.

use clap::Subcommand;
use docmonk_core::errors::{DocMonkError, ErrorKind};
use serde_json::{json, Value};

use docmonk_service::indexing::{index_sources, selected_sources, SourceEvent};
use docmonk_service::sources::{
    assert_no_active_daemon, catalog, docs_rows, project_data_dir, remove_source,
};

use crate::{load, prepared_home, print_json};

fn generic(e: impl std::fmt::Display) -> DocMonkError {
    DocMonkError::new(ErrorKind::Generic, e.to_string())
}

/// Plain aligned table: a header row, then rows, columns separated by two
/// spaces.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(c.chars().count());
            }
        }
    }
    let line = |cells: Vec<&str>| {
        let s: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(headers.to_vec());
    for r in rows {
        line(r.iter().map(String::as_str).collect());
    }
}

// ---------------------------------------------------------------- init ---

#[derive(clap::Args)]
pub struct InitArgs {
    /// Storage backend mode for a *new* config: 'local' (default) or 'server'.
    #[arg(long = "storage-mode", default_value = "local")]
    storage_mode: String,
    /// Shorthand for --storage-mode=local (the default; explicit opt-in).
    #[arg(long)]
    local: bool,
    /// Server engine when --storage-mode=server: 'opensearch' or 'elasticsearch'.
    #[arg(long = "storage-engine", default_value = "opensearch")]
    storage_engine: String,
    /// Server URL when --storage-mode=server.
    #[arg(long = "storage-url", default_value = "")]
    storage_url: String,
    /// Index name prefix when --storage-mode=server.
    #[arg(long = "storage-index-prefix", default_value = "docmonk")]
    storage_index_prefix: String,
    /// Do not verify TLS certificates when --storage-mode=server.
    #[arg(long = "storage-no-verify-tls")]
    storage_no_verify_tls: bool,
}

fn validated_server_config(
    a: &InitArgs,
) -> Result<docmonk_config::model::ServerStorageConfig, DocMonkError> {
    if a.storage_url.is_empty() {
        return Err(DocMonkError::usage(
            "--storage-url is required when --storage-mode=server",
        ));
    }
    if docmonk_telemetry::redact::url_has_userinfo(&a.storage_url) {
        return Err(DocMonkError::usage(
            "--storage-url must not contain credentials (user-info such as 'user:password@'); \
             set DOCMONK_OPENSEARCH_USERNAME/_PASSWORD/_API_KEY or \
             DOCMONK_ELASTICSEARCH_USERNAME/_PASSWORD/_API_KEY instead; \
             no config file was written",
        ));
    }
    if !matches!(a.storage_engine.as_str(), "opensearch" | "elasticsearch") {
        return Err(DocMonkError::usage(format!(
            "unknown --storage-engine {:?}; expected 'opensearch' or 'elasticsearch'",
            a.storage_engine
        )));
    }
    let mut cfg = docmonk_config::DocMonkConfig::default();
    let server = &mut cfg.storage.server;
    server.engine = if a.storage_engine == "elasticsearch" {
        docmonk_config::model::StorageEngine::Elasticsearch
    } else {
        docmonk_config::model::StorageEngine::OpenSearch
    };
    server.url = a.storage_url.clone();
    server.index_prefix = a.storage_index_prefix.clone();
    server.verify_tls = !a.storage_no_verify_tls;
    let server = cfg.storage.server;
    let redacted = docmonk_telemetry::redact::redact_urls_in_text(&a.storage_url);
    docmonk_backends::ServerBackend::connect(&server, None).map_err(|e| {
        DocMonkError::usage(format!(
            "storage validation failed: {}; no config file was written",
            docmonk_telemetry::redact::redact_urls_in_text(&e.to_string())
        ))
    })?;
    println!(
        "Storage validation OK -- {} at {redacted}",
        a.storage_engine
    );
    Ok(server)
}

pub fn init(a: &InitArgs) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let config_path = home.user_config();
    if config_path.is_file() {
        // Validates the existing config; errors surface early.
        load(&home)?;
        println!("Using existing configuration at {}", config_path.display());
    } else {
        if a.local && a.storage_mode != "local" {
            return Err(DocMonkError::usage(
                "--local conflicts with --storage-mode=server",
            ));
        }
        let mut cfg = docmonk_config::DocMonkConfig::default();
        match a.storage_mode.as_str() {
            "local" => {}
            "server" => {
                cfg.storage.mode = "server".into();
                cfg.storage.server = validated_server_config(a)?;
            }
            other => {
                return Err(DocMonkError::usage(format!(
                    "unknown --storage-mode {other:?}; expected 'local' or 'server'"
                )))
            }
        }
        docmonk_config::write_user_config(&cfg, &home).map_err(generic)?;
        println!("Wrote default configuration to {}", config_path.display());
    }
    // Creates the local control plane, or (server mode) the server
    // indexes; an unreachable server fails init instead of silently
    // creating local state.
    let c = catalog(&home)?;
    if c.is_server() {
        println!("Server indexes are ready (storage.mode = server).");
    }
    println!("DocMonk initialized at {}", home.root().display());
    Ok(())
}

// -------------------------------------------------------------- source ---

#[derive(Subcommand)]
pub enum SourceCommand {
    /// Register a directory as a source.
    Add {
        path: String,
        /// Include glob pattern.
        #[arg(long)]
        include: Vec<String>,
        /// Exclude glob pattern.
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// List registered sources.
    List,
    /// Show one source as JSON.
    Info { source_id: String },
    /// Enable a source.
    Enable { source_id: String },
    /// Disable a source.
    Disable { source_id: String },
    /// Remove a source and its indexed data (never the original files).
    Remove {
        source_id: String,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt} [y/N]: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn source(cmd: SourceCommand) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let mut cp = catalog(&home)?;
    match cmd {
        SourceCommand::Add {
            path,
            include,
            exclude,
        } => {
            let s = cp.add(&path, include, exclude)?;
            println!("Added source {} -> {}", s.id, s.path);
        }
        SourceCommand::List => {
            let mut rows = Vec::new();
            for s in cp.list(false)? {
                let state = cp.summary(&s.id)?;
                rows.push(vec![
                    s.id.clone(),
                    s.path.clone(),
                    s.source_type.as_str().into(),
                    if s.enabled { "yes" } else { "no" }.into(),
                    state.online_status.clone(),
                    state.last_scan_at.clone().unwrap_or_else(|| "-".into()),
                ]);
            }
            print_table(
                &["ID", "Path", "Type", "Enabled", "Status", "Last Scan"],
                &rows,
            );
        }
        SourceCommand::Info { source_id } => {
            let s = cp.get(&source_id)?;
            let v = cp.info(&s)?;
            println!("{}", serde_json::to_string_pretty(&v).map_err(generic)?);
        }
        SourceCommand::Enable { source_id } => {
            cp.set_enabled(&source_id, true)?;
            println!("Enabled {source_id}");
        }
        SourceCommand::Disable { source_id } => {
            cp.set_enabled(&source_id, false)?;
            println!("Disabled {source_id}");
        }
        SourceCommand::Remove { source_id, yes } => {
            let s = cp.get(&source_id)?;
            assert_no_active_daemon(&home)?;
            let project_dir = if cp.is_server() {
                std::path::PathBuf::from(format!("server indexes ({} records)", s.id))
            } else {
                project_data_dir(&home, &s)
            };
            if !yes {
                println!("This will permanently remove:");
                println!("  - source {} ({}) from the registry", s.id, s.path);
                println!("  - its indexed data at {}", project_dir.display());
                println!("The original files at the source path are never touched.");
                if !confirm("Proceed?") {
                    println!("Aborted; nothing was removed.");
                    return Ok(());
                }
            }
            let deleted = remove_source(&home, &source_id)?;
            println!("Removed {} ({})", s.id, s.path);
            if deleted {
                println!("  deleted indexed data at {}", project_dir.display());
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------------- index ---

// --------------------------------------------------------------- index ---

pub fn index(source_id: Option<String>) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let sources = selected_sources(&home, source_id.as_deref())?;
    if sources.is_empty() {
        println!("No enabled sources to index.");
        return Ok(());
    }
    let summary = index_sources(&home, &sources, "index", |event| match event {
        SourceEvent::Started { .. } => {}
        SourceEvent::Blocked { source, error } => {
            println!(
                "{} {}: blocked: {}",
                source.id,
                source.path,
                error.message()
            );
        }
        SourceEvent::Failed { source, error } => println!(
            "{} {}: source pass failed: {}",
            source.id,
            source.path,
            docmonk_telemetry::redact::redact_urls_in_text(error.message())
        ),
        SourceEvent::Completed { source, run: r } => {
            if let Some(reason) = &r.offline {
                println!(
                    "{} {}: source unreachable ({reason}); marked OFFLINE, skipped deletion reconciliation",
                    source.id, source.path
                );
                return;
            }
            if r.became_online {
                println!(
                    "{} {}: source reachable again; back to ACTIVE",
                    source.id, source.path
                );
            }
            let c = &r.counts;
            println!(
                "{} {}: scanned={} new={} changed={} unchanged={} moved={} deleted={} \
                 indexed={} skipped={} failed={} linked={} embedded={}",
                source.id,
                source.path,
                c.scanned,
                c.new,
                c.changed,
                c.unchanged,
                c.moved,
                c.deleted,
                r.indexed,
                r.skipped_limit,
                r.failed,
                r.linked,
                r.embedded
            );
            let a = &r.attachments;
            if a.seen > 0 {
                println!(
                    "{}: email attachments seen={} indexed={} skipped={} failed={}",
                    source.id, a.seen, a.indexed, a.skipped, a.failed
                );
            }
            if !r.scan_errors.is_empty() {
                println!(
                    "{}: scan was incomplete ({} unreadable path(s)); deletion reconciliation skipped this pass, will retry",
                    source.id,
                    r.scan_errors.len()
                );
            }
        }
        SourceEvent::Relationships { source, outcome } => print_relationships(&source.id, outcome),
        SourceEvent::Dependencies { source, outcome } => print_dependencies(&source.id, outcome),
    })?;
    let (attempted, failed) = (summary.attempted, summary.failed_sources);
    if failed > 0 {
        println!(
            "Index complete with failures: {attempted} source(s) attempted, {} completed, {failed} source(s) failed.",
            attempted - failed
        );
    } else {
        println!(
            "Index complete: {attempted} source(s) processed, {attempted} succeeded, 0 failed."
        );
    }
    print_relationship_totals(&summary);
    summary.into_result().map(drop)
}

/// One source's phase-2 line (`relationships: ...`), separate from its
/// indexing line.
fn print_dependencies(source_id: &str, outcome: &docmonk_service::dependencies::DependencyOutcome) {
    use docmonk_service::dependencies::DependencyOutcome as O;
    match outcome {
        O::Current => {}
        O::Updated {
            references,
            bound,
            triggered_by,
            ..
        } => println!(
            "{source_id}: cross-source dependencies updated ({references} reference(s) looked up, {bound} bound; triggered by {})",
            triggered_by.join(", ")
        ),
        O::Stale { reason } | O::Pending { reason } => println!(
            "{source_id}: cross-source dependencies {}: {reason}",
            outcome.state()
        ),
    }
}

fn print_relationships(
    source_id: &str,
    outcome: &docmonk_service::relationships::RelationshipOutcome,
) {
    use docmonk_service::relationships::RelationshipOutcome as O;
    match outcome {
        O::Built(r) | O::UpToDate(r) => {
            let g = &r.graph;
            println!(
                "{source_id}: relationships {} (generation {}, {}: {}, {} file(s) recomputed{}, {} edge(s), {} link(s))",
                outcome.state(),
                g.generation,
                g.action.as_str(),
                g.reason,
                g.files_processed,
                if g.full { ", full" } else { "" },
                g.relationships_written,
                g.links
            );
        }
        O::Skipped(reason) => println!("{source_id}: relationships skipped: {reason}"),
        other => println!(
            "{source_id}: relationships {}: {} (the index is published and searchable)",
            other.state(),
            other.error().unwrap_or_default()
        ),
    }
}

fn print_relationship_totals(summary: &docmonk_service::indexing::RunSummary) {
    if !summary.relationships_enabled {
        println!("Relationships: disabled (indexing.relationships_enabled: false).");
        return;
    }
    let r = &summary.relationships;
    if r.attempted == 0 {
        return;
    }
    println!(
        "Relationships: {} source(s) attempted, {} built, {} up to date, {} stale, {} failed.",
        r.attempted, r.built, r.up_to_date, r.stale, r.failed
    );
    println!("  {}", graph_work(r));
    let d = &summary.dependencies;
    if d.consumers > 0 {
        println!(
            "  Cross-source dependencies: {} consumer(s), {} current, {} updated, {} stale, {} pending.",
            d.consumers, d.current, d.updated, d.stale, d.pending
        );
    }
}

/// One line of graph work by planned action.
fn graph_work(r: &docmonk_service::relationships::StageSummary) -> String {
    format!(
        "Graphs: {} skipped, {} rebound, {} incremental, {} full, {} retried; {} file(s) derived, {} re-resolved, {} relationship write(s).",
        r.graph_skipped,
        r.graph_rebound,
        r.graph_incremental,
        r.graph_full,
        r.graph_retried,
        r.files_processed,
        r.files_reresolved,
        r.relationships_written
    )
}

// ------------------------------------------------------- relationships ---

/// `docmonk relationships build [--source ID]`: the graph stage alone.
pub fn relationships_build(
    source_id: Option<String>,
    json_output: bool,
) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let sources = selected_sources(&home, source_id.as_deref())?;
    if sources.is_empty() {
        println!("No enabled sources.");
        return Ok(());
    }
    let mut rows = Vec::new();
    let summary = docmonk_service::relationships::build(&home, &sources, |source, outcome| {
        if json_output {
            let mut row = outcome.json();
            row["source_id"] = serde_json::json!(source.id);
            rows.push(row);
        } else {
            print_relationships(&source.id, outcome);
        }
    })?;
    if json_output {
        crate::print_json(&serde_json::json!({
            "outcome": if summary.not_current() == 0 { "success" } else { "partial_success" },
            "summary": summary,
            "sources": rows,
        }))?;
    } else {
        println!(
            "Relationships: {} source(s) attempted, {} built, {} up to date, {} stale, {} failed, {} skipped.",
            summary.attempted,
            summary.built,
            summary.up_to_date,
            summary.stale,
            summary.failed,
            summary.skipped
        );
        println!("  {}", graph_work(&summary));
    }
    if summary.not_current() > 0 {
        return Err(DocMonkError::new(
            docmonk_core::errors::ErrorKind::IndexingPartialFailure,
            format!(
                "relationships were not published for {} source(s); see 'docmonk status' for details",
                summary.not_current()
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- docs ---

pub fn docs(source_id: Option<String>, json_output: bool) -> Result<(), DocMonkError> {
    let home = prepared_home()?;
    let rows = docs_rows(&home, source_id.as_deref())?;
    if json_output {
        return print_json(&json!({ "documents": rows }));
    }
    if rows.is_empty() {
        println!("No indexed documents.");
        return Ok(());
    }
    let s = |v: &Value| v.as_str().map_or_else(|| "-".to_owned(), str::to_owned);
    let mut table = Vec::new();
    for r in &rows {
        table.push(vec![
            s(&r["path"]),
            s(&r["format"]),
            s(&r["status"]),
            r["page_count"]
                .as_i64()
                .map_or_else(|| "-".to_owned(), |n| n.to_string()),
            r["section_count"].to_string(),
            s(&r["title"]),
        ]);
        for a in r["attachments"].as_array().into_iter().flatten() {
            table.push(vec![
                format!("  -> {}", a.as_str().unwrap_or_default()),
                "attachment".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ]);
        }
    }
    print_table(
        &["Path", "Format", "Status", "Pages", "Sections", "Title"],
        &table,
    );
    Ok(())
}
