//! The docmonk-retrieval integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-retrieval --test it -- <module>::`.

mod attachments;
mod context;
mod endpoints;
mod explore;
mod golden;
mod graph;
mod passages;
mod quality;
