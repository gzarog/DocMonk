//! The docmonk-cli integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-cli --test it -- <module>::`.

mod ai;
mod cli;
mod daemon;
mod eval;
mod git_source;
mod mcp;
mod ops;
mod queries;
mod status;
mod ui;
mod update;
mod workflow;
