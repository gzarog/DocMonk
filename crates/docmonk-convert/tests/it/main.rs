//! The docmonk-convert integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-convert --test it -- <module>::`.

mod common;
mod conversion;
mod email;
mod indexing;
mod normalize;
mod pdf_ocr;
