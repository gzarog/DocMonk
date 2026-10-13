//! Native filesystem events for one local root
//! through `notify`. Every
//! file-level create, modify, remove or rename (both halves) is debounced
//! per path, then reported. Directory events are dropped, since a
//! targeted pass skips directories anyway. When native watching cannot
//! start (no inotify instances left, an unsupported filesystem), the
//! watcher falls back to `notify`'s polling watcher.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};

use super::debounce::Debouncer;

pub type OnPath = Arc<dyn Fn(PathBuf) + Send + Sync>;

/// Which backend a [`LocalWatcher`] ended up with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Native,
    Polling,
}

pub struct LocalWatcher {
    // Field order matters on drop: stop events before the debouncer.
    _watcher: Box<dyn Watcher + Send>,
    debouncer: Arc<Debouncer>,
    backend: Backend,
}

impl std::fmt::Debug for LocalWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalWatcher")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

fn relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

impl LocalWatcher {
    /// Watches `root` recursively. Fails when the root cannot be watched
    /// at all, for example because it is missing (the daemon retries at
    /// reconciliation). `poll_interval` paces the polling fallback.
    pub fn start(
        root: &Path,
        debounce_ms: i64,
        poll_interval: Duration,
        force_polling: bool,
        on_path: OnPath,
    ) -> notify::Result<Self> {
        if !root.is_dir() {
            return Err(notify::Error::path_not_found().add_path(root.to_path_buf()));
        }
        let debouncer = Arc::new(Debouncer::new(debounce_ms, move |key| {
            on_path(PathBuf::from(key));
        }));
        let handler = handler_for(Arc::clone(&debouncer));
        let (mut watcher, backend): (Box<dyn Watcher + Send>, Backend) = if force_polling {
            (polling(handler, poll_interval)?, Backend::Polling)
        } else {
            match notify::recommended_watcher(handler.clone()) {
                Ok(w) => (Box::new(w), Backend::Native),
                Err(e) => {
                    tracing::warn!(component = "watcher", event = "native_watch_unavailable", root = %root.display(), error = %e);
                    (polling(handler, poll_interval)?, Backend::Polling)
                }
            }
        };
        if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
            if backend == Backend::Polling {
                return Err(e);
            }
            // The native backend can fail per root (inotify watch limit).
            tracing::warn!(component = "watcher", event = "native_watch_failed", root = %root.display(), error = %e);
            let mut w = polling(handler_for(Arc::clone(&debouncer)), poll_interval)?;
            w.watch(root, RecursiveMode::Recursive)?;
            return Ok(Self {
                _watcher: w,
                debouncer,
                backend: Backend::Polling,
            });
        }
        Ok(Self {
            _watcher: watcher,
            debouncer,
            backend,
        })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Drops debounced paths not yet due (shutdown).
    pub fn flush(&self) {
        self.debouncer.flush();
    }
}

/// Forwards file-level events to the debouncer. A path that is a
/// directory right now carries nothing a contained file event would not.
/// A removed path cannot be checked, so it is forwarded.
fn handler_for(d: Arc<Debouncer>) -> impl notify::EventHandler + Clone {
    move |res: notify::Result<Event>| {
        let Ok(event) = res else {
            return;
        };
        if !relevant(&event.kind) {
            return;
        }
        for p in event.paths.into_iter().filter(|p| !p.is_dir()) {
            d.notify(p.to_string_lossy().into_owned());
        }
    }
}

fn polling(
    handler: impl notify::EventHandler,
    interval: Duration,
) -> notify::Result<Box<dyn Watcher + Send>> {
    let cfg = notify::Config::default()
        .with_poll_interval(interval)
        .with_compare_contents(false);
    Ok(Box::new(notify::PollWatcher::new(handler, cfg)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn wait_for(seen: &Mutex<Vec<PathBuf>>, name: &str) -> bool {
        for _ in 0..250 {
            if seen
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == name))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn check(force_polling: bool) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("r");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let seen: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let s = Arc::clone(&seen);
        let w = LocalWatcher::start(
            &root,
            50,
            Duration::from_millis(100),
            force_polling,
            Arc::new(move |p| s.lock().unwrap().push(p)),
        )
        .unwrap();
        assert_eq!(
            w.backend(),
            if force_polling {
                Backend::Polling
            } else {
                Backend::Native
            }
        );
        // Give the backend a moment to take its baseline.
        std::thread::sleep(Duration::from_millis(250));
        std::fs::write(root.join("sub/new.py"), "x = 1\n").unwrap();
        assert!(wait_for(&seen, "new.py"), "{force_polling}: {seen:?}");
        // Directories themselves are never reported.
        assert!(!seen.lock().unwrap().iter().any(|p| p.ends_with("sub")));
    }

    #[test]
    fn native_reports_file_events() {
        check(false);
    }

    #[test]
    fn polling_fallback_reports_file_events() {
        check(true);
    }

    #[test]
    fn missing_root_fails() {
        let dir = tempfile::tempdir().unwrap();
        let r = LocalWatcher::start(
            &dir.path().join("missing"),
            10,
            Duration::from_millis(100),
            false,
            Arc::new(|_| {}),
        );
        assert!(r.is_err());
    }
}
