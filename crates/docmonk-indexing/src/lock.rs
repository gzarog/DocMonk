//! Bounded cross-process run lock.
//!
//! The OS lock (`std::fs::File::try_lock`: `flock` on POSIX, `LockFileEx`
//! on Windows) is authoritative. Owner metadata is diagnostic only,
//! sanitized, and written after byte 0; the lock file is never deleted or force-unlocked based on it. On
//! Windows the whole-file lock makes a blocked process unable to read the
//! metadata, so it reports the owner as unknown.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use docmonk_core::errors::{DocMonkError, LockOwner};
use serde_json::{json, Value};

const POLL: Duration = Duration::from_millis(50);
const METADATA_OFFSET: u64 = 1;
const METADATA_MAX_BYTES: u64 = 4096;
const METADATA_SCHEMA_VERSION: u64 = 1;

/// Keeps only a short identifier-like token (never a URL/argv/secret).
pub fn sanitize_token(value: &str, limit: usize) -> Option<String> {
    let s: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.:-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(limit)
        .collect();
    (!s.is_empty()).then_some(s)
}

fn sanitize_timestamp(value: &str) -> Option<String> {
    let s: String = value
        .chars()
        .filter(|c| c.is_ascii_digit() || "T.:+-".contains(*c))
        .take(40)
        .collect();
    (!s.is_empty()).then_some(s)
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_default()
}

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Never truncate on open: another process may hold the lock.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// `locks/index-<source_id>.lock`: one pass per source at a time (ADR
/// 0033). Whole-home operations take `locks/index.lock` and then every
/// source lock in id order.
pub fn source_lock_path(home: &docmonk_core::paths::Home, source_id: &str) -> PathBuf {
    let safe: String = source_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    home.locks_dir().join(format!("index-{safe}.lock"))
}

/// Defensively reads owner metadata; `None` if absent/malformed/unreadable.
pub fn read_lock_owner(path: &Path) -> Option<LockOwner> {
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::Start(METADATA_OFFSET)).ok()?;
    let mut raw = Vec::new();
    file.take(METADATA_MAX_BYTES).read_to_end(&mut raw).ok()?;
    let data: Value = serde_json::from_slice(&raw).ok()?;
    let obj = data.as_object()?;
    let mut owner = LockOwner::default();
    if let Some(pid) = obj.get("pid").and_then(Value::as_i64).filter(|p| *p > 0) {
        owner.pid = Some(pid.to_string());
    }
    let token = |k: &str| {
        obj.get(k)
            .and_then(Value::as_str)
            .and_then(|v| sanitize_token(v, 64))
    };
    owner.operation = token("operation");
    owner.source_id = token("source_id");
    owner.hostname = token("hostname");
    owner.acquired_at = obj
        .get("acquired_at")
        .and_then(Value::as_str)
        .and_then(sanitize_timestamp);
    owner.present = owner.pid.is_some()
        || owner.operation.is_some()
        || owner.source_id.is_some()
        || owner.hostname.is_some()
        || owner.acquired_at.is_some();
    owner.present.then_some(owner)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum LockState {
    Free,
    Held(Option<LockOwner>),
    Unknown,
}

/// Non-blocking, non-disturbing inspection (for doctor/status).
pub fn inspect_lock(path: &Path) -> LockState {
    if !path.exists() {
        return LockState::Free;
    }
    let Ok(file) = open_lock_file(path) else {
        return LockState::Unknown;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            LockState::Free
        }
        Err(std::fs::TryLockError::WouldBlock) => LockState::Held(read_lock_owner(path)),
        Err(_) => LockState::Unknown,
    }
}

/// A held lock; released on drop.
#[derive(Debug)]
pub struct RunLock {
    file: Option<File>,
    path: PathBuf,
}

