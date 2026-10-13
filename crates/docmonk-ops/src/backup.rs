//! Backup archives and restore.
//!
//! Archives are a `.tar.gz` holding `manifest.json`, a consistent
//! `VACUUM INTO` snapshot of `state/control.db` and of every indexed
//! project's `projects/<id>/knowledge.db`, plus `config.yaml`. The
//! manifest records the archive `format_version` and the schema
//! fingerprints of the databases; an archive whose format or schemas
//! differ from this build is refused, never converted. ANN index files are
//! caches and are not archived: queries fall back to exact search until
//! the next sync rebuilds them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_indexing::daemon::pid;
use docmonk_storage::maintenance;
use docmonk_storage::maintenance::SchemaState;
use docmonk_storage::schema::{control_fingerprint, knowledge_fingerprint};
use docmonk_storage::StorageLayout;
use serde_json::{json, Value};

use crate::{dberr, generic, now_iso};

pub const MANIFEST_FORMAT_VERSION: i64 = 1;

pub(crate) const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// A private scratch directory under `<home>/tmp`, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(home: &Home, label: &str) -> Result<Self, DocMonkError> {
        let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S%f");
        let dir = home
            .tmp_dir()
            .join(format!("{label}_{stamp}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(generic)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a backup archive and returns `(path, manifest)`. Reads every
/// database through read-only snapshots.
pub fn create_backup(home: &Home, dest: Option<&Path>) -> Result<(PathBuf, Value), DocMonkError> {
    // Server mode: the knowledge lives in the cluster; back it up with the
    // cluster's snapshot API. A local archive would not contain it.
    crate::require_local(home, "backup")?;
    let layout = StorageLayout::new(home);
    let control = layout.control_db();
    let mut projects = serde_json::Map::new();
    let mut sources = Vec::new();
    for (id, path) in maintenance::registered_sources(&control) {
        let pid = project_id_for_canonical(&path);
        if layout.project_db(&pid).is_file() {
            projects.insert(pid.clone(), json!(knowledge_fingerprint()));
        }
        sources.push(json!({"id": id, "path": path, "project_id": pid}));
    }
    let created_at = now_iso();
    let manifest = json!({
        "format_version": MANIFEST_FORMAT_VERSION,
        "docmonk_version": docmonk_core::version::version(),
        "created_at": created_at,
        "control_schema": control_fingerprint(),
        "projects": projects,
        "sources": sources,
    });
    let archive = match dest {
        Some(d) => d.to_path_buf(),
        None => {
            let stamp: String = created_at
                .chars()
                .filter(|c| !matches!(c, ':' | '-' | '.'))
                .collect();
            home.backups_dir()
                .join(format!("docmonk-backup-{stamp}.tar.gz"))
        }
    };
    if let Some(dir) = archive.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(generic)?;
    }
    let scratch = Scratch::new(home, "backup")?;
    let stage = &scratch.0;
    if control.is_file() {
        maintenance::snapshot(&control, &stage.join("state").join("control.db")).map_err(dberr)?;
    }
    for pid in projects.keys() {
        maintenance::snapshot(
            &layout.project_db(pid),
            &stage.join("projects").join(pid).join("knowledge.db"),
        )
        .map_err(dberr)?;
    }
    if home.user_config().is_file() {
        std::fs::copy(home.user_config(), stage.join("config.yaml")).map_err(generic)?;
    }
    std::fs::write(
        stage.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).map_err(generic)?,
    )
    .map_err(generic)?;
    let file = std::fs::File::create(&archive).map_err(generic)?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    tar.append_dir_all(".", stage).map_err(generic)?;
    tar.into_inner()
        .and_then(flate2::write::GzEncoder::finish)
        .map_err(generic)?;
    Ok((archive, manifest))
}

fn extract(archive: &Path, into: &Path) -> Result<(), DocMonkError> {
    let file = std::fs::File::open(archive).map_err(generic)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    tar.set_preserve_permissions(false);
    // `unpack` refuses entries that would escape `into` (`..`, absolute).
    tar.unpack(into)
        .map_err(|e| dberr(format!("archive is corrupt or unreadable: {e}")))
}

fn move_aside(path: &Path, holding: &Path) -> Result<Option<PathBuf>, DocMonkError> {
    if !path.exists() {
        return Ok(None);
    }
    let target = holding.join(path.file_name().unwrap_or_default());
    std::fs::rename(path, &target).map_err(generic)?;
    Ok(Some(target))
}

