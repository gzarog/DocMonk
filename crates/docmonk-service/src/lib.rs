//! DocMonk application services.
//!
//! The operations the CLI, the Admin UI and the MCP server share, as plain
//! functions returning data: source registration ([`sources`]), indexing
//! runs ([`indexing`]), queries ([`query`]), status ([`status`]), AI
//! answers ([`ask`]) and the background daemon's lifecycle ([`daemon`]).
//! Rendering stays with each front end.

pub mod ask;
pub mod backend;
#[doc(hidden)]
pub mod bench;
pub mod daemon;
pub mod dependencies;
pub mod evidence;
pub mod indexing;
pub mod query;
pub mod read;
pub mod relationships;
pub mod server_index;
pub mod sources;
pub mod status;

use docmonk_config::{load_config, DocMonkConfig, LoadOptions};
use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::Home;

/// The process's DocMonk home, with its directory layout created.
pub fn prepared_home() -> Result<Home, DocMonkError> {
    let home = Home::discover();
    home.ensure_layout().map_err(|e| {
        DocMonkError::new(
            ErrorKind::Generic,
            format!("cannot create {}: {e}", home.root().display()),
        )
    })?;
    Ok(home)
}

/// The layered configuration for `home`.
pub fn load(home: &Home) -> Result<DocMonkConfig, DocMonkError> {
    load_config(&LoadOptions {
        home: Some(home.root().to_path_buf()),
        ..LoadOptions::default()
    })
}

/// The embedding tokenizer this build uses and the configured chunk ceiling.
pub fn tokenizer_info(cfg: &DocMonkConfig) -> serde_json::Value {
    use docmonk_documents::tokenizer as t;
    serde_json::json!({
        "model_id": t::EMBEDDING_MODEL_ID,
        "revision": t::TOKENIZER_REVISION,
        "fingerprint": t::tokenizer_fingerprint(),
        "max_sequence_tokens": t::MAX_SEQUENCE_TOKENS,
        "chunk_ceiling": cfg.documents.chunking.resolved_max_tokens(),
    })
}

pub(crate) fn generic(e: impl std::fmt::Display) -> DocMonkError {
    DocMonkError::new(ErrorKind::Generic, e.to_string())
}

pub(crate) fn db(e: impl std::fmt::Display) -> DocMonkError {
    DocMonkError::new(ErrorKind::Database, e.to_string())
}
