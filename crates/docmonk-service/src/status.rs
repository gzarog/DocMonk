//! The status facade: opens the configured backend and asks
//! [`docmonk_status`] for the one canonical [`StatusReport`]. No status
//! logic lives here.

use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::Home;
use docmonk_status::local::LocalInputs;
use docmonk_status::server::{ServerInputs, ServerSession};
pub use docmonk_status::{CollectError, CollectOptions, StatusReport};

use crate::backend::Backend;
use crate::load;
use crate::sources::control_plane;

/// Collection options from the configuration (stall threshold).
pub fn options(cfg: &docmonk_config::DocMonkConfig) -> CollectOptions {
    CollectOptions {
        stall_threshold_seconds: cfg.indexing.status_stall_threshold_seconds,
        ..CollectOptions::default()
    }
}

/// A typed collection failure as the CLI/MCP/UI error.
pub fn collect_error(e: CollectError) -> DocMonkError {
    let (kind, class) = match &e {
        CollectError::Unavailable(_) => (ErrorKind::SourceUnavailable, "BackendUnavailableError"),
        CollectError::SchemaMismatch(_) => (ErrorKind::Config, "BackendSchemaMismatchError"),
        CollectError::Database(_) => (ErrorKind::Database, "StatusDatabaseError"),
        CollectError::Config(_) => (ErrorKind::Config, "BackendConfigError"),
    };
    DocMonkError::new(kind, e.to_string()).with_class(class)
}

/// One open backend for repeated snapshots (`docmonk status --watch`):
/// the server connection and schema check happen once, and published
/// aggregates are reused while no build is published.
pub struct StatusSession {
    home: Home,
    cfg: docmonk_config::DocMonkConfig,
    backend: Backend,
    server: ServerSession,
}

impl StatusSession {
    /// Opens the configured backend (server: reachable and schema-verified;
    /// an unreachable cluster is a typed error, never a local fallback).
    pub fn open(home: &Home) -> Result<Self, DocMonkError> {
        let cfg = load(home)?;
        let backend = crate::backend::open_with(&cfg)?;
        Ok(Self {
            home: home.clone(),
            cfg,
            backend,
            server: ServerSession::default(),
        })
    }

    pub fn config(&self) -> &docmonk_config::DocMonkConfig {
        &self.cfg
    }

    /// One snapshot.
    pub fn collect(&mut self, opts: &CollectOptions) -> Result<StatusReport, DocMonkError> {
        let mut report = self.collect_report(opts)?;
        docmonk_status::snapshot::attach_dependency_retries(&mut report, &self.home);
        Ok(report)
    }

    fn collect_report(&mut self, opts: &CollectOptions) -> Result<StatusReport, DocMonkError> {
        match &self.backend {
            Backend::Local => {
                let cp = control_plane(&self.home)?;
                docmonk_status::local::collect(
                    &LocalInputs {
                        home: &self.home,
                        control: &cp,
                        cache_size_mb: self.cfg.runtime.sqlite_cache_size_mb,
                    },
                    opts,
                )
                .map_err(collect_error)
            }
            Backend::Server(server) => docmonk_status::server::collect(
                &ServerInputs {
                    backend: server,
                    endpoint: &self.cfg.storage.server.url,
                },
                opts,
                Some(&mut self.server),
            )
            .map_err(collect_error),
        }
    }
}

/// One snapshot with the configured options.
pub fn report(home: &Home) -> Result<StatusReport, DocMonkError> {
    report_with(home, |_| {})
}

/// One snapshot; `tune` adjusts the configured options.
pub fn report_with(
    home: &Home,
    tune: impl FnOnce(&mut CollectOptions),
) -> Result<StatusReport, DocMonkError> {
    let mut session = StatusSession::open(home)?;
    let mut opts = options(session.config());
    tune(&mut opts);
    session.collect(&opts)
}
