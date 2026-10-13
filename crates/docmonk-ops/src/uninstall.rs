//! Data removal for `uninstall`.

use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::Home;
use docmonk_indexing::daemon::pid;

use crate::backup::STOP_TIMEOUT;
use crate::generic;

/// Deletes the whole DocMonk home, stopping a running daemon first.
/// Returns whether anything was deleted.
pub fn purge_data(home: &Home) -> Result<bool, DocMonkError> {
    if !home.root().exists() {
        return Ok(false);
    }
    pid::stop_and_wait(home, STOP_TIMEOUT, "delete this data").map_err(generic)?;
    std::fs::remove_dir_all(home.root()).map_err(generic)?;
    Ok(true)
}
