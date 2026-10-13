//! What each Admin UI page renders, in the shapes its templates
//! expect. Built from the same functions
//! the CLI uses (status, query, doctor, ops), never a second copy.

use std::path::Path;

use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_retrieval::graph::{self, Direction};
use docmonk_storage::control::SourceRecord;
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

use docmonk_ops::doctor as doctor_cmd;
use docmonk_service::load;
use docmonk_service::query::{self as query_cmd, open_sources, Opened};
use docmonk_service::sources::catalog;
use docmonk_status::model::{Liveness, SourceStatus};
use docmonk_status::StatusReport;
use docmonk_storage::knowledge::{DocumentRow, FileRow};
use docmonk_storage::read::KnowledgeRead;

fn db(e: impl std::fmt::Display) -> DocMonkError {
    DocMonkError::new(ErrorKind::Database, e.to_string())
}

fn abs(root: &str, rel: &str) -> String {
    Path::new(root).join(rel).to_string_lossy().into_owned()
}

// ------------------------------------------------------------ status ---

/// The canonical status report (the same data as `docmonk status --json`).
pub fn report(home: &Home) -> Result<StatusReport, DocMonkError> {
    docmonk_service::status::report(home)
}

fn to_value(v: &impl serde::Serialize) -> Result<Value, DocMonkError> {
    serde_json::to_value(v).map_err(|e| DocMonkError::new(ErrorKind::Generic, e.to_string()))
}

/// The daemon's heartbeat snapshot.
pub fn daemon_snapshot(home: &Home) -> Value {
    let mut v = docmonk_indexing::daemon::status_payload(home);
    if let Some(o) = v.as_object_mut() {
        o.remove("uptime_seconds");
    }
    v
}

/// Dashboard data: the report plus the daemon snapshot.
pub fn dashboard(home: &Home) -> Result<Value, DocMonkError> {
    let mut r = report(home)?;
    r.recent_errors.truncate(10);
    Ok(json!({"status": to_value(&r)?, "daemon": daemon_snapshot(home)}))
}

// ----------------------------------------------------------- sources ---

fn summarize(s: &SourceRecord, row: Option<&SourceStatus>) -> Value {
    let p = row.and_then(|r| r.published.as_ref());
    let live = row
        .and_then(|r| r.live.as_ref())
        .filter(|l| l.liveness == Liveness::Live);
    json!({
        "id": s.id,
        "path": s.path,
        "type": s.source_type.as_str(),
        "enabled": s.enabled,
        "access": row.map(|r| r.access),
        "index_state": row.map(|r| r.index_state),
        "include_patterns": s.include_patterns,
        "exclude_patterns": s.exclude_patterns,
        "last_scan_at": row.and_then(|r| r.last_scan_at.clone()),
        "last_error": row.and_then(|r| r.last_error.clone()),
        "last_error_at": row.and_then(|r| r.last_error_at.clone()),
        "published": p,
        "live": live,
        "warnings": row.map(|r| r.warnings.clone()).unwrap_or_default(),
    })
}

/// Every registered source with its status row.
pub fn list_sources(home: &Home) -> Result<Vec<Value>, DocMonkError> {
    let r = report(home)?;
    let cp = catalog(home)?;
    Ok(cp
        .list(false)?
        .iter()
        .map(|s| summarize(s, r.source(&s.id)))
        .collect())
}

/// One source: its status row, published counters and its newest errors.
pub fn source_detail(home: &Home, source_id: &str) -> Result<Value, DocMonkError> {
    let s = catalog(home)?.get(source_id)?;
    let r = docmonk_service::status::report_with(home, |o| {
        o.error_source = Some(source_id.to_owned());
    })?;
    let mut detail = summarize(&s, r.source(&s.id));
    detail["errors"] = json!(r
        .recent_errors
        .iter()
        .map(|e| json!({
            "path": e.path.as_deref().map(|p| abs(&s.path, p)),
            "code": e.code,
            "message": e.message,
            "occurred_at": e.occurred_at,
        }))
        .collect::<Vec<_>>());
    Ok(detail)
}

// ---------------------------------------------------------- indexing ---

