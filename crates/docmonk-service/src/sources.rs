//! Source registration: add, list, inspect, enable/disable and remove.

use std::path::Path;

use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::daemon::pid;
use docmonk_indexing::lock::RunLock;
use docmonk_storage::control::{ControlPlane, NewSource, SourceRecord};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

use crate::backend::{record_of, server_err, Backend};
use crate::{db, generic, load};

/// The control plane (created on first use). Local mode only: in server
/// mode the server catalog is authoritative and the local control plane is
/// never opened (use [`catalog`]).
pub fn control_plane(home: &Home) -> Result<ControlPlane, DocMonkError> {
    let cfg = load(home)?;
    if cfg.storage.mode == "server" {
        return Err(crate::backend::local_only(
            "the local source registry (state/control.db)",
        ));
    }
    ControlPlane::open(&StorageLayout::new(home), cfg.runtime.sqlite_cache_size_mb).map_err(db)
}

/// The authoritative source catalog of the configured backend.
pub enum Catalog {
    Local(ControlPlane),
    Server(std::sync::Arc<docmonk_backends::ServerBackend>),
}

/// The catalog for `home` (server mode: the server catalog, created if
/// missing; an unreachable server is an error).
pub fn catalog(home: &Home) -> Result<Catalog, DocMonkError> {
    match crate::backend::open_for_write(home)? {
        Backend::Local => Ok(Catalog::Local(control_plane(home)?)),
        Backend::Server(s) => Ok(Catalog::Server(s)),
    }
}

/// Git options of [`Catalog::add`] (ignored for directories).
#[derive(Debug, Clone, Default)]
pub struct GitOptions {
    pub branch: Option<String>,
    /// A directory inside the repository to index instead of its root.
    pub subpath: Option<String>,
}

/// `(online_status, last_scan_at)` of a source.
pub struct SourceSummary {
    pub online_status: String,
    pub last_scan_at: Option<String>,
}

impl Catalog {
    pub fn is_server(&self) -> bool {
        matches!(self, Catalog::Server(_))
    }

    /// Registered sources in registration order.
    pub fn list(&self, enabled_only: bool) -> Result<Vec<SourceRecord>, DocMonkError> {
        match self {
            Catalog::Local(cp) => cp.list_sources(enabled_only).map_err(db),
            Catalog::Server(s) => {
                let mut v: Vec<SourceRecord> = s
                    .list_catalog(enabled_only)
                    .map_err(server_err)?
                    .iter()
                    .map(record_of)
                    .collect();
                v.sort_by(|a, b| {
                    pad_millis(&a.created_at)
                        .cmp(&pad_millis(&b.created_at))
                        .then_with(|| a.id.cmp(&b.id))
                });
                Ok(v)
            }
        }
    }

    pub fn get(&self, id: &str) -> Result<SourceRecord, DocMonkError> {
        match self {
            Catalog::Local(cp) => get_source(cp, id),
            Catalog::Server(s) => s
                .catalog_entry(id)
                .map_err(server_err)?
                .map(|e| record_of(&e))
                .ok_or_else(|| DocMonkError::usage(format!("no such source: {id}"))),
        }
    }

    pub fn summary(&self, id: &str) -> Result<SourceSummary, DocMonkError> {
        match self {
            Catalog::Local(cp) => {
                let st = cp.state(id).map_err(db)?;
                Ok(SourceSummary {
                    online_status: st.online_status,
                    last_scan_at: st.last_scan_at,
                })
            }
            Catalog::Server(s) => {
                let st = s.source_state(id).map_err(server_err)?;
                let f = |k: &str| st.as_ref().and_then(|x| x.field(k)).map(str::to_owned);
                Ok(SourceSummary {
                    online_status: f("online_status").unwrap_or_else(|| "unknown".into()),
                    last_scan_at: f("last_scan_at").map(|m| millis_to_iso(&m)),
                })
            }
        }
    }

