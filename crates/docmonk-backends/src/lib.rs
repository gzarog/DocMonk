//! RagMonk server backends (OpenSearch and Elasticsearch).
//!
//! * [`schema`]: the index set `{prefix}-{source-state,files,code,
//!   documents,chunks,relationships,runtime}` with strict mappings,
//!   vector/HNSW settings fixed at creation and a `_meta.schema_version`.
//! * [`graph`]: relationship graph generations, published after (and
//!   independently of) the base build they were derived from.
//! * [`status`]: batched, bounded status reads and fenced run heartbeats.
//! * [`backend::ServerBackend`]: idempotent writes with deterministic
//!   IDs, bounded adaptive bulk batching, and atomic per-source build
//!   publication (an unpublished build is never returned by any read).
//!
//! OpenSearch and Elasticsearch share one adapter; [`engine::Engine`]
//! isolates the few mapping/query differences (vector field types).

pub mod backend;
pub mod bulk;
pub mod engine;
pub mod error;
pub mod graph;
pub mod publish;
pub mod reader;
pub mod schema;
pub mod status;
pub mod transport;

pub use backend::ServerBackend;
pub use error::BackendError;
