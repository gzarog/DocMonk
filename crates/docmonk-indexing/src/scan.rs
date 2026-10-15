//! Source tree scanning.
//!
//! * Symlinked files/directories are skipped unless `follow_symlinks`
//!   (on Windows, Rust also reports junctions as links, so with the default
//!   they are skipped instead of walked, so junction loops cannot occur).
//! * Every candidate is resolved and must stay inside the source root
//!   ([`PathGuard`]); the same real file/directory is never yielded twice.
//! * A directory that cannot be listed or a file that cannot be stat'ed is
//!   recorded in [`ScanOutcome::errors`]: such a scan is *incomplete* and
//!   must never be used to infer deletions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use docmonk_core::security::PathGuard;
use serde::Serialize;

use crate::fingerprint::mtime_secs;
use crate::ignore::IgnoreMatcher;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScannedFile {
    /// Resolved absolute path.
    pub path: PathBuf,
    /// Path relative to the resolved root with `/` separators.
    pub rel_path: String,
    pub size: i64,
    pub mtime: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanError {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ScanOutcome {
    pub files: Vec<ScannedFile>,
    pub errors: Vec<ScanError>,
}

impl ScanOutcome {
    pub fn complete(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub follow_symlinks: bool,
    /// Fault injection: directories treated as unreadable. Used by tests
    /// (permission bits are ignored when running as root) and failure
    /// drills; empty in normal operation.
    pub inject_unreadable: Vec<PathBuf>,
}

/// `None` when `root` is reachable (exists, is a directory, can be listed),
/// else a short reason. An unreachable root must be treated as offline,
/// never as "every file was deleted".
pub fn check_root_accessible(root: &Path) -> Option<String> {
    let resolved = match docmonk_core::paths::resolve(root) {
        Ok(r) => r,
        Err(e) => return Some(format!("cannot resolve path: {e}")),
    };
    if !resolved.exists() {
        return Some("path does not exist".into());
    }
    if !resolved.is_dir() {
        return Some("path is not a directory".into());
    }
    match std::fs::read_dir(&resolved) {
        Ok(mut entries) => match entries.next() {
            Some(Err(e)) => Some(format!("cannot list directory: {e}")),
            _ => None,
        },
        Err(e) => Some(format!("cannot list directory: {e}")),
    }
}

fn rel_posix(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

pub fn scan(
    root: &Path,
    ignore: &IgnoreMatcher,
    opts: &ScanOptions,
) -> std::io::Result<ScanOutcome> {
    let root = docmonk_core::paths::resolve(root)?;
    let guard = PathGuard::new(std::slice::from_ref(&root))?;
    let unreadable: HashSet<PathBuf> = opts
        .inject_unreadable
        .iter()
        .filter_map(|p| docmonk_core::paths::resolve(p).ok())
        .collect();
    let mut out = ScanOutcome::default();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::from([root.clone()]);
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        if unreadable.contains(&dir) {
            out.errors.push(ScanError {
                path: dir.display().to_string(),
                message: "permission denied (injected)".into(),
            });
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                out.errors.push(ScanError {
                    path: dir.display().to_string(),
                    message: e.to_string(),
                });
                continue;
            }
        };
        let mut subdirs = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.errors.push(ScanError {
                        path: dir.display().to_string(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            let candidate = entry.path();
            // The entry's type comes with the directory listing (no stat
            // on most platforms); only links are stat'ed and resolved.
            let file_type = entry.file_type();
            let link = match &file_type {
                Ok(t) => t.is_symlink(),
                Err(_) => is_link(&candidate),
            };
            if link && !opts.follow_symlinks {
                continue;
            }
            let (is_dir, resolved) = if link {
                // Follows links when allowed: the target must stay inside
                // the root.
                let is_dir = std::fs::metadata(&candidate).is_ok_and(|m| m.is_dir());
                if ignore.is_ignored(&candidate, is_dir) {
                    continue;
                }
                let Ok(resolved) = guard.resolve(&candidate) else {
                    continue;
                };
                (is_dir, resolved)
            } else {
                // A plain entry of an already resolved directory is its
                // own real path, inside the root.
                let is_dir = match &file_type {
                    Ok(t) => t.is_dir(),
                    Err(_) => std::fs::metadata(&candidate).is_ok_and(|m| m.is_dir()),
                };
                if ignore.is_ignored(&candidate, is_dir) {
                    continue;
                }
                (is_dir, candidate)
            };
            if is_dir {
                if seen_dirs.insert(resolved.clone()) {
                    subdirs.push(resolved);
                }
                continue;
            }
            if !seen_files.insert(resolved.clone()) {
                continue;
            }
            // One stat per file: the entry's own (not following) for a
            // plain file, the target's for a followed link.
            let meta = if link {
                std::fs::metadata(&resolved)
            } else {
                entry.metadata()
            };
            match meta {
                Ok(meta) => out.files.push(ScannedFile {
                    rel_path: rel_posix(&root, &resolved),
                    size: meta.len() as i64,
                    mtime: mtime_secs(&meta),
                    path: resolved,
                }),
                Err(e) => out.errors.push(ScanError {
                    path: resolved.display().to_string(),
                    message: e.to_string(),
                }),
            }
        }
        // Reverse so the walk visits subdirectories in listing order.
        subdirs.reverse();
        stack.extend(subdirs);
    }
    Ok(out)
}

/// A scan of named paths only (a watcher's touched paths).
#[derive(Debug, Clone, Default)]
pub struct TargetedScan {
    /// Named paths that exist as regular, non-ignored files.
    pub present: ScanOutcome,
    /// Relative paths of named paths that no longer exist.
    pub missing: Vec<String>,
}

/// Examines exactly `targets` (`IndexCoordinator._run_targeted`). There
/// is no directory walk, so nothing can be silently truncated. A path is
/// skipped when it is a symlink (unless followed), escapes the root, is a
/// directory, or is ignored. A path that no longer exists is reported as
/// missing.
pub fn scan_targets(
    root: &Path,
    ignore: &IgnoreMatcher,
    targets: &std::collections::BTreeSet<PathBuf>,
    follow_symlinks: bool,
) -> std::io::Result<TargetedScan> {
    let root = docmonk_core::paths::resolve(root)?;
    let guard = PathGuard::new(std::slice::from_ref(&root))?;
    let mut out = TargetedScan::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for candidate in targets {
        if !follow_symlinks && is_link(candidate) {
            continue;
        }
        let Ok(resolved) = guard.resolve(candidate) else {
            continue;
        };
        if !seen.insert(resolved.clone()) {
            continue;
        }
        let meta = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(_) => {
                out.missing.push(rel_posix(&root, &resolved));
                continue;
            }
        };
        if meta.is_dir() || ignore.is_ignored(&resolved, false) {
            continue;
        }
        out.present.files.push(ScannedFile {
            rel_path: rel_posix(&root, &resolved),
            size: meta.len() as i64,
            mtime: mtime_secs(&meta),
            path: resolved,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_with_rules_and_reports_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("src/sub")).unwrap();
        std::fs::create_dir_all(r.join("node_modules/x")).unwrap();
        std::fs::write(r.join("src/a.py"), "x").unwrap();
        std::fs::write(r.join("src/sub/b.md"), "y").unwrap();
        std::fs::write(r.join("node_modules/x/c.js"), "z").unwrap();
        std::fs::write(r.join(".env"), "s").unwrap();
        let m = IgnoreMatcher::new(r, &[], &[]);
        let out = scan(r, &m, &ScanOptions::default()).unwrap();
        let mut rels: Vec<_> = out.files.iter().map(|f| f.rel_path.clone()).collect();
        rels.sort();
        assert_eq!(rels, vec!["src/a.py", "src/sub/b.md"]);
        assert!(out.complete());

        let out = scan(
            r,
            &m,
            &ScanOptions {
                inject_unreadable: vec![r.join("src/sub")],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!out.complete());
        assert_eq!(out.files.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_skipped_by_default_and_loops_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("d")).unwrap();
        std::fs::write(r.join("d/f.py"), "x").unwrap();
        std::os::unix::fs::symlink(r.join("d"), r.join("loop")).unwrap();
        std::os::unix::fs::symlink(r.join("d/f.py"), r.join("alias.py")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.py"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), r.join("escape")).unwrap();
        let m = IgnoreMatcher::new(r, &[], &[]);
        let out = scan(r, &m, &ScanOptions::default()).unwrap();
        assert_eq!(out.files.len(), 1);
        let follow = scan(
            r,
            &m,
            &ScanOptions {
                follow_symlinks: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            follow.files.len(),
            1,
            "same real file once; escapes rejected"
        );
    }

    #[test]
    fn root_accessibility() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_root_accessible(dir.path()).is_none());
        assert!(check_root_accessible(&dir.path().join("missing")).is_some());
        let f = dir.path().join("file");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(
            check_root_accessible(&f).as_deref(),
            Some("path is not a directory")
        );
    }
}