    /// [`Catalog::add`], or, when `path` is a git URL, a shallow checkout
    /// of `git.branch` (default: the remote's default branch) under
    /// `<home>/sources/git` (ADR 0043; idempotent by URL, branch and
    /// subpath).
    pub fn add_any(
        &mut self,
        home: &Home,
        path: &str,
        git: &GitOptions,
        include: Vec<String>,
        exclude: Vec<String>,
    ) -> Result<SourceRecord, DocMonkError> {
        let path = if docmonk_indexing::git::is_remote_url(path) {
            if self.is_server() {
                return Err(DocMonkError::usage(
                    "git sources are local-mode only: in server mode, check the repository \
                     out on each indexing host and add the directory",
                ));
            }
            docmonk_indexing::git::clone(
                home.root(),
                path,
                git.branch.as_deref(),
                git.subpath.as_deref(),
            )
            .map_err(|e| DocMonkError::new(ErrorKind::SourceUnavailable, e))?
            .to_string_lossy()
            .into_owned()
        } else if git.branch.is_some() || git.subpath.is_some() {
            return Err(DocMonkError::usage(
                "--branch and --subpath apply to git URLs only",
            ));
        } else {
            path.to_owned()
        };
        self.add(&path, include, exclude)
    }

    /// Registers a directory (idempotent by canonical path).
    pub fn add(
        &mut self,
        path: &str,
        include: Vec<String>,
        exclude: Vec<String>,
    ) -> Result<SourceRecord, DocMonkError> {
        match self {
            Catalog::Local(cp) => add_source(cp, path, include, exclude),
            Catalog::Server(s) => {
                let (canonical, source_type) = canonical_dir(path)?;
                let (e, _) = s
                    .register_source(&docmonk_backends::backend::CatalogEntry {
                        source_id: docmonk_core::ids::make_source_id(&canonical),
                        path: canonical,
                        source_type: source_type.as_str().into(),
                        enabled: true,
                        include_patterns: include,
                        exclude_patterns: exclude,
                        created_at: String::new(),
                        updated_at: String::new(),
                    })
                    .map_err(server_err)?;
                Ok(record_of(&e))
            }
        }
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), DocMonkError> {
        match self {
            Catalog::Local(cp) => set_source_enabled(cp, id, enabled),
            Catalog::Server(s) => s.set_source_enabled(id, enabled).map_err(server_err),
        }
    }

    /// `source info` JSON.
    pub fn info(&self, s: &SourceRecord) -> Result<Value, DocMonkError> {
        match self {
            Catalog::Local(cp) => source_info(cp, s),
            Catalog::Server(b) => {
                let st = b.source_state(&s.id).map_err(server_err)?;
                let f = |k: &str| st.as_ref().and_then(|x| x.field(k)).map(str::to_owned);
                Ok(json!({
                    "id": s.id,
                    "path": s.path,
                    "source_type": s.source_type.as_str(),
                    "enabled": s.enabled,
                    "include_patterns": s.include_patterns,
                    "exclude_patterns": s.exclude_patterns,
                    "status": f("online_status").unwrap_or_else(|| "unknown".into()),
                    "build_state": f("state").unwrap_or_else(|| "needs_full_rebuild".into()),
                    "rebuild_reason": f("rebuild_reason"),
                    "active_build_id": f("active_build_id"),
                    "last_full_build_at": f("last_full_build_at").map(|m| millis_to_iso(&m)),
                    "last_scan_at": f("last_scan_at").map(|m| millis_to_iso(&m)),
                    "last_error": f("last_error"),
                    "created_at": millis_to_iso(&s.created_at),
                    "updated_at": millis_to_iso(&s.updated_at),
                    "backend": b.engine().as_str(),
                }))
            }
        }
    }
}

