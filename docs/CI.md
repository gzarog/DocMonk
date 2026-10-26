# CI

Three workflows, all in `.github/workflows/`:

| Workflow | File | Triggers |
| --- | --- | --- |
| `CI` | `rust.yml` | every pull request; pushes to `main` |
| `Nightly` | `rust-nightly.yml` | daily 03:00 UTC; manual dispatch |
| `Rust release` | `rust-release.yml` | unchanged: green push CI on `main` (`workflow_run`), tags, manual |

The workflow name `CI` and its push triggers are what `rust-release.yml`
listens to; do not rename them. A failed `CI` push run never releases.

## What runs when

| Job | Application PR | Platform-specific PR | Installer PR | Docker PR | Docs-only PR | Push (main / release branch) |
| --- | --- | --- | --- | --- | --- | --- |
| Detect changes | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ (selects everything) |
| Repository audits | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Rust fmt/clippy/test (ubuntu-latest) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ + `cargo build --release` |
| Rust fmt/clippy/test (windows-latest, macos-latest) | – | ✓ incl. `cargo build --release` | ✓ incl. `cargo build --release` | – | – | ✓ incl. `cargo build --release` |
| Server integration (OpenSearch + Elasticsearch) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Install script (ubuntu, macos, windows) | – | – | ✓ | – | – | ✓ |
| Container image (linux/amd64) | – | – | – | ✓ | – | ✓ |
| CI gate | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| 150-source status benchmark | – | – | – | – | – | – (nightly) |
| Retrieval eval (repository, pinned models) | – | – | – | – | – | – (nightly) |
| Dependency advisories (`cargo deny check advisories`) | – | – | – | – | – | – (nightly) |
| Coverage (`cargo llvm-cov`, per crate in the job summary) | – | – | – | – | – | – (nightly) |
| ADR open items (`cargo xtask adr-audit --markdown`, job summary) | – | – | – | – | – | – (nightly) |
| Mutation testing (`cargo mutants`: docmonk-retrieval, docmonk-storage, docmonk-backends) | – | – | – | – | – | – (nightly) |

The Linux Rust job always requires the pinned OCR, embedding and reranker
models (`DOCMONK_REQUIRE_OCR=1`, `DOCMONK_REQUIRE_MODELS=1`).
`.github/actions/fetch-models` checks every model file's sha256 after a
cache restore, so a cache hit alone is never trusted.

### Path classification

`scripts/ci_changes.sh` reads the PR's changed paths (the merge commit
against its first parent) and decides:

- **installer**: `install.sh`, `install.ps1`, `crates/docmonk-update/**`, `xtask/**`
- **docker**: `Dockerfile`, `.dockerignore`, `docker/**`, `scripts/fetch_models.sh`
- **cross_platform** (Windows/macOS Rust jobs): everything selected for
  **installer**, plus the Rust sources with platform-conditional code
  (`cfg(windows)`, `cfg(unix)`, `target_os`, ...), listed in the
  classifier's `platform_source`, **and** path-sensitive endpoint/retrieval
  components and fixtures even when they contain no platform-specific `cfg`
  guards. These use filesystem paths whose canonical representation differs
  on Windows, macOS, and Linux
- **all three**: `Cargo.toml`, `Cargo.lock`, `deny.toml`, `rust-toolchain.toml`, any crate
  manifest, `.github/workflows/**`, `.github/actions/**`, the classifier itself

`scripts/ci_changes.sh --self-test` checks representative paths and runs
first in the job. It also greps every tracked Rust file for
platform-conditional code and fails when one is missing from
`platform_source`, so a new platform-specific file cannot slip past the
Windows/macOS jobs. Any error (diff, classifier, self-test) fails
`Detect changes`, and the CI gate fails with it. A broken filter cannot
skip a job and still go green.

Endpoint and exact-occurrence retrieval changes (including source/query paths
and `fixtures/endpoints/**`) select Windows and macOS Rust checks on pull
requests. This catches cross-platform temporary-directory alias and
canonicalization mistakes before merging; classifier self-tests cover the paths.

## Dependency policy (cargo-deny)