/// Indexing page data (without the UI runner state, which the router adds).
pub fn indexing_overview(home: &Home) -> Result<Value, DocMonkError> {
    let r = report(home)?;
    Ok(json!({
        "summary": r.summary,
        "workers": r.indexer.workers,
        "scope": r.indexer.scope,
        "health": r.health,
    }))
}

/// `index_service.failed_files`.
pub fn failed_files(home: &Home) -> Result<Vec<Value>, DocMonkError> {
    let mut out = Vec::new();
    for o in open_sources(home, None)? {
        let (times, files) = file_listing(&o)?;
        for f in files {
            if f.status == "failed" {
                out.push(json!({
                    "file_id": f.id,
                    "source_id": o.source.id,
                    "path": abs(&o.source.path, &f.rel_path),
                    "error": f.last_error,
                    "updated_at": times.get(&f.id).and_then(|t| t.1.clone()),
                }));
            }
        }
    }
    Ok(out)
}

// --------------------------------------------------------- documents ---

fn attachment_info(d: &docmonk_storage::knowledge::DocumentRow) -> Value {
    match &d.attachment {
        Some(a) => json!({
            "name": a.name,
            "content_type": a.content_type,
            "index": a.index,
            "parent_document_id": a.parent_document_id,
        }),
        None => Value::Null,
    }
}

type Times = std::collections::BTreeMap<String, docmonk_storage::status::FileTimes>;

fn sdb(e: docmonk_storage::StorageError) -> DocMonkError {
    e.into()
}

/// `(file times, files)` of a source's build, from either backend.
fn file_listing(o: &Opened) -> Result<(Times, Vec<FileRow>), DocMonkError> {
    match o.store.server() {
        Some(server) => Ok((Times::new(), server.file_rows(&o.build).map_err(sdb)?)),
        None => {
            let store = o.store.local()?;
            Ok((
                store.file_times(&o.build).map_err(db)?,
                store.files(&o.build).map_err(db)?,
            ))
        }
    }
}

type KindCounts = std::collections::HashMap<String, (i64, i64, i64)>;

fn documents_and_counts(o: &Opened) -> Result<(Vec<DocumentRow>, KindCounts), DocMonkError> {
    match o.store.server() {
        Some(server) => Ok((
            server.document_rows(&o.build).map_err(sdb)?,
            server.chunk_kind_counts(&o.build).map_err(sdb)?,
        )),
        None => {
            let store: &ProjectStore = o.store.local()?;
            Ok((
                store.documents(&o.build).map_err(db)?,
                store.chunk_kind_counts(&o.build).map_err(db)?,
            ))
        }
    }
}

fn document_rows(o: &Opened) -> Result<Vec<Value>, DocMonkError> {
    let (docs, counts) = documents_and_counts(o)?;
    let (times, file_list) = file_listing(o)?;
    let files: std::collections::HashMap<String, _> =
        file_list.into_iter().map(|f| (f.id.clone(), f)).collect();
    let mut rows = Vec::new();
    for d in &docs {
        let f = files.get(&d.file_id);
        let (indexed_at, updated_at) = times.get(&d.file_id).cloned().unwrap_or_default();
        let path = f.map_or_else(|| d.file_id.clone(), |f| abs(&o.source.path, &f.rel_path));
        let file_name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = match &d.attachment {
            Some(a) => format!("{file_name} -> {}", a.name.clone().unwrap_or_default()),
            None => file_name,
        };
        let (sections, paragraphs, tables) = counts.get(&d.id).copied().unwrap_or_default();
        rows.push(json!({
            "id": d.id,
            "source_id": o.source.id,
            "file_id": d.file_id,
            "path": path,
            "name": name,
            "attachment": attachment_info(d),
            "format": d.format,
            "title": d.title,
            "size": f.map(|f| f.size),
            "status": f.map(|f| f.status.clone()),
            "section_count": sections,
            "paragraph_count": paragraphs,
            "table_count": tables,
            "last_modified": updated_at,
            "indexed_at": indexed_at,
            // Not rendered: keeps a parent ahead of its attachments.
            "_order": [d.attachment.is_some(), d.attachment.as_ref().map_or(0, |a| a.index)],
        }));
    }
    Ok(rows)
}

