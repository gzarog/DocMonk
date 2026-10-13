//! RagMonk status: the single status model and its producers.
//!
//! * [`model`]: the canonical status contract every surface renders.
//! * [`local`]: the local SQLite collector.
//! * [`server`]: the batched OpenSearch/Elasticsearch collector.
//! * [`runtime`]: run liveness (local progress and locks, server
//!   heartbeats against writer leases).
//! * [`health`]: the one evaluator (source states, problems, verdict).
//! * [`errors`]: the globally ordered, redacted error feed.
//! * [`snapshot`]: collector-neutral facts and the one report builder.
//!
//! Nothing else in the workspace builds a status report: the CLI, MCP
//! server and Admin UI all consume [`model::StatusReport`].

pub mod errors;
pub mod health;
pub mod local;
pub mod model;
pub mod runtime;
pub mod server;
pub mod snapshot;

pub use model::{CollectError, StatusReport, SCHEMA_VERSION};
pub use snapshot::CollectOptions;
