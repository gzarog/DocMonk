//! Content hashing and the mtime+size fast-skip.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

const CHUNK: usize = 1 << 20;
pub const MTIME_EPSILON: f64 = 1e-6;

/// Streaming sha256 (the default and only supported `indexing.hash_algorithm`).
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// [`hash_file`] of bytes already in memory (the same digest).
pub fn hash_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn stat_unchanged(prev_size: i64, prev_mtime: f64, size: i64, mtime: f64) -> bool {
    prev_size == size && (prev_mtime - mtime).abs() < MTIME_EPSILON
}

/// Seconds around `last_indexed_at` in which a matching stat proves
/// nothing: a file rewritten within the filesystem's timestamp
/// granularity of being indexed can keep its size and mtime.
pub const RACY_WINDOW_SECS: f64 = 2.0;

/// The racy-clean rule (as in git): a file whose mtime is not clearly
/// older than the moment its row was written may have changed again in
/// the same timestamp tick, so its stat cannot be trusted and it must be
/// hashed. Unknown or unparseable times are not racy.
pub fn racy(mtime: f64, last_indexed_at: Option<&str>) -> bool {
    let Some(at) = last_indexed_at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    else {
        return false;
    };
    let indexed = at.timestamp() as f64 + f64::from(at.timestamp_subsec_micros()) / 1e6;
    mtime + RACY_WINDOW_SECS > indexed
}

/// Seconds since the epoch, as a float.
pub fn mtime_secs(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_fast_skip() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            hash_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(stat_unchanged(3, 1.0, 3, 1.0 + 5e-7));
        assert!(!stat_unchanged(3, 1.0, 3, 1.0 + 2e-6));
        assert!(!stat_unchanged(3, 1.0, 4, 1.0));
    }

    #[test]
    fn racy_only_near_the_indexing_time() {
        let at = "2026-01-01T00:00:10+00:00";
        let t = 1_767_225_610.0;
        assert!(racy(t - 1.0, Some(at)), "written 1 s before indexing");
        assert!(racy(t + 3.0, Some(at)), "written after indexing");
        assert!(!racy(t - 5.0, Some(at)), "clearly older");
        assert!(!racy(t, None));
        assert!(!racy(t, Some("garbage")));
    }
}