`Repository audits` runs `cargo deny check` against `deny.toml` on every
PR and push: any RustSec vulnerability fails, licences must be on the
MIT-compatible allowlist, crates may only come from crates.io, and
duplicate versions are reported as warnings. Ignores are allowed only for
advisories that cannot affect DocMonk, each with a reason and a review
date. The nightly `Dependency advisories` job re-runs the advisory check
on `main`; on failure it opens one issue labelled `security-advisory`, or
comments on the open one. Locally: `cargo install cargo-deny --locked`,
then `cargo deny check`.

## Mutation testing

The nightly `Mutation testing` legs run `cargo mutants` on
docmonk-retrieval, docmonk-storage and docmonk-backends (in-process
engine, no live cluster). Each writes its score (caught / (caught +
missed)) to the job summary and keeps `mutants.out` as an artifact;
`missed.txt` lists the mutants the tests did not catch. Triage a survivor
into a test or, when the mutant cannot change behaviour, leave it. The
job reports and never gates. Locally: `cargo install cargo-mutants
--locked`, then `cargo mutants -p <crate> --file <path>` for one file.

## ADR open items

`Repository audits` also prints `cargo xtask adr-audit`. It lists every
ADR whose `Status:` paragraph says in progress, proposed or not
implemented, and every line that names a measurement not taken yet
("not measured yet", "still to be measured", "stays open", ...). It never
fails a build; the nightly workflow writes the same list to its job
summary. Closing an item means measuring it and rewording the ADR, never
rewording alone.

## CI gate

`CI gate` always runs (`if: always()`) and reads every job's result:

- `Detect changes`, audits, Linux Rust and server integration must succeed.
- On pushes, every job must succeed.
- On PRs, the Windows/macOS, installer and Docker jobs may be skipped
  only when `Detect changes` succeeded and did not select them. A selected job that fails, is cancelled or is missing fails the gate.

The gate has no permissions and sees no secrets.

## Concurrency

`ci-${PR number || run id}`, with `cancel-in-progress` only for
`pull_request`. A new push to a PR cancels its previous run. Pushes get a
unique group, so a `main` run is never cancelled.

## Nightly

`rust-nightly.yml` starts OpenSearch 2.15.0 and Elasticsearch 8.15.0 and
runs the ignored release-mode `docmonk-status --test live_server`
benchmark unchanged. Its log and duration are uploaded as the
`status-benchmark-log` artifact (14-day retention). It never publishes.

The `retrieval-eval` job runs `docmonk-service --test repo_eval` in
release mode with `DOCMONK_REPO_EVAL=1` and the pinned models, so every
arm (lexical, routed, hybrid, hybrid with reranking) is checked against
`fixtures/search_quality/repo-eval-gates.json`. The report is uploaded as
the `repo-eval-report` artifact. The smaller quality guards run in every
PR's Linux job: `p0_eval` (generated corpus), `link_precision` (link and
call-edge precision floors in `fixtures/linking/precision-gates.json`)
and `batched_reads` (batched graph reads must equal the one-row reads and
use at least 10x fewer statements).

## Required checks (repository settings)

Branch protection is set in GitHub settings, not in this repository.
Rollout order:

1. Merge this change while the old required checks are still set. The
   `Rust fmt/clippy/test (ubuntu-latest)` and `Server integration` names
   are unchanged.
2. Once `CI gate` has reported on a run, make it the required check and
   remove `Rust fmt/clippy/test (windows-latest)`,
   `Rust fmt/clippy/test (macos-latest)` and the `Install script (*)` /
   `Container image` names. PRs no longer always report those, and a
   required check that never reports blocks merging.

## Baseline (2026-10-08, before this change)

- PR run: 1038 s wall, https://github.com/gzarog/DocMonk/actions/runs/37847236804
  (Windows 1034 s, macOS 896 s, Docker 492 s, Linux 477 s, server 230 s).
- Main run: 1946 s wall, https://github.com/gzarog/DocMonk/actions/runs/37797728388;
  the automatic release that followed took 1374 s.

Wall time is measured from the run's start to its last job's end. The
targets are ≤ 600 s warm and ≤ 1200 s cold for PRs. After this change,
record P50/P90 wall time and runner minutes from at least five PR runs and
two main runs here. The targets are estimates until those runs exist.

## Rollback

Revert the change: `rust.yml` returns to the all-OS matrix with the
benchmark inline. Delete `rust-nightly.yml`, `.github/actions/fetch-models`
and `scripts/ci_changes.sh`, then restore the previous required check names.
