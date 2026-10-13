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
}
