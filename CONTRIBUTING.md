# Contributing to DocMonk

DocMonk is a Rust application. The Cargo workspace is the repository root;
each crate's role is listed in [`docs/architecture.md`](docs/architecture.md)
and design decisions are recorded in [`docs/adr/`](docs/adr).

## Development setup

`rust-toolchain.toml` pins the toolchain; `rustup` installs it on first
use. No other runtime is needed.

```bash
cargo build --workspace          # the `docmonk` binary: target/debug/docmonk
```

Debug builds keep line tables for the workspace crates and no debug info
for dependencies (`[profile.dev]` in `Cargo.toml`), so backtraces still
name files and lines. A full debug test build (`cargo test --workspace
--no-run`) needs about 9 GB of `target/`. For a debugger session, build
with `CARGO_PROFILE_DEV_DEBUG=full`.

## Running the checks

CI (`.github/workflows/rust.yml`) runs these on Linux, macOS and Windows.
Run them locally before you push:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo xtask branding-audit       # the former product name must not come back
cargo xtask clean-slate-audit    # forbidden-reference check (ADR 0032)
```

Some test groups only run fully when their external inputs are present.
Without them they skip, unless the matching `DOCMONK_REQUIRE_*` variable
makes skipping a failure, as CI does:

| Inputs | Environment |
|---|---|
| Pinned embedding and reranker models (`scripts/fetch_models.sh DIR`) | `DOCMONK_MODELS_DIR=DIR`, `DOCMONK_REQUIRE_MODELS=1` |
| Pinned OCR models (the `ocrs/` folder of the same script's output) | `DOCMONK_OCR_MODELS_DIR=DIR/ocrs`, `DOCMONK_REQUIRE_OCR=1` |
| A live OpenSearch / Elasticsearch (`docker compose -f docker/docker-compose.opensearch.yml up -d`) | `DOCMONK_TEST_OPENSEARCH_URL` / `DOCMONK_TEST_ELASTICSEARCH_URL`, `DOCMONK_REQUIRE_LIVE_SERVER=1` |

The live-server tests create and delete indices under a unique prefix.
Never point them at a real deployment.

## Fixtures and expected results

`fixtures/` holds the source inputs tests index (code in several
languages, documents, emails, OCR images). `fixtures/expected/` holds the
expected results tests compare against, and
`fixtures/search_quality/gates.json` the retrieval quality floors.
`fixtures/eval/` holds the held-out retrieval evaluation's corpus and
labels (see [`docs/evaluation.md`](docs/evaluation.md)); its labels are
pending human review and are never rewritten by a test run.
`benchmarks/` holds benchmark records.

Expected results are DocMonk's own specification. A deliberate behaviour
change regenerates the affected file with `DOCMONK_BLESS=1 cargo test
-p <crate> --test <test>` (where the test supports it) or edits it by
hand, in the same PR, and the PR description says why.

## Labels, floors and audits

Held-out labels (`fixtures/eval/`), quality floors (`*gates*.json`), the
remediation ledger (`audit/remediation/ledger.json`), the clean-slate
baseline and the audit rules (`xtask/src/audit.rs`) decide whether the
code is good enough; `.github/CODEOWNERS` requires a code-owner approval
for every change to them. Floors only move up. A label written by a model
is approved by a person, never by the model that wrote it or the change
it judges. The PR template's checklist asks for this explicitly.

## Test isolation

Every test that runs the CLI sets `DOCMONK_HOME` to a temporary directory
and removes inherited `DOCMONK_*` variables. Tests must never read or
write the real `~/.docmonk`.

## Packaging and the install scripts

`cargo xtask package` builds a release archive for one target and records
it in the output directory's `SHA256SUMS`; `cargo xtask release-manifest`
writes the final `SHA256SUMS` over every archive in a directory.

`install.sh` and `install.ps1` download `docmonk-<ver>-<target>` for this
platform, verify it against the release's `SHA256SUMS`, unpack it under
`$DOCMONK_INSTALL_DIR/versions` and link `docmonk` onto `DOCMONK_BIN_DIR`.
Run them locally exactly as CI does, against archives packaged from your
own build:

```bash
cargo build -p docmonk-cli
cargo xtask package --binary target/debug/docmonk \
    --version 0.9.0 --target x86_64-unknown-linux-gnu --out /tmp/dist
DOCMONK_VERSION=0.9.0 DOCMONK_DOWNLOAD_BASE=/tmp/dist \
    DOCMONK_INSTALL_DIR=/tmp/docmonk-install DOCMONK_BIN_DIR=/tmp/docmonk-bin sh ./install.sh
/tmp/docmonk-bin/docmonk version
```

```powershell
$env:DOCMONK_VERSION = "0.9.0"; $env:DOCMONK_DOWNLOAD_BASE = "$env:TEMP\dist"; ./install.ps1
```

CI runs this (install, re-install, `docmonk update rollback`, a tampered
archive) on all three OSes.

## Releases

Releases are published by `.github/workflows/rust-release.yml` (ADR 0030).
Tags are strict semantic versions, `vMAJOR.MINOR.PATCH`.

## Commit style

Keep commits focused and explain why a change was made as well as what it
does. User-visible changes are described in the release notes.
