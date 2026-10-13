//! RagMonk operations shared by the CLI and the Admin UI: health checks
//! ([`doctor`]), backup and restore ([`backup`]), full rebuilds
//! ([`rebuild`]), vector maintenance ([`vectors`]) and data removal
//! ([`uninstall`]).

pub mod backup;
pub mod doctor;
pub mod rebuild;
pub mod uninstall;
pub mod vectors;

use docmonk_core::errors::{ErrorKind, RagMonkError};
use docmonk_core::paths::Home;
use docmonk_service::load;

pub(crate) fn generic(e: impl std::fmt::Display) -> RagMonkError {
    RagMonkError::new(ErrorKind::Generic, e.to_string())
}

pub(crate) fn dberr(e: impl std::fmt::Display) -> RagMonkError {
    RagMonkError::new(ErrorKind::Database, e.to_string())
}

/// Whole-home exclusive locks: `index.lock`, then every source lock in id
/// order (see `docmonk_service::indexing::home_exclusive`).
pub(crate) fn index_lock(
    home: &Home,
    operation: &str,
) -> Result<docmonk_service::indexing::HomeLocks, RagMonkError> {
    docmonk_service::indexing::home_exclusive(home, operation)
}

/// Fails with a typed `LocalStorageModeRequired` error in server mode.
pub(crate) fn require_local(home: &Home, what: &str) -> Result<(), RagMonkError> {
    if load(home)?.storage.mode == "server" {
        return Err(docmonk_service::backend::local_only(what));
    }
    Ok(())
}

pub(crate) fn now_iso() -> String {
    docmonk_indexing::progress::now_iso()
}