/// Epoch milliseconds (server dates) as RFC 3339; anything else unchanged.
pub fn millis_to_iso(v: &str) -> String {
    match v.parse::<i64>() {
        Ok(ms) => chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Micros, false))
            .unwrap_or_else(|| v.to_owned()),
        Err(_) => v.to_owned(),
    }
}

fn pad_millis(v: &str) -> String {
    format!("{v:0>20}")
}

/// `(canonical path, source type)` of an existing directory.
fn canonical_dir(path: &str) -> Result<(String, docmonk_core::models::SourceType), DocMonkError> {
    let source_type = docmonk_core::ids::detect_source_type(path);
    let p = expand_user(path);
    if !p.exists() {
        return Err(DocMonkError::new(
            ErrorKind::SourceUnavailable,
            format!("source path does not exist: {path}"),
        ));
    }
    let canonical = docmonk_core::paths::resolve(&p)
        .map_err(generic)?
        .to_string_lossy()
        .into_owned();
    if !p.is_dir() {
        return Err(DocMonkError::new(
            ErrorKind::SourceUnavailable,
            format!("source path is not a directory: {canonical}"),
        ));
    }
    Ok((canonical, source_type))
}

pub fn get_source(cp: &ControlPlane, id: &str) -> Result<SourceRecord, DocMonkError> {
    cp.get_source(id)
        .map_err(db)?
        .ok_or_else(|| DocMonkError::usage(format!("no such source: {id}")))
}

fn expand_user(raw: &str) -> std::path::PathBuf {
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
            return Path::new(&home).join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
            return home.into();
        }
    }
    raw.into()
}

pub fn source_info(cp: &ControlPlane, s: &SourceRecord) -> Result<Value, DocMonkError> {
    let state = cp.state(&s.id).map_err(db)?;
    let mut v = json!({
        "id": s.id,
        "path": s.path,
        "source_type": s.source_type.as_str(),
        "enabled": s.enabled,
        "include_patterns": s.include_patterns,
        "exclude_patterns": s.exclude_patterns,
        "status": state.online_status,
        "build_state": state.build_state.as_str(),
        "rebuild_reason": state.rebuild_reason,
        "active_build_id": state.active_build_id,
        "last_full_build_at": state.last_full_build_at,
        "last_scan_at": state.last_scan_at,
        "last_error": state.last_error,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    });
    if let Some(git) = docmonk_indexing::git::remote_of(Path::new(&s.path)) {
        v["git"] = json!(git);
    }
    Ok(v)
}

pub fn assert_no_active_daemon(home: &Home) -> Result<(), DocMonkError> {
    if pid::running_daemon(home).is_some() {
        return Err(DocMonkError::usage(
            "a DocMonk daemon is running and may be actively indexing sources; \
             run `docmonk daemon stop` first, then retry.",
        ));
    }
    Ok(())
}

/// Registers a source (`SourceRegistry.add`).
pub fn add_source(
    cp: &mut ControlPlane,
    path: &str,
    include: Vec<String>,
    exclude: Vec<String>,
) -> Result<SourceRecord, DocMonkError> {
    let (canonical, source_type) = canonical_dir(path)?;
    let (s, _) = cp
        .add_source(&NewSource {
            canonical_path: canonical,
            source_type,
            enabled: true,
            include_patterns: include,
            exclude_patterns: exclude,
        })
        .map_err(db)?;
    Ok(s)
}

/// Enables or disables a registered source.
pub fn set_source_enabled(
    cp: &mut ControlPlane,
    source_id: &str,
    enabled: bool,
) -> Result<(), DocMonkError> {
    get_source(cp, source_id)?;
    cp.set_enabled(source_id, enabled).map_err(db)
}