/// Verifies and restores an archive into `home` (the `restore` report).
pub fn restore_archive(home: &Home, archive: &Path) -> Result<Value, DocMonkError> {
    crate::require_local(home, "restore")?;
    let home = home.clone();
    let archive = archive.to_path_buf();
    if !archive.is_file() {
        return Err(DocMonkError::usage(format!(
            "no such archive: {}",
            archive.display()
        )));
    }
    let scratch = Scratch::new(&home, "restore_staging")?;
    let stage = &scratch.0;
    extract(&archive, stage)?;
    let manifest: Value = std::fs::read_to_string(stage.join("manifest.json"))
        .map_err(|_| dberr("archive is missing manifest.json; refusing to restore"))
        .and_then(|t| {
            serde_json::from_str(&t)
                .map_err(|e| dberr(format!("archive manifest is unreadable: {e}")))
        })?;
    let format = manifest["format_version"].as_i64();
    if format != Some(MANIFEST_FORMAT_VERSION) {
        return Err(dberr(format!(
            "unsupported backup archive format v{} (this DocMonk restores v{MANIFEST_FORMAT_VERSION}); \
             refusing to restore, nothing was changed",
            format.map_or_else(|| "?".to_owned(), |v| v.to_string())
        )));
    }
    let incompatible = |what: &str, detail: &str| {
        dberr(format!(
            "{what} in this archive does not match this DocMonk's storage format ({detail}); \
             refusing to restore, nothing was changed. Reindex your sources instead"
        ))
    };
    let staged_state = stage.join("state");
    let staged_projects = stage.join("projects");
    let control = staged_state.join("control.db");
    if !control.is_file() {
        return Err(dberr(
            "archive is missing state/control.db; refusing to restore",
        ));
    }
    maintenance::integrity_check(&control).map_err(dberr)?;
    match maintenance::control_schema_state(&control) {
        SchemaState::Current => {}
        SchemaState::Missing => return Err(incompatible("control.db", "empty database")),
        SchemaState::Incompatible(d) => return Err(incompatible("control.db", &d)),
    }
    let mut restored = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&staged_projects) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let db = dir.join("knowledge.db");
            if !db.is_file() {
                continue;
            }
            maintenance::integrity_check(&db).map_err(dberr)?;
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let SchemaState::Incompatible(d) = maintenance::knowledge_schema_state(&db) {
                return Err(incompatible(&format!("project {name}"), &d));
            }
            restored.push(name);
        }
    }
    // Everything verified: only now touch the live home.
    let daemon_was_running =
        pid::stop_and_wait(&home, STOP_TIMEOUT, "restore into a live runtime directory")
            .map_err(dberr)?;
    let holding = home.tmp_dir().join(format!(
        "pre_restore_{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S%f")
    ));
    std::fs::create_dir_all(&holding).map_err(generic)?;
    let staged_config = stage.join("config.yaml");
    let mut moves = vec![
        (staged_state, home.state_dir()),
        (staged_projects, home.projects_dir()),
    ];
    if staged_config.is_file() {
        moves.push((staged_config, home.user_config()));
    }
    let mut aside: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (_, live) in &moves {
        if let Some(t) = move_aside(live, &holding)? {
            aside.push((t, live.clone()));
        }
    }
    let swap = moves
        .iter()
        .filter(|(staged, _)| staged.exists())
        .try_for_each(|(staged, live)| std::fs::rename(staged, live));
    if let Err(e) = swap {
        for (_, live) in &moves {
            if live.is_dir() {
                let _ = std::fs::remove_dir_all(live);
            } else {
                let _ = std::fs::remove_file(live);
            }
        }
        for (moved, original) in aside.into_iter().rev() {
            let _ = std::fs::rename(&moved, &original);
        }
        return Err(generic(format!(
            "restore failed; previous state put back: {e}"
        )));
    }
    let _ = std::fs::remove_dir_all(&holding);
    let mut daemon_restarted = false;
    if daemon_was_running {
        docmonk_service::daemon::start(&home)?;
        daemon_restarted = true;
    }
    Ok(json!({
        "archive": archive.to_string_lossy(),
        "projects_restored": restored,
        "daemon_was_running": daemon_was_running,
        "daemon_restarted": daemon_restarted,
    }))
}