impl RunLock {
    /// Acquires within `timeout`, else fails with a
    /// `RunLockTimeoutError` message (including diagnostic owner info).
    pub fn acquire(
        path: &Path,
        operation: &str,
        source_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, DocMonkError> {
        let io_err = |e: std::io::Error| {
            DocMonkError::new(
                docmonk_core::ErrorKind::Generic,
                format!("cannot open lock {}: {e}", path.display()),
            )
        };
        let file = open_lock_file(path).map_err(io_err)?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        let owner = read_lock_owner(path).unwrap_or_default();
                        tracing::warn!(component = "lock", event = "lock_wait_timeout", lock = %path.display());
                        return Err(DocMonkError::run_lock_timeout(
                            &path.display().to_string(),
                            timeout.as_secs_f64(),
                            &owner,
                        ));
                    }
                    std::thread::sleep(POLL);
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(io_err(e)),
            }
        }
        let meta = json!({
            "schema_version": METADATA_SCHEMA_VERSION,
            "pid": std::process::id(),
            "operation": sanitize_token(operation, 64),
            "source_id": source_id.and_then(|s| sanitize_token(s, 64)),
            "hostname": sanitize_token(&hostname(), 64),
            "acquired_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        });
        let mut lock = Self {
            file: Some(file),
            path: path.to_path_buf(),
        };
        // Diagnostics only: never fail a lock we already hold.
        let _ = lock.write_metadata(&meta.to_string());
        tracing::info!(component = "lock", event = "lock_acquired", lock = %path.display());
        Ok(lock)
    }

    fn write_metadata(&mut self, text: &str) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.set_len(METADATA_OFFSET)?;
            f.seek(SeekFrom::Start(METADATA_OFFSET))?;
            f.write_all(text.as_bytes())?;
            f.flush()?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(f) = self.file.take() {
            let _ = f.set_len(METADATA_OFFSET);
            let _ = f.unlock();
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        self.release_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_bounded_and_diagnosable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("locks").join("index.lock");
        let held = RunLock::acquire(&p, "index", Some("src_1"), Duration::from_secs(1)).unwrap();
        // Windows' whole-file lock hides metadata from other handles.
        if cfg!(windows) {
            assert!(read_lock_owner(&p).is_none());
        } else {
            let owner = read_lock_owner(&p).unwrap();
            assert_eq!(owner.operation.as_deref(), Some("index"));
            assert_eq!(owner.source_id.as_deref(), Some("src_1"));
            assert_eq!(owner.pid, Some(std::process::id().to_string()));
        }

        let started = Instant::now();
        let err = RunLock::acquire(&p, "rebuild", None, Duration::from_millis(200)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "bounded wait");
        assert!(
            err.message()
                .contains("Another DocMonk process holds index.lock"),
            "{}",
            err.message()
        );
        assert!(matches!(inspect_lock(&p), LockState::Held(_)));

        held.release();
        assert_eq!(inspect_lock(&p), LockState::Free);
        assert!(read_lock_owner(&p).is_none(), "metadata cleared on release");
        let again = RunLock::acquire(&p, "index", None, Duration::from_millis(200)).unwrap();
        drop(again);
        assert_eq!(inspect_lock(&p), LockState::Free);
    }

    #[test]
    fn metadata_is_sanitized() {
        assert_eq!(
            sanitize_token("https://u:p@h/x y", 64).unwrap(),
            "https:__u:p_h_x_y"
        );
        assert_eq!(sanitize_token(&"a".repeat(100), 64).unwrap().len(), 64);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l");
        std::fs::write(&p, b"\x00{\"pid\": -3, \"operation\": \"x;rm -rf\", \"acquired_at\": \"2026-01-01T00:00:00+00:00<script>\"}").unwrap();
        let o = read_lock_owner(&p).unwrap();
        assert_eq!(o.pid, None);
        assert_eq!(o.operation.as_deref(), Some("x_rm_-rf"));
        assert_eq!(o.acquired_at.as_deref(), Some("2026-01-01T00:00:00+00:00"));
    }
}
