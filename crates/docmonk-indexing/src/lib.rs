//! RagMonk indexing core.
//!
//! * [`classify`], [`ignore`], [`scan`], [`fingerprint`]: file discovery,
//!   ported from `ragmonk.sources.{detector,ignore,scanner,fingerprint}`.
//! * [`diff`]: metadata-first change classification with conditional
//!   hashing, rename detection and incomplete/offline-scan safety
//!   (with the coordinator's reconciliation).
//! * [`retry`]: the exponential backoff policy.
//! * [`lock`]: the bounded cross-process run lock with owner metadata
//!   without `unsafe`.
//! * [`governor`]: process-wide CPU/OCR/embedding/I-O/byte permits shared
//!   by every concurrently running source.
//! * [`coordinator`]: per-source builds with bounded parallel prepare,
//!   a single writer, per-source failure isolation and atomic publication.

pub mod classify;
pub mod coordinator;
pub mod daemon;
pub mod diff;
pub mod executor;
pub mod fingerprint;
pub mod governor;
pub mod ignore;
pub mod lock;
pub mod progress;
pub mod retry;
pub mod runtime;
pub mod scan;
pub mod watcher;
