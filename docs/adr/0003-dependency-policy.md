# ADR 0003 — Dependency policy

Status: accepted

* Shared versions live in `[workspace.dependencies]`; crates use `.workspace = true`.
* Preferred stack: tokio, clap, serde, thiserror/anyhow, tracing,
  reqwest+rustls, rusqlite, notify, tree-sitter, tokenizers, axum, and
  docling.rs behind DocMonk's own trait.
* A new dependency needs: maintained upstream, permissive license compatible
  with MIT, builds on Windows/Linux/macOS without system packages where
  possible (e.g. `rusqlite` `bundled`), and a reason it beats a few lines of
  local code.
* `anyhow` only at process/application boundaries; libraries use `thiserror`.
* TLS uses rustls; no OpenSSL requirement.
* `Cargo.lock` is committed (the workspace ships binaries).
* Toolchain is pinned by `rust-toolchain.toml` (1.92.0); MSRV is the
  workspace `rust-version` (1.92, required by docling.rs 2.67).
* The production runtime has no dependency outside the Rust binary and its
  bundled model assets.
* `deny.toml` enforces this policy with `cargo deny check` in CI
  (docs/CI.md): no open RustSec vulnerability, an allowlist of permissive
  licences, crates.io as the only source. An advisory may be ignored only
  when it cannot affect DocMonk, with the reason and a review date next
  to the id; the nightly advisory run catches advisories published
  between PRs.
