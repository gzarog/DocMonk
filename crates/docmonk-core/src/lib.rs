//! DocMonk core contracts shared by every other crate: errors,
//! models, paths, source IDs, server document IDs and security checks.

pub mod deadline;
pub mod errors;
pub mod ids;
pub mod lexnorm;
pub mod messaging;
pub mod models;
pub mod paths;
pub mod secrets;
pub mod security;
pub mod urls;
pub mod version;

pub use errors::{DocMonkError, ErrorKind};
