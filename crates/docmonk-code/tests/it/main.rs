//! The docmonk-code integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-code --test it -- <module>::`.

mod common;
mod golden;
mod indexing;
mod passages;