/// `document_service.list_documents`.
pub fn list_documents(
    home: &Home,
    source_id: Option<&str>,
    query: Option<&str>,
    fmt: Option<&str>,
    page: i64,
) -> Result<Value, DocMonkError> {
    const PAGE_SIZE: i64 = 25;
    let mut rows = Vec::new();
    for o in open_sources(home, None)? {
        if source_id.is_some_and(|id| id != o.source.id) {
            continue;
        }
        for r in document_rows(&o)? {
            let path = r["path"].as_str().unwrap_or_default().to_lowercase();
            if query.is_some_and(|q| !path.contains(&q.to_lowercase())) {
                continue;
            }
            if fmt.is_some_and(|f| r["format"] != f) {
                continue;
            }
            rows.push(r);
        }
    }
    rows.sort_by(|a, b| {
        let key = |r: &Value| {
            (
                r["path"].as_str().unwrap_or_default().to_owned(),
                r["_order"][0].as_bool().unwrap_or(false),
                r["_order"][1].as_i64().unwrap_or(0),
            )
        };
        key(a).cmp(&key(b))
    });
    for r in &mut rows {
        r.as_object_mut().map(|o| o.remove("_order"));
    }
    let total = rows.len() as i64;
    let page = page.max(1);
    let start = ((page - 1) * PAGE_SIZE).min(total) as usize;
    let end = (start + PAGE_SIZE as usize).min(rows.len());
    let mut formats: Vec<String> = rows
        .iter()
        .filter_map(|r| r["format"].as_str().map(str::to_owned))
        .collect();
    formats.sort();
    formats.dedup();
    Ok(json!({
        "documents": rows[start..end],
        "total": total,
        "page": page,
        "page_size": PAGE_SIZE,
        "pages": ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1),
        "formats": formats,
    }))
}

/// `document_service.document_detail`; `Ok(None)` for an unknown id.
pub fn document_detail(
    home: &Home,
    source_id: &str,
    document_id: &str,
) -> Result<Option<Value>, DocMonkError> {
    catalog(home)?.get(source_id)?;
    let Some(o) = open_sources(home, Some(source_id))?.into_iter().next() else {
        return Ok(None);
    };
    let (docs, counts) = documents_and_counts(&o)?;
    let Some(d) = docs.iter().find(|d| d.id == document_id) else {
        return Ok(None);
    };
    let (times, files) = file_listing(&o)?;
    let file = files.into_iter().find(|f| f.id == d.file_id);
    let (sections, paragraphs, tables) = counts.get(&d.id).copied().unwrap_or_default();
    let mut chunks: Vec<_> = match o.store.server() {
        Some(server) => server.document_chunks(&o.build, &d.id).map_err(sdb)?,
        None => o
            .store
            .local()?
            .file_chunks(&o.build, &d.file_id)
            .map_err(db)?
            .into_iter()
            .filter(|c| c.document_id == d.id)
            .collect(),
    };
    chunks.sort_by_key(|c| c.ordinal);
    Ok(Some(json!({
        "id": d.id,
        "source_id": source_id,
        "path": file.as_ref().map_or_else(|| d.file_id.clone(), |f| abs(&o.source.path, &f.rel_path)),
        "attachment": attachment_info(d),
        "format": d.format,
        "title": d.title,
        "author": d.author,
        "page_count": d.page_count,
        "section_count": sections,
        "paragraph_count": paragraphs,
        "table_count": tables,
        "is_scanned": d.is_scanned,
        "indexed_at": times.get(&d.file_id).and_then(|t| t.0.clone()),
        "chunks": chunks.iter().map(|c| json!({
            "id": c.id,
            "kind": c.kind,
            "heading_path": c.heading_path,
            "page_start": c.page_start,
            "page_end": c.page_end,
            "text": docmonk_retrieval::context::text_of(c),
            "has_embedding": c.embedding_text.as_deref().is_some_and(|t| !t.is_empty()),
        })).collect::<Vec<_>>(),
    })))
}

// ------------------------------------------------------------ search ---

