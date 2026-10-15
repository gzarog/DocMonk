//! Incremental change detection (and the coordinator's classification/rename/delete reconciliation).
//!
//! * Metadata first: a file whose size and mtime match its baseline row is
//!   unchanged without being hashed; otherwise it is hashed and a matching
//!   hash still counts as unchanged (a `touch`).
//! * Rename: a new path whose hash matches exactly one baseline file that
//!   vanished from the scan, with the same kind, is a move (0 or 2+
//!   candidates, or a kind change, fall back to delete + new).
//! * Deletes are inferred only from a complete scan. An incomplete scan
//!   keeps every unseen baseline file.
//! * Racy clean: a matching stat is not trusted when the mtime falls
//!   within [`RACY_WINDOW_SECS`](crate::fingerprint::RACY_WINDOW_SECS) of
//!   the row's `last_indexed_at`; such a file is hashed.
//! * New files are not hashed here (unless a rename needs it): the worker
//!   that processes them hashes them in parallel.
//! * A baseline file in `retry` whose `next_attempt_at` is due is
//!   reprocessed; one not yet due is kept as is.
//!
//! A full rebuild passes an empty baseline, so every file is new.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use docmonk_core::models::FileKind;
use docmonk_storage::knowledge::FileRow;
use serde::Serialize;

use crate::classify::classify;
use crate::fingerprint::{racy, stat_unchanged};
use crate::scan::{ScanOutcome, ScannedFile};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    New,
    Changed,
    Unchanged,
    /// Unchanged content but the stored stat is stale (hash matched).
    UnchangedRestat,
    Moved,
    /// Baseline row not seen by an incomplete scan: kept, never deleted.
    Unseen,
    Deleted,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub change: Change,
    pub rel_path: String,
    pub kind: FileKind,
    pub scanned: Option<ScannedFile>,
    pub prev: Option<FileRow>,
    /// Baseline row a move came from.
    pub moved_from: Option<FileRow>,
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiffCounts {
    pub scanned: usize,
    pub new: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub moved: usize,
    pub deleted: usize,
    pub unseen_kept: usize,
    pub hash_calls: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiffResult {
    pub decisions: Vec<Decision>,
    pub counts: DiffCounts,
    pub scan_complete: bool,
}

impl DiffResult {
    /// Files that must be (re)processed.
    pub fn to_process(&self) -> impl Iterator<Item = &Decision> {
        self.decisions
            .iter()
            .filter(|d| matches!(d.change, Change::New | Change::Changed | Change::Moved))
    }
}

fn due(next_attempt_at: Option<&str>, now: &str) -> bool {
    next_attempt_at.is_none_or(|t| t <= now)
}

/// Scanned files [`diff`] will hash against `baseline`: stat mismatches,
/// racy rows, due retries, and every unknown path when a vanished
/// baseline row makes a rename possible. Hashing them up front on a
/// bounded pool keeps the classify step off a single thread.
pub fn hash_candidates<'a>(
    scan: &'a ScanOutcome,
    baseline: &[FileRow],
    now_iso: &str,
) -> Vec<&'a ScannedFile> {
    let by_path: HashMap<&str, &FileRow> =
        baseline.iter().map(|r| (r.rel_path.as_str(), r)).collect();
    let scanned: std::collections::HashSet<&str> =
        scan.files.iter().map(|f| f.rel_path.as_str()).collect();
    let renames_possible = baseline
        .iter()
        .any(|r| r.content_hash.is_some() && !scanned.contains(r.rel_path.as_str()));
    scan.files
        .iter()
        .filter(|f| match by_path.get(f.rel_path.as_str()) {
            None => renames_possible,
            Some(p) => {
                (p.status == "retry" && due(p.next_attempt_at.as_deref(), now_iso))
                    || !stat_unchanged(p.size, p.mtime, f.size, f.mtime)
                    || racy(f.mtime, p.last_indexed_at.as_deref())
            }
        })
        .collect()
}

