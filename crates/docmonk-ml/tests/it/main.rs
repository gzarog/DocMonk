//! The docmonk-ml integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-ml --test it -- <module>::`.

mod common;
mod finalize;
mod multilingual;
mod parity;
mod quality;
mod reranker;
mod semantic;