/// `search_service.run_search`.
pub fn run_search(
    home: &Home,
    query: &str,
    mode: &str,
    limit: usize,
) -> Result<Value, DocMonkError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(
            json!({"query": query, "mode": mode, "lexical": [], "semantic": null, "reason": null}),
        );
    }
    let cfg = load(home)?;
    let opened = open_sources(home, None)?;
    let mut lexical = Vec::new();
    if matches!(mode, "lexical" | "hybrid") {
        let v = query_cmd::lexical_value(&cfg, &opened, query, limit)?;
        for (i, r) in v["results"].as_array().into_iter().flatten().enumerate() {
            lexical.push(json!({
                "rank": i + 1,
                "method": "lexical",
                "kind": r["kind"],
                "score": null,
                "tier": r["tier"],
                "id": r["id"],
                "title": r["title"],
                "path": r["path"],
                "source_id": r["source_id"],
                "snippet": r["snippet"],
            }));
        }
    }
    let mut semantic = Value::Null;
    let mut reason = Value::Null;
    if matches!(mode, "semantic" | "hybrid") {
        if !cfg.search.semantic {
            reason = json!("semantic search is disabled (search.semantic = false)");
        } else {
            let s = query_cmd::semantic_value(home, &cfg, &opened, query, limit)?;
            if s["available"] == false {
                reason = s["reason"].clone();
                semantic = json!([]);
            } else {
                semantic = json!(s["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(i, h)| json!({
                        "rank": i + 1,
                        "method": "semantic",
                        "kind": h["kind"],
                        "score": h["score"].as_f64().map(|f| (f * 10000.0).round() / 10000.0),
                        "tier": null,
                        "id": h["id"],
                        "title": h["title"],
                        "path": h["path"],
                        "source_id": h["source_id"],
                        "snippet": h["snippet"],
                    }))
                    .collect::<Vec<_>>());
            }
        }
    }
    Ok(
        json!({"query": query, "mode": mode, "lexical": lexical, "semantic": semantic, "reason": reason}),
    )
}

// --------------------------------------------------------- knowledge ---

/// `knowledge_service.list_symbols`.
pub fn list_symbols(
    home: &Home,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<Value>, DocMonkError> {
    let mut rows = Vec::new();
    for o in open_sources(home, None)? {
        let entities = match (query, o.store.server()) {
            (Some(q), Some(server)) => {
                // Same token-AND semantics as the local FTS lookup.
                let expr = docmonk_storage::search::word_tokens(q)
                    .iter()
                    .map(|t| format!("\"{t}\""))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                server
                    .search_entities_fts(&o.build, &expr, limit as i64)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|h| server.entity(&o.build, &h.id).ok().flatten())
                    .collect()
            }
            (None, Some(server)) => server.all_entities(&o.build).map_err(sdb)?,
            // A query FTS rejects matches nothing, never a 500.
            (Some(q), None) => {
                let store = o.store.local()?;
                store
                    .search_code(&o.build, q, limit as i64)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|h| store.entity(&o.build, &h.id).ok().flatten())
                    .collect()
            }
            (None, None) => o.store.local()?.all_entities(&o.build).map_err(db)?,
        };
        for e in entities {
            rows.push(json!({
                "id": e.id,
                "name": e.name,
                "qualified_name": e.qualified_name,
                "kind": e.kind,
                "language": e.language,
                "source_id": o.source.id,
                "start_line": e.start_line,
            }));
            if rows.len() >= limit {
                break;
            }
        }
    }
    rows.sort_by(|a, b| {
        a["qualified_name"]
            .as_str()
            .cmp(&b["qualified_name"].as_str())
    });
    rows.truncate(limit);
    Ok(rows)
}

/// The symbol page's data for `relation` (`knowledge_service`).
pub fn symbol_relation(home: &Home, name: &str, relation: &str) -> Result<Value, DocMonkError> {
    let opened = open_sources(home, None)?;
    let (depth, limit) = (graph::DEFAULT_MAX_DEPTH, graph::DEFAULT_LIMIT);
    match relation {
        "callees" => query_cmd::calls_value(&opened, name, Direction::Outgoing, depth, limit),
        "references" => query_cmd::traverse_value(
            &opened,
            name,
            Direction::Incoming,
            &["references"],
            depth,
            limit,
        ),
        "impact" => query_cmd::impact_value(&opened, name, depth, limit),
        _ => query_cmd::calls_value(&opened, name, Direction::Incoming, depth, limit),
    }
}

// ---------------------------------------------------------------- ai ---

