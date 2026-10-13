//! The single storage-mode resolver (ADR 0033).
//!
//! [`open`] turns `storage.mode` into a [`Backend`]: the local SQLite
//! control plane, or a connected and schema-verified
//! OpenSearch/Elasticsearch backend. CLI, Admin UI, MCP and daemon all reach
//! storage through it. In server mode nothing local is consulted for the
//! catalog, build state or knowledge: an unreachable cluster is a typed
//! `BackendUnavailableError`, never a fallback to SQLite.

use std::sync::Arc;
use std::time::Duration;

use docmonk_backends::backend::CatalogEntry;
use docmonk_backends::engine::default_vector_spec;
use docmonk_backends::ServerBackend;
use docmonk_config::RagMonkConfig;
use docmonk_core::errors::{ErrorKind, RagMonkError};
use docmonk_core::models::SourceType;
use docmonk_core::paths::Home;
use docmonk_storage::control::{ControlPlane, SourceRecord};
use docmonk_storage::StorageLayout;

use crate::{db, load};

/// The storage a process works against.
pub enum Backend {
    Local,
    Server(Arc<ServerBackend>),
}

impl Backend {
    pub fn is_server(&self) -> bool {
        matches!(self, Backend::Server(_))
    }

    /// `"local"`, `"opensearch"` or `"elasticsearch"`.
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Local => "local",
            Backend::Server(s) => s.engine().as_str(),
        }
    }

    pub fn server(&self) -> Option<&Arc<ServerBackend>> {
        match self {
            Backend::Server(s) => Some(s),
            Backend::Local => None,
        }
    }
}

/// A server error as a [`RagMonkError`] (redacted, classed).
pub fn server_err(e: docmonk_backends::BackendError) -> RagMonkError {
    e.into()
}

/// Connects to the configured cluster (engine detection and a reachability
/// check) without touching any index.
pub fn connect(cfg: &RagMonkConfig) -> Result<ServerBackend, RagMonkError> {
    let s = &cfg.storage.server;
    if s.url.is_empty() {
        return Err(RagMonkError::config(
            "storage.mode is 'server' but storage.server.url is empty",
        )
        .with_class("BackendConfigError"));
    }
    Ok(ServerBackend::connect(s, Some(default_vector_spec()))
        .map_err(server_err)?
        .with_gc_grace(Duration::from_secs_f64(s.gc_grace_seconds)))
}

/// The configured backend, verified: in server mode the cluster must be
/// reachable and every RagMonk index must exist with this build's schema.
pub fn open(home: &Home) -> Result<Backend, RagMonkError> {
    let cfg = load(home)?;
    open_with(&cfg)
}

pub fn open_with(cfg: &RagMonkConfig) -> Result<Backend, RagMonkError> {
    if cfg.storage.mode != "server" {
        return Ok(Backend::Local);
    }
    let server = connect(cfg)?;
    // One request verifies every existing index and lists missing ones.
    let missing = server.check_schema().map_err(server_err)?;
    if !missing.is_empty() {
        return Err(RagMonkError::config(format!(
            "server indexes are missing ({}); run `ragmonk server init` first",
            missing.join(", ")
        ))
        .with_class("BackendSchemaMismatchError"));
    }
    Ok(Backend::Server(Arc::new(server)))
}

/// The configured backend, creating missing server indexes (idempotent;
/// existing indexes are verified, never changed). Used by writers.
pub fn open_for_write(home: &Home) -> Result<Backend, RagMonkError> {
    let cfg = load(home)?;
    if cfg.storage.mode != "server" {
        return Ok(Backend::Local);
    }
    let server = connect(&cfg)?;
    server.init().map_err(server_err)?;
    Ok(Backend::Server(Arc::new(server)))
}

/// Errors for an operation that exists only in local mode.
pub fn local_only(what: &str) -> RagMonkError {
    RagMonkError::new(
        ErrorKind::LocalStorageModeRequired,
        format!(
            "{what} is not available with storage.mode = 'server'; \
             the server cluster holds the knowledge (use the cluster's own snapshot tools)"
        ),
    )
    .with_class("LocalStorageModeRequiredError")
}

/// The local control plane (local mode only).
pub fn local_control_plane(home: &Home) -> Result<ControlPlane, RagMonkError> {
    let cfg = load(home)?;
    ControlPlane::open(&StorageLayout::new(home), cfg.runtime.sqlite_cache_size_mb).map_err(db)
}

/// A catalog entry as the shared [`SourceRecord`] shape.
pub fn record_of(e: &CatalogEntry) -> SourceRecord {
    SourceRecord {
        id: e.source_id.clone(),
        path: e.path.clone(),
        source_type: if e.source_type == "network" {
            SourceType::Network
        } else {
            SourceType::Local
        },
        enabled: e.enabled,
        include_patterns: e.include_patterns.clone(),
        exclude_patterns: e.exclude_patterns.clone(),
        created_at: e.created_at.clone(),
        updated_at: e.updated_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: &str, url: &str) -> RagMonkConfig {
        let mut c = RagMonkConfig::default();
        c.storage.mode = mode.into();
        c.storage.server.url = url.into();
        c
    }

    #[test]
    fn local_mode_resolves_without_any_server() {
        assert!(matches!(open_with(&cfg("local", "")), Ok(Backend::Local)));
    }

    #[test]
    fn server_mode_without_url_is_a_config_error() {
        let e = open_with(&cfg("server", "")).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::Config);
        assert_eq!(e.class(), Some("BackendConfigError"));
    }

    #[test]
    fn unreachable_server_is_a_typed_unavailable_error() {
        // Port 9 (discard) on localhost: refused immediately.
        let mut c = cfg("server", "http://127.0.0.1:9");
        c.storage.server.request_timeout_seconds = 2.0;
        let e = open_with(&c).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::SourceUnavailable);
        assert_eq!(e.class(), Some("BackendUnavailableError"));
        assert_eq!(e.exit_code(), 4);
    }
}
