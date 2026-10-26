//! The docmonk-backends integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-backends --test it -- <module>::`.

mod ann_recall;
mod graph_mock;
mod live;
mod publication;