fn auth_status(provider: &str) -> &'static str {
    let Some(cap) = docmonk_ai::registry::get(provider).filter(|_| provider != "none") else {
        return "not configured";
    };
    if let Some(env) = cap.api_key_env {
        return if std::env::var(env).is_ok_and(|v| !v.is_empty()) {
            "configured"
        } else {
            "not configured"
        };
    }
    if cap.subscription {
        return "account-based (test to check sign-in)";
    }
    "local/no auth"
}

/// `ai_service.provider_overview`.
pub fn ai_overview(home: &Home) -> Result<Value, DocMonkError> {
    let cfg = load(home)?;
    let selected = &cfg.ai.provider;
    Ok(json!({
        "selected": selected,
        "model": if cfg.ai.model.is_empty() { Value::Null } else { json!(cfg.ai.model) },
        "base_url": cfg.ai.base_url,
        "external_ai_allowed": cfg.privacy.external_ai_allowed,
        "auth_status": auth_status(selected),
        "providers": docmonk_ai::registry::REGISTRY.iter().map(|c| json!({
            "provider_id": c.provider_id,
            "display_name": c.display_name,
            "subscription": c.subscription,
            "cloud_egress": c.cloud_egress,
            "beta": c.beta,
            "api_key_env": c.api_key_env,
            "model_discovery": c.model_discovery,
            "selected": c.provider_id == selected,
        })).collect::<Vec<_>>(),
    }))
}

/// `ai_service.test_provider`: never reveals a secret.
pub fn ai_test(home: &Home, provider: &str) -> Result<Value, DocMonkError> {
    let cfg = load(home)?;
    let Some(cap) = docmonk_ai::registry::get(provider) else {
        return Ok(json!({"provider": provider, "ok": false, "detail": "unknown provider"}));
    };
    if cap.subscription {
        let outcome = docmonk_ai::runtime::resolve_runtime(cap.provider_id, &cfg.ai)
            .and_then(|mut r| r.status());
        return Ok(match outcome {
            Ok(s) => json!({
                "provider": provider,
                "ok": s.authenticated,
                "detail": s.detail,
                "account": s.account,
            }),
            Err(e) => json!({"provider": provider, "ok": false, "detail": e.message()}),
        });
    }
    if let Some(env) = cap.api_key_env {
        let present = std::env::var(env).is_ok_and(|v| !v.is_empty());
        return Ok(json!({
            "provider": provider,
            "ok": present,
            "detail": if present { format!("{env} is set") } else { format!("{env} is not set") },
        }));
    }
    let base = cfg.ai.base_url.unwrap_or_else(|| "(default)".into());
    Ok(
        json!({"provider": provider, "ok": true, "detail": format!("local provider; base URL {base}")}),
    )
}

// ------------------------------------------------------------ health ---

