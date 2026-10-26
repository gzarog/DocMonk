//! The docmonk-ui integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-ui --test it -- <module>::`.

mod mcp_http;
mod security;
