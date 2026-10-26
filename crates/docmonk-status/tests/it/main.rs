//! The docmonk-status integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-status --test it -- <module>::`.

mod common;
mod contract;
mod live_server;
mod local;
mod server_budget;
mod verdicts;