/// `health_service.health_report`.
pub fn health_report(home: &Home) -> Result<Value, DocMonkError> {
    let sections = doctor_cmd::run_checks(home)?;
    Ok(json!({
        "overall": doctor_cmd::overall(&sections),
        "sections": sections.iter().map(|s| json!({
            "name": s.name,
            "checks": s.checks.iter().map(|c| json!({
                "name": c.name, "status": c.status, "detail": c.detail,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    }))
}

// ----------------------------------------------------------- backups ---

/// `backup_service.list_backups`: newest first.
pub fn list_backups(home: &Home) -> Vec<Value> {
    let mut entries: Vec<(f64, Value)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(home.backups_dir()) {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".tar.gz") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0.0, |d| d.as_secs_f64());
            entries.push((
                mtime,
                json!({
                    "name": name,
                    "path": e.path().to_string_lossy(),
                    "size_bytes": meta.len(),
                    "created_at": mtime,
                }),
            ));
        }
    }
    entries.sort_by(|a, b| b.0.total_cmp(&a.0));
    entries.into_iter().map(|(_, v)| v).collect()
}

/// A backup name resolved inside the backups directory, refusing
/// traversal (`backup_service.backup_path`).
pub fn backup_path(home: &Home, name: &str) -> Result<std::path::PathBuf, String> {
    let dir = home.backups_dir();
    let candidate = dir.join(name);
    let resolved = dunce_canonical(&candidate);
    let base = dunce_canonical(&dir);
    match (resolved, base) {
        (Some(r), Some(b)) if r.parent() == Some(b.as_path()) && r.is_file() => Ok(r),
        _ => Err(format!("no such backup: {name}")),
    }
}

fn dunce_canonical(p: &Path) -> Option<std::path::PathBuf> {
    docmonk_core::paths::resolve(p).ok().filter(|r| r.exists())
}

/// `backup_service.update_status`: the local update cache only.
pub fn update_status(home: &Home) -> Result<Value, DocMonkError> {
    let cfg = load(home)?;
    let installed = docmonk_core::version::version();
    let cache: Option<Value> = std::fs::read_to_string(home.root().join("update.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|c| {
            [
                "last_checked",
                "installed_version",
                "latest_version",
                "release_url",
            ]
            .iter()
            .all(|k| c.get(*k).is_some())
        });
    let Some(c) = cache else {
        return Ok(json!({
            "installed_version": installed,
            "latest_version": null,
            "update_available": null,
            "channel": cfg.updates.channel,
            "last_checked": null,
            "release_url": null,
        }));
    };
    let latest = c["latest_version"].as_str().unwrap_or_default();
    let newer = docmonk_update::versioning::is_newer(latest, installed);
    Ok(json!({
        "installed_version": installed,
        "latest_version": c["latest_version"],
        "update_available": newer,
        "channel": cfg.updates.channel,
        "last_checked": c["last_checked"],
        "release_url": c["release_url"],
    }))
}

// -------------------------------------------------------------- logs ---

/// `logs_service.read_logs`: newest first, filtered.
pub fn read_logs(
    home: &Home,
    level: Option<&str>,
    component: Option<&str>,
    query: Option<&str>,
    errors_only: bool,
) -> Value {
    const LIMIT: usize = 300;
    let log_path = home.logs_dir().join("docmonk.log");
    let mut entries = Vec::new();
    let mut components = std::collections::BTreeSet::new();
    if let Ok(text) = std::fs::read_to_string(&log_path) {
        let lines: Vec<&str> = text.lines().collect();
        let tail = &lines[lines.len().saturating_sub(LIMIT * 4)..];
        for raw in tail.iter().rev() {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let record = match serde_json::from_str::<Value>(raw) {
                Ok(v @ Value::Object(_)) => v,
                _ => json!({"level": "INFO", "component": "-", "event": raw, "timestamp": null}),
            };
            let comp = record
                .get("component")
                .map_or_else(|| "-".to_owned(), crate::plain_json);
            components.insert(comp);
            let lvl = record
                .get("level")
                .map_or_else(|| "INFO".to_owned(), crate::plain_json)
                .to_uppercase();
            if errors_only && !matches!(lvl.as_str(), "ERROR" | "CRITICAL") {
                continue;
            }
            if level.is_some_and(|l| lvl != l.to_uppercase()) {
                continue;
            }
            if component.is_some_and(|c| {
                record
                    .get("component")
                    .map_or_else(String::new, crate::plain_json)
                    != c
            }) {
                continue;
            }
            if query.is_some_and(|q| !raw.to_lowercase().contains(&q.to_lowercase())) {
                continue;
            }
            entries.push(record);
            if entries.len() >= LIMIT {
                break;
            }
        }
    }
    json!({
        "entries": entries,
        "levels": ["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"],
        "components": components,
        "log_path": log_path.to_string_lossy(),
    })
}

/// The last `max` lines of the daemon's own output log.
pub fn daemon_log_tail(home: &Home, max: usize) -> Vec<String> {
    let Ok(bytes) = std::fs::read(home.logs_dir().join("daemon.out.log")) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines[lines.len().saturating_sub(max)..].to_vec()
}

/// `_format_uptime` for an ISO start time.
pub fn format_uptime(started_at: Option<&str>) -> String {
    let Some(started) = started_at.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    else {
        return String::new();
    };
    let secs = (chrono::Utc::now() - started.with_timezone(&chrono::Utc)).num_seconds();
    docmonk_indexing::daemon::format_uptime(secs as f64)
}

// -------------------------------------------------------- project ids ---

/// The project directory of a source (for completeness of the layout).
#[allow(dead_code)]
pub fn project_dir(home: &Home, source_path: &str) -> std::path::PathBuf {
    StorageLayout::new(home).project_dir(&project_id_for_canonical(source_path))
}
