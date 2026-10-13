//! RagMonk core contracts shared by every other crate: errors,
//! models, paths, source IDs, server document IDs and security checks.

pub mod errors;
pub mod ids;
pub mod models;
pub mod paths;
pub mod security;
pub mod version;

pub use errors::{ErrorKind, RagMonkError};