/// Removes a source and its indexed data (never the original files),
/// under the `index` lock and only while no daemon is running. Returns
/// whether indexed data was deleted.
pub fn remove_source(home: &Home, source_id: &str) -> Result<bool, DocMonkError> {
    if let Catalog::Server(server) = catalog(home)? {
        Catalog::Server(server.clone()).get(source_id)?;
        assert_no_active_daemon(home)?;
        let cfg = load(home)?;
        let lock = RunLock::acquire(
            &crate::indexing::source_lock_path(home, source_id),
            "source-remove",
            Some(source_id),
            std::time::Duration::from_secs_f64(cfg.indexing.lock_timeout_seconds),
        )?;
        // Refuses while another host holds the source's writer lease.
        let lease = server
            .acquire_lease(
                source_id,
                &crate::indexing::lease_owner(),
                std::time::Duration::from_secs(60),
            )
            .map_err(server_err)?;
        let n = server.remove_source(source_id).map_err(server_err)?;
        let _ = lease;
        crate::server_index::drop_staging(home, &cfg, source_id);
        lock.release();
        return Ok(n > 0);
    }
    let mut cp = control_plane(home)?;
    let s = get_source(&cp, source_id)?;
    assert_no_active_daemon(home)?;
    let layout = StorageLayout::new(home);
    let project_dir = layout.project_dir(&project_id_for_canonical(&s.path));
    let cfg = load(home)?;
    let lock = RunLock::acquire(
        &home.locks_dir().join("index.lock"),
        "source-remove",
        Some(source_id),
        std::time::Duration::from_secs_f64(cfg.indexing.lock_timeout_seconds),
    )?;
    // Data first: if deleting it fails, the source stays registered
    // rather than half removed.
    let deleted = project_dir.exists();
    if deleted {
        std::fs::remove_dir_all(&project_dir).map_err(|e| {
            generic(format!(
                "failed to delete project data at {}: {e}; source '{source_id}' was not removed",
                project_dir.display()
            ))
        })?;
    }
    cp.remove_source(source_id).map_err(db)?;
    // A git source's checkout is DocMonk's own copy, not original files.
    if let Some(dir) = docmonk_indexing::git::remote_dir_of(Path::new(&s.path)) {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(component = "sources", event = "git_checkout_not_removed", source_id, path = %dir.display(), error = %e);
        }
    }
    lock.release();
    Ok(deleted)
}

/// `docs` rows (`cli/docs._run`): every document-kind file of each
/// source's active build, with its document metadata when there is one.
pub fn docs_rows(home: &Home, source_id: Option<&str>) -> Result<Vec<Value>, DocMonkError> {
    if let Catalog::Server(_) = catalog(home)? {
        return server_docs_rows(home, source_id);
    }
    let cfg = load(home)?;
    let cp = control_plane(home)?;
    let sources = match source_id {
        Some(id) => vec![get_source(&cp, id)?],
        None => cp.list_sources(false).map_err(db)?,
    };
    let layout = StorageLayout::new(home);
    let mut rows = Vec::new();
    for s in &sources {
        let Some(build) = cp.state(&s.id).map_err(db)?.active_build_id else {
            continue;
        };
        let store = ProjectStore::open(
            &layout,
            &project_id_for_canonical(&s.path),
            &s.id,
            cfg.runtime.sqlite_cache_size_mb,
        )
        .map_err(db)?;
        let docs = store.documents(&build).map_err(db)?;
        let counts = store.chunk_kind_counts(&build).map_err(db)?;
        let mut files = store.files(&build).map_err(db)?;
        files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        for f in files.iter().filter(|f| f.kind == "document") {
            let doc = docs
                .iter()
                .find(|d| d.file_id == f.id && d.attachment.is_none());
            let mut children: Vec<_> = docs
                .iter()
                .filter(|d| d.file_id == f.id && d.attachment.is_some())
                .collect();
            children.sort_by_key(|d| d.attachment.as_ref().map_or(0, |a| a.index));
            let attachments: Vec<Value> = children
                .iter()
                .map(|d| json!(d.attachment.as_ref().and_then(|a| a.name.clone())))
                .collect();
            let (sections, paragraphs, tables) = doc
                .and_then(|d| counts.get(&d.id).copied())
                .unwrap_or_default();
            let abs = Path::new(&s.path).join(&f.rel_path);
            rows.push(json!({
                "source_id": s.id,
                "file_id": f.id,
                "path": abs.to_string_lossy(),
                "status": f.status,
                "format": doc.map(|d| d.format.clone()),
                "title": doc.and_then(|d| d.title.clone()),
                "page_count": doc.and_then(|d| d.page_count),
                "section_count": sections,
                "paragraph_count": paragraphs,
                "table_count": tables,
                "is_scanned": doc.is_some_and(|d| d.is_scanned),
                "language": doc.and_then(|d| d.language.clone()),
                "script": doc.and_then(|d| d.script.clone()),
                "pages": doc.and_then(|d| d.pages.as_ref()).map(|p| json!({
                    "ocr": p.ocr, "ocr_pages": p.ocr_pages, "empty_pages": p.empty_pages,
                })),
                "attachments": attachments,
            }));
        }
    }
    Ok(rows)
}

