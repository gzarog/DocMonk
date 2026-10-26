//! Polling for new commits on git sources (ADR 0043). A checkout only
//! changes when a pass fetches it, so instead of filesystem events each
//! tick asks the remote for the branch head (`git ls-remote`, no objects
//! transferred) and reports when it differs from the checked-out commit.
//! A gone branch is reported too, so the pass records why the source is
//! offline.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub type OnMoved = Arc<dyn Fn() + Send + Sync>;

/// Whether the remote branch head differs from what is checked out.
/// Unreachable remotes are not a change: the next reconciliation pass
/// records the failure.
pub fn moved(remote_dir: &std::path::Path) -> bool {
    let Ok(meta) = crate::git::read_meta(remote_dir) else {
        return false;
    };
    match crate::git::remote_head(remote_dir) {
        Ok(head) => head != meta.commit,
        Err(_) => false,
    }
}

/// Polls one git source every `interval`.
pub struct GitWatcher {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for GitWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitWatcher").finish_non_exhaustive()
    }
}

impl GitWatcher {
    pub fn start(
        remote_dir: PathBuf,
        interval: Duration,
        on_moved: OnMoved,
    ) -> std::io::Result<Self> {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let s = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("docmonk-git-poll".into())
            .spawn(move || loop {
                let g = s.0.lock().unwrap_or_else(|p| p.into_inner());
                let (g, _) =
                    s.1.wait_timeout_while(g, interval, |stopped| !*stopped)
                        .unwrap_or_else(|p| p.into_inner());
                if *g {
                    return;
                }
                drop(g);
                if moved(&remote_dir) {
                    on_moved();
                }
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

impl Drop for GitWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}: {o:?}");
    }

    fn commit(work: &Path, name: &str) {
        std::fs::write(work.join(name), name).unwrap();
        git(work, &["add", "."]);
        git(work, &["commit", "-q", "-m", name]);
        git(work, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    }

    #[test]
    fn a_moved_branch_is_seen_and_fetched() {
        let dir = tempfile::tempdir().unwrap();
        let (bare, work, home) = (
            dir.path().join("r.git"),
            dir.path().join("w"),
            dir.path().join("h"),
        );
        std::fs::create_dir_all(&work).unwrap();
        git(
            dir.path(),
            &["init", "--bare", "-q", bare.to_str().unwrap()],
        );
        git(&work, &["init", "-q"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        commit(&work, "a.txt");
        let url = bare.to_string_lossy().into_owned();
        let root = crate::git::clone(&home, &url, Some("main"), None).unwrap();
        let remote = crate::git::remote_dir_of(&root).unwrap();
        assert!(!moved(&remote), "fresh clone is current");
        commit(&work, "b.txt");
        assert!(moved(&remote));
        let meta = crate::git::sync(&remote).unwrap();
        assert!(root.join("b.txt").is_file());
        assert!(!moved(&remote));
        assert_eq!(meta.last_error, None);
    }
}
