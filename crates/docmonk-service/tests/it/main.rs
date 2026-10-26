//! The docmonk-service integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-service --test it -- <module>::`.

mod batched_reads;
mod dependencies;
mod embedding_model_switch;
mod endpoints;
mod evidence;
mod exact_parity;
mod heldout_eval;
mod link_precision;
mod multilingual_lexical;
mod p0_eval;
mod ranking_parity;
mod relationships;
mod repo_eval;
mod selective_updates;
mod server_hydration;
mod server_live;