/// Where a source's indexed data lives (never its original files).
pub fn project_data_dir(home: &Home, source: &SourceRecord) -> std::path::PathBuf {
    StorageLayout::new(home).project_dir(&project_id_for_canonical(&source.path))
}

/// [`docs_rows`] from the server's published builds.
fn server_docs_rows(home: &Home, source_id: Option<&str>) -> Result<Vec<Value>, DocMonkError> {
    let e = |e: docmonk_storage::StorageError| -> DocMonkError { e.into() };
    let mut rows = Vec::new();
    for o in crate::query::open_sources(home, source_id)? {
        let Some(server) = o.store.server() else {
            continue;
        };
        let docs = server.documents(&o.build).map_err(e)?;
        let counts = server.chunk_kind_counts(&o.build).map_err(e)?;
        let mut files = server.files(&o.build, Some("document")).map_err(e)?;
        let str_of = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_owned();
        files.sort_by_key(|f| str_of(f, "rel_path"));
        for f in &files {
            let file_id = str_of(f, "file_id");
            let doc = docs
                .iter()
                .find(|d| str_of(d, "file_id") == file_id && d["parent_document_id"].is_null());
            let mut children: Vec<&Value> = docs
                .iter()
                .filter(|d| str_of(d, "file_id") == file_id && !d["parent_document_id"].is_null())
                .collect();
            children.sort_by_key(|d| d["attachment_index"].as_i64().unwrap_or(0));
            let attachments: Vec<Value> = children
                .iter()
                .map(|d| d["attachment_name"].clone())
                .collect();
            let (sections, paragraphs, tables) = doc
                .and_then(|d| counts.get(&str_of(d, "document_id")).copied())
                .unwrap_or_default();
            let abs = Path::new(&o.source.path).join(str_of(f, "rel_path"));
            rows.push(json!({
                "source_id": o.source.id,
                "file_id": file_id,
                "path": abs.to_string_lossy(),
                "status": f["status"].as_str().unwrap_or("indexed"),
                "format": doc.map(|d| d["format"].clone()),
                "title": doc.map(|d| d["title"].clone()),
                "page_count": doc.map(|d| d["page_count"].clone()),
                "section_count": sections,
                "paragraph_count": paragraphs,
                "table_count": tables,
                "is_scanned": doc.is_some_and(|d| d["is_scanned"].as_bool() == Some(true)),
                "language": doc.map(|d| d["language"].clone()),
                "script": doc.map(|d| d["script"].clone()),
                "pages": doc.filter(|d| d["ocr"].is_string()).map(|d| json!({
                    "ocr": d["ocr"], "ocr_pages": d["ocr_pages"], "empty_pages": d["empty_pages"],
                })),
                "attachments": attachments,
            }));
        }
    }
    Ok(rows)
}
