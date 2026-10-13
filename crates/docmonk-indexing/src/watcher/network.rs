//! Polling change detection for network roots
//! Native events are not reliably delivered
//! across a network mount, so each tick fingerprints the tree and reports
//! the paths that appeared, disappeared, or changed size or mtime.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ignore::IgnoreMatcher;
use crate::scan::{check_root_accessible, scan, ScanOptions};

/// Absolute path to `(size, mtime)`.
pub type Fingerprint = BTreeMap<String, (i64, f64)>;

/// One snapshot of every file under `root`. Empty when the root is
/// unreachable: the pass this triggers decides what offline means.
pub fn fingerprint(
    root: &Path,
    include: &[String],
    exclude: &[String],
    follow_symlinks: bool,
) -> Fingerprint {
    if check_root_accessible(root).is_some() {
        return Fingerprint::new();
    }
    let Ok(resolved) = docmonk_core::paths::resolve(root) else {
        return Fingerprint::new();
    };
    let ignore = IgnoreMatcher::new(&resolved, exclude, include);
    let opts = ScanOptions {
        follow_symlinks,
        ..ScanOptions::default()
    };
    scan(root, &ignore, &opts)
        .map(|o| {
            o.files
                .into_iter()
                .map(|f| (f.path.to_string_lossy().into_owned(), (f.size, f.mtime)))
                .collect()
        })
        .unwrap_or_default()
}

/// Paths that appeared, disappeared or changed between two fingerprints.
pub fn diff(previous: &Fingerprint, current: &Fingerprint) -> BTreeSet<String> {
    let mut changed: BTreeSet<String> = current
        .iter()
        .filter(|(p, stat)| previous.get(*p) != Some(stat))
        .map(|(p, _)| p.clone())
        .collect();
    changed.extend(
        previous
            .keys()
            .filter(|p| !current.contains_key(*p))
            .cloned(),
    );
    changed
}

pub type OnChange = Arc<dyn Fn(Vec<PathBuf>) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct NetworkSpec {
    pub root: PathBuf,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub follow_symlinks: bool,
    pub interval: Duration,
}

struct Poller {
    spec: NetworkSpec,
    last: Fingerprint,
    on_change: OnChange,
}

impl Poller {
    fn poll_once(&mut self) -> bool {
        let s = &self.spec;
        let current = fingerprint(&s.root, &s.include, &s.exclude, s.follow_symlinks);
        let changed = diff(&self.last, &current);
        self.last = current;
        if changed.is_empty() {
            return false;
        }
        (self.on_change)(changed.into_iter().map(PathBuf::from).collect());
        true
    }
}

/// Polls one network root every `interval`. At most one callback per tick,
/// and only when something changed.
pub struct NetworkWatcher {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for NetworkWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkWatcher").finish_non_exhaustive()
    }
}

impl NetworkWatcher {
    /// Takes the baseline fingerprint now, then polls on a thread.
    pub fn start(spec: NetworkSpec, on_change: OnChange) -> std::io::Result<Self> {
        let last = fingerprint(
            &spec.root,
            &spec.include,
            &spec.exclude,
            spec.follow_symlinks,
        );
        let interval = spec.interval;
        let mut poller = Poller {
            spec,
            last,
            on_change,
        };
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let s = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("ragmonk-network-poll".into())
            .spawn(move || loop {
                let g = s.0.lock().unwrap_or_else(|p| p.into_inner());
                let (g, _) =
                    s.1.wait_timeout_while(g, interval, |stopped| !*stopped)
                        .unwrap_or_else(|p| p.into_inner());
                if *g {
                    return;
                }
                drop(g);
                poller.poll_once();
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn stop(&mut self) {
        *self.stop.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.stop.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for NetworkWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polls_and_reports_exact_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        let seen: Arc<Mutex<Vec<Vec<PathBuf>>>> = Arc::default();
        let s = Arc::clone(&seen);
        let mut w = NetworkWatcher::start(
            NetworkSpec {
                root: root.clone(),
                include: vec![],
                exclude: vec![],
                follow_symlinks: false,
                interval: Duration::from_millis(50),
            },
            Arc::new(move |p| s.lock().unwrap().push(p)),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(seen.lock().unwrap().is_empty(), "no change, no trigger");
        std::fs::write(root.join("a.txt"), "aaaa").unwrap();
        std::fs::remove_file(root.join("b.txt")).unwrap();
        // A tick may land between the two changes, so they can arrive in
        // one callback or two; together they must be exactly these paths.
        let names = || -> BTreeSet<String> {
            seen.lock()
                .unwrap()
                .iter()
                .flatten()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        for _ in 0..100 {
            if names().len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        w.stop();
        assert_eq!(
            names(),
            BTreeSet::from(["a.txt".to_owned(), "b.txt".to_owned()])
        );
        assert!(
            seen.lock().unwrap().len() <= 2,
            "at most one callback per tick"
        );
        assert!(!w.is_alive());
    }

    #[test]
    fn unreachable_root_fingerprints_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fingerprint(&dir.path().join("missing"), &[], &[], false).is_empty());
    }
}
