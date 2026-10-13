//! File-based conversion cache for expensive conversions (PDF, OCR).
//!
//! Keyed by content hash, the effective settings key and the converter and
//! OCR identities, so a settings or version change never serves a stale
//! result. Entries are written atomically (temp file + rename), so
//! concurrent workers and crashes never expose a partial entry; a corrupt
//! entry is ignored and rebuilt.

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct ConversionCache {
    dir: PathBuf,
}

impl ConversionCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, content_hash: &str, key: &str) -> PathBuf {
        let digest = format!(
            "{:x}",
            Sha256::digest(format!("{content_hash}\0{key}").as_bytes())
        );
        self.dir.join(&digest[..2]).join(format!("{digest}.json"))
    }

    pub fn get(&self, content_hash: &str, key: &str) -> Option<Value> {
        let bytes = std::fs::read(self.path(content_hash, key)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn put(&self, content_hash: &str, key: &str, value: &Value) {
        let path = self.path(content_hash, key);
        let write = || -> std::io::Result<()> {
            let parent = path.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            let tmp = parent.join(format!(".{}.tmp", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec(value).unwrap_or_default())?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            tracing::warn!(component = "conversion_cache", event = "cache_write_failed", error = %e);
        }
    }
}
