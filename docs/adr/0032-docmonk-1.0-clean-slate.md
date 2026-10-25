<!-- clean-slate-audit: allow-start -->
# ADR 0032 — DocMonk 1.0: fresh install, fresh index, no migrations

Status: accepted (RM10-00)

## Context
The Rust implementation on `main` is complete. It still carries the
machinery of the rewrite that produced it: a V1 source import, a migration
preflight, `migrate-to-rust-v2`, numbered SQLite migration arrays,
`schema_migrations` / `migration_log`, `V2Layout` paths under `<home>/v2/`,
`docmonk-v2-*` server indexes with legacy-index discovery and deletion, a
Python differential harness under `rust/compat/`, and Python packaging and
branding scripts. DocMonk 1.0 is the first product generation of this Rust
implementation, not an upgrade of anything that came before it.
## Decision
* **Fresh install, fresh index only.** A DocMonk 1.0 user runs
  `docmonk init`, `docmonk source add <path>`, `docmonk index`. The only
  input that survives from before is the user's original source files;
  DocMonk rebuilds all knowledge from them.
* **Unsupported inputs.** Earlier DocMonk homes, config files, source
  registries, SQLite databases, embeddings, chunks, IDs, generation state
  and OpenSearch/Elasticsearch indexes are not inputs. No code detects,
  opens, imports, transforms, cleans up or deletes them, and no installer
  or command mentions them.
* **One schema per database, created directly.** A missing database is
  created at the complete current schema in one step. There is no
  migration type, runner, numbered history, or migration/audit table. An
  existing database whose schema is not the current one is rejected with a
  deterministic error telling the user to reset the home (or remove the
  project directory) and reindex; it is never altered.
* **Fingerprints are not migrations.** Parser, chunker, converter,
  embedding model (id, checksum, dimension), embedding-text and tokenizer
  identities stay. They decide cache invalidation and when a source needs a
  full rebuild; they never sequence schema changes.
* **Canonical local layout** under `DOCMONK_HOME` (default `~/.docmonk`):
  `config.yaml`, `state/control.db`, `projects/<id>/…`, `models/`,
  `cache/`, `logs/`, `locks/`. No versioned or legacy directory exists.
* **Canonical server indexes** under the configured prefix (default
  `docmonk`): `{prefix}-source-state`, `{prefix}-files`, `{prefix}-code`,
  `{prefix}-documents`, `{prefix}-chunks`, `{prefix}-relationships`.
  Creation is idempotent for the current mappings only. Incompatible
  mappings under the configured prefix fail with an instruction to use a
  fresh prefix/cluster or reset the current DocMonk indexes. No other
  prefix is ever probed.
* **Kept from the rewrite architecture:** atomic build publication with
  invisible incomplete builds and failed-build cleanup, deterministic IDs,
  bulk batches bounded by action count and bytes, vector mappings defined
  at creation, bounded build-mode tuning, metadata-first incremental
  scanning with conditional hashing and move/rename/delete detection,
  offline-scan delete safety, bounded channels and CPU parallelism,
  per-source failure isolation and retry, bounded lock acquisition with
  owner diagnostics, crash recovery, watch debounce and reconciliation,
  fair scheduling, native document conversion/OCR/EML attachments and
  native embeddings/reranking with pinned assets.
* **Python** is only an indexed source language: the Python tree-sitter
  grammar, its query/framework wiring, and `.py` input fixtures. Runtime,
  build, tests, CI, packaging and release never execute Python.
* **Releases** use one strict semver tag convention (`v1.0.0`) shared by
  GitHub releases, `install.sh`, `install.ps1`, `docmonk update` and the
  release workflow.

## Enforcement
`cargo xtask clean-slate-audit` scans every tracked file for migration,
legacy-format, V1/V2 naming, rewrite/differential-harness and Python-tooling
references. Accepted occurrences are limited to:

1. a narrow path allowlist for Python source-language support and fixtures
   (in `xtask/src/audit.rs`);
2. neutral tokens that only look forbidden (pinned model ids such as
   `all-MiniLM-L6-v2`, semver tags, the Python grammar crate name);
3. `clean-slate-audit: allow-start/allow-end` blocks in policy text that
   must name what it forbids (this ADR);
4. the transition baseline `audit/clean-slate-baseline.tsv`.

The baseline is shrink-only: a file above its count fails, and a file
below its count also fails until the baseline is regenerated, so each
cleanup phase (RM10-01 … RM10-09) locks in what it removed. The release
gate runs `--require-empty-baseline`. The audit is never weakened to get
CI green; a legitimate Python-language occurrence gets the narrowest
possible allowlist entry instead.

The per-area removal inventory and Python classification lived in
`docs/clean-slate-inventory.md` while the cleanup phases ran; it was
removed for the 1.0 release once the baseline was empty (git history keeps
it).

## Consequences
Users of earlier builds reinstall and reindex. Rewrite-era ADRs are
removed or replaced as their phases land (RM10-09 removes the rest).
<!-- clean-slate-audit: allow-end -->
