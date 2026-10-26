//! The docmonk-indexing integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-indexing --test it -- <module>::`.

mod common;
mod daemon;
mod daemon_golden;
mod golden;
mod scenarios;
mod targeted_golden;