/// `hash` hashes one scanned file; errors are reported as `Changed` with
/// no hash (the processor will then surface the I/O error per file).
pub fn diff(
    scan: &ScanOutcome,
    baseline: &[FileRow],
    now_iso: &str,
    hash: &mut dyn FnMut(&Path) -> std::io::Result<String>,
) -> DiffResult {
    let mut counts = DiffCounts {
        scanned: scan.files.len(),
        ..DiffCounts::default()
    };
    let mut hashes: HashMap<String, String> = HashMap::new();
    let mut hash_of = |f: &ScannedFile, counts: &mut DiffCounts| -> Option<String> {
        if let Some(h) = hashes.get(&f.rel_path) {
            return Some(h.clone());
        }
        counts.hash_calls += 1;
        let h = hash(&f.path).ok()?;
        hashes.insert(f.rel_path.clone(), h.clone());
        Some(h)
    };
    let mut by_path: BTreeMap<&str, &FileRow> =
        baseline.iter().map(|r| (r.rel_path.as_str(), r)).collect();
    let scanned_paths: std::collections::HashSet<&str> =
        scan.files.iter().map(|f| f.rel_path.as_str()).collect();

    // Rename candidates: baseline rows missing from the scan, by hash.
    let mut missing_by_hash: HashMap<String, Vec<&FileRow>> = HashMap::new();
    for r in baseline {
        if !scanned_paths.contains(r.rel_path.as_str()) {
            if let Some(h) = &r.content_hash {
                missing_by_hash.entry(h.clone()).or_default().push(r);
            }
        }
    }
    let renames_possible = !missing_by_hash.is_empty();
    let mut decisions = Vec::new();
    let mut consumed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut moved_targets: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !missing_by_hash.is_empty() {
        for f in &scan.files {
            if by_path.contains_key(f.rel_path.as_str()) {
                continue;
            }
            let Some(h) = hash_of(f, &mut counts) else {
                continue;
            };
            let Some(cands) = missing_by_hash.get_mut(&h) else {
                continue;
            };
            if cands.len() != 1 {
                continue;
            }
            let kind = classify(&f.path);
            if cands[0].kind != kind.as_str() {
                continue;
            }
            let old = cands.pop().expect("one candidate");
            consumed.insert(old.rel_path.clone());
            moved_targets.insert(f.rel_path.clone());
            // A move is also reported as unchanged content. (It is still
            // re-extracted: stable IDs derive from the path.)
            counts.moved += 1;
            counts.unchanged += 1;
            decisions.push(Decision {
                change: Change::Moved,
                rel_path: f.rel_path.clone(),
                kind,
                scanned: Some(f.clone()),
                prev: None,
                moved_from: Some(old.clone()),
                content_hash: Some(h),
            });
        }
    }

    // Deletions (complete scans only) and unseen rows (incomplete scans).
    for r in baseline {
        if scanned_paths.contains(r.rel_path.as_str()) || consumed.contains(&r.rel_path) {
            continue;
        }
        let kind = r.kind.parse().unwrap_or(FileKind::Unknown);
        let change = if scan.complete() {
            Change::Deleted
        } else {
            Change::Unseen
        };
        if change == Change::Deleted {
            counts.deleted += 1;
        } else {
            counts.unseen_kept += 1;
        }
        decisions.push(Decision {
            change,
            rel_path: r.rel_path.clone(),
            kind,
            scanned: None,
            prev: Some(r.clone()),
            moved_from: None,
            content_hash: r.content_hash.clone(),
        });
    }
    by_path.retain(|p, _| !consumed.contains(*p));

    for f in &scan.files {
        if moved_targets.contains(&f.rel_path) {
            continue;
        }
        let kind = classify(&f.path);
        let prev = by_path.get(f.rel_path.as_str()).copied();
        let (change, content_hash) = match prev {
            // Hashed by the processing worker unless a rename check
            // already did.
            None if renames_possible => (Change::New, hash_of(f, &mut counts)),
            None => (Change::New, None),
            Some(p) if p.status == "retry" && due(p.next_attempt_at.as_deref(), now_iso) => {
                (Change::Changed, hash_of(f, &mut counts))
            }
            Some(p)
                if stat_unchanged(p.size, p.mtime, f.size, f.mtime)
                    && !racy(f.mtime, p.last_indexed_at.as_deref()) =>
            {
                (Change::Unchanged, p.content_hash.clone())
            }
            Some(p) => {
                let h = hash_of(f, &mut counts);
                if h.is_some() && h == p.content_hash {
                    (Change::UnchangedRestat, h)
                } else {
                    (Change::Changed, h)
                }
            }
        };
        match change {
            Change::New => counts.new += 1,
            Change::Changed => counts.changed += 1,
            _ => counts.unchanged += 1,
        }
        decisions.push(Decision {
            change,
            rel_path: f.rel_path.clone(),
            kind,
            scanned: Some(f.clone()),
            prev: prev.cloned(),
            moved_from: None,
            content_hash,
        });
    }
    DiffResult {
        decisions,
        counts,
        scan_complete: scan.complete(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScanError;
    use std::path::PathBuf;

    fn sf(rel: &str, size: i64, mtime: f64) -> ScannedFile {
        ScannedFile {
            path: PathBuf::from(format!("/r/{rel}")),
            rel_path: rel.into(),
            size,
            mtime,
        }
    }

    fn row(rel: &str, size: i64, mtime: f64, hash: &str, kind: &str) -> FileRow {
        FileRow {
            id: rel.into(),
            rel_path: rel.into(),
            kind: kind.into(),
            size,
            mtime,
            content_hash: Some(hash.into()),
            status: "indexed".into(),
            ..FileRow::default()
        }
    }

    fn hasher(
        map: &'static [(&'static str, &'static str)],
    ) -> impl FnMut(&Path) -> std::io::Result<String> {
        move |p: &Path| {
            let rel = p.strip_prefix("/r").unwrap().to_string_lossy().into_owned();
            Ok(map
                .iter()
                .find(|(k, _)| *k == rel)
                .map(|(_, v)| (*v).to_string())
                .unwrap_or_else(|| "?".into()))
        }
    }

    #[test]
    fn metadata_first_then_hash() {
        let scan = ScanOutcome {
            files: vec![
                sf("same.py", 1, 1.0),
                sf("touched.py", 1, 2.0),
                sf("edited.py", 2, 2.0),
                sf("new.py", 1, 1.0),
            ],
            errors: vec![],
        };
        let base = vec![
            row("same.py", 1, 1.0, "a", "code"),
            row("touched.py", 1, 1.0, "b", "code"),
            row("edited.py", 1, 1.0, "c", "code"),
            row("gone.py", 1, 1.0, "d", "code"),
        ];
        let mut h = hasher(&[("touched.py", "b"), ("edited.py", "c2"), ("new.py", "n")]);
        let d = diff(&scan, &base, "now", &mut h);
        assert_eq!(
            d.counts,
            DiffCounts {
                scanned: 4,
                new: 1,
                changed: 1,
                unchanged: 2,
                moved: 0,
                deleted: 1,
                unseen_kept: 0,
                hash_calls: 3
            }
        );
        let touched = d
            .decisions
            .iter()
            .find(|x| x.rel_path == "touched.py")
            .unwrap();
        assert_eq!(touched.change, Change::UnchangedRestat);
    }

    #[test]
    fn renames_need_a_unique_same_kind_match() {
        let scan = ScanOutcome {
            files: vec![sf("b/moved.py", 1, 5.0), sf("x.md", 1, 5.0)],
            errors: vec![],
        };
        let base = vec![
            row("a/old.py", 1, 1.0, "h", "code"),
            row("y.py", 1, 1.0, "k", "code"),
        ];
        let mut h = hasher(&[("b/moved.py", "h"), ("x.md", "k")]);
        let d = diff(&scan, &base, "now", &mut h);
        assert_eq!(d.counts.moved, 1, "same hash, same kind");
        assert_eq!(d.counts.unchanged, 1, "a move counts as unchanged content");
        assert_eq!(d.counts.new, 1, "kind change is not a move");
        assert_eq!(d.counts.deleted, 1);
        let m = d
            .decisions
            .iter()
            .find(|x| x.change == Change::Moved)
            .unwrap();
        assert_eq!(m.moved_from.as_ref().unwrap().rel_path, "a/old.py");

        // Ambiguous: two vanished files share the hash.
        let base = vec![
            row("a.py", 1, 1.0, "h", "code"),
            row("b.py", 1, 1.0, "h", "code"),
        ];
        let scan = ScanOutcome {
            files: vec![sf("c.py", 1, 5.0)],
            errors: vec![],
        };
        let mut h = hasher(&[("c.py", "h")]);
        let d = diff(&scan, &base, "now", &mut h);
        assert_eq!((d.counts.moved, d.counts.new, d.counts.deleted), (0, 1, 2));
    }

    #[test]
    fn incomplete_scan_never_deletes() {
        let scan = ScanOutcome {
            files: vec![sf("a.py", 1, 1.0)],
            errors: vec![ScanError {
                path: "/r/sub".into(),
                message: "denied".into(),
            }],
        };
        let base = vec![
            row("a.py", 1, 1.0, "a", "code"),
            row("sub/b.py", 1, 1.0, "b", "code"),
        ];
        let mut h = hasher(&[]);
        let d = diff(&scan, &base, "now", &mut h);
        assert_eq!((d.counts.deleted, d.counts.unseen_kept), (0, 1));
        assert!(!d.scan_complete);
    }

    #[test]
    fn retry_rows_are_reprocessed_only_when_due() {
        let scan = ScanOutcome {
            files: vec![sf("a.py", 1, 1.0), sf("b.py", 1, 1.0)],
            errors: vec![],
        };
        let mut a = row("a.py", 1, 1.0, "a", "code");
        a.status = "retry".into();
        a.next_attempt_at = Some("2000".into());
        let mut b = row("b.py", 1, 1.0, "b", "code");
        b.status = "retry".into();
        b.next_attempt_at = Some("3000".into());
        let mut h = hasher(&[("a.py", "a")]);
        let d = diff(&scan, &[a, b], "2500", &mut h);
        assert_eq!((d.counts.changed, d.counts.unchanged), (1, 1));
    }

    #[test]
    fn full_rebuild_baseline_is_empty_so_everything_is_new() {
        let scan = ScanOutcome {
            files: vec![sf("a.py", 1, 1.0), sf("b.md", 1, 1.0)],
            errors: vec![],
        };
        let mut h = hasher(&[]);
        let d = diff(&scan, &[], "now", &mut h);
        assert_eq!(
            (d.counts.new, d.counts.unchanged, d.counts.deleted),
            (2, 0, 0)
        );
    }
}
