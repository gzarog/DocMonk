//! The docmonk-knowledge integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-knowledge --test it -- <module>::`.

mod common;
mod golden;
mod scaling;
