# Changelog

All notable changes to DocMonk. Releases before 1.0 were published
automatically from `main` (a patch bump per merged change, a minor bump
for `[release minor]`); this file summarises them by theme. Versions are
git tags; every release lists its archives in a `SHA256SUMS`.

## 1.0.0 (unreleased)

The first product generation. DocMonk 1.0 is fresh-install and
fresh-index only (ADR 0032): nothing reads or converts a home, database
or server index written by an earlier build.

**Upgrading from 0.3.25.** `docmonk update install` (or the install
one-liner) replaces the binary. A home written by 0.3.25 opens unchanged
and keeps serving its existing index (checked: `status` and `search`
work on a 0.3.25 home without any reset). The redaction rules below
change every source's parser and chunker identity, so the next index
pass (`docmonk index`) fully rebuilds each
source once. Until then the old index is served as it was, including
any credentials it stored in plain text. **Run `docmonk index` right
after upgrading.** Nothing converts older data (ADR 0032): a home that
1.0 cannot open stops with the documented reset-and-reindex error and is
left unchanged.

**Server mode (OpenSearch / Elasticsearch).** Whether server mode ships
as generally available or as a preview in 1.0 is still open (roadmap
decision D1). It depends on the live 8k/50k-file measurements (roadmap
R4), which have not been run yet.

### Security

- Patched the open RustSec advisories in the TLS stack (rustls 0.23.46,
  rustls-webpki 0.103.16, webpki-roots 1.0.11) and tokio
  (RUSTSEC-2026-0386). The zip advisories only affect
  `ZipArchive::extract`, which DocMonk does not call; they are documented
  in `deny.toml`.
- `cargo deny check` (advisories, licences, bans, sources) runs on every
  PR, and a nightly advisory run reports to a single issue.
- Installers and `docmonk update` fail closed once a release-signing key
  is configured. A missing `minisign` tool, a missing or invalid
  `SHA256SUMS.minisig`, or tampered checksums abort the install.
  `DOCMONK_INSECURE_SKIP_SIGNATURE=1` is the only opt-out. The release
  workflow refuses to publish an unsigned release when a key is
  configured.
- Credentials never enter the index (ADR 0042). More secret file names
  are never indexed (`.netrc`, `.git-credentials`, `.npmrc`, kubeconfig,
  keystores, `*.tfstate`, ...), `indexing.secret_patterns` extends the
  list, and secrets inside indexed files (private keys, cloud and VCS
  tokens, JWTs, password and API-key literals, connection-string
  passwords, URL credentials) are replaced by `[REDACTED:<kind>]` before
  storage and embedding (`indexing.redact_secrets`, on by default).
- `mcp.hidden_sources` hides sources from AI agents while the CLI and the
  Admin UI keep searching them. `mcp.query_log` records every MCP tool
  call, with redacted arguments, in `<home>/logs/mcp.jsonl`.

### Sources

- Git repositories can be registered by URL
  (`docmonk source add <url> [--branch B] [--subpath DIR]`, ADR 0043).
  DocMonk keeps a shallow checkout, fetches it before every full pass, and
  the daemon polls the remote every `indexing.git_poll_seconds` (default
  300). `status` and `source info` show the indexed commit. A deleted
  branch or an unreachable remote marks the source offline with the
  reason and keeps its last index. Credentials come only from git's own
  helpers. Local mode only.

### Code intelligence

- Messaging topology (ADR 0044). RabbitMQ.Client, MassTransit,
  Confluent.Kafka, kafkajs and Spring Kafka publish, consume, bind and
  declare sites are graph edges to their topic, queue, exchange or message
  type. `impact` on a message contract lists its publishers and consumers
  across sources (RabbitMQ bindings followed), and `explore` and MCP
  `docmonk_impact` carry the same sites. Graph derivation `graph-4`: the
  first index after upgrading recomputes graphs, not content.

### Project

- Code owners for held-out labels, quality floors, the remediation
  ledger and the audits, plus a PR template that asks for eval reports
  and human approval of labels and gates.
- Debug builds keep line tables only for the workspace and no debug info
  for dependencies, which shrinks a debug test build substantially.
- New docmonk-ops tests: backup/restore round trip, rebuild, uninstall,
  vector maintenance. Nightly per-crate coverage.
- The local ANN benchmark sweeps `ef_search` and records recall and
  latency at 20k and 100k vectors (`benchmarks/ann-sweep.json`).

## 0.3.17 – 0.3.25 (2026-10-13 – 2026-10-23)

- The product is named DocMonk. The Rust workspace replaces the earlier
  implementation entirely, with a fresh home and fresh indexes (ADR 0032).
- Relationship graphs are built after indexing, as independent
  generations: incremental resolution by symbol, selective updates,
  explicit cross-source dependencies, server-side graph copy-forward, and
  `indexing.relationships_enabled` / `indexing.pipeline_relationships`.
- One canonical `status` implementation with indexing observability,
  progress and live watch mode, plus index locking and deadlock safety.
- Server storage on OpenSearch and Elasticsearch with backend parity,
  pass-scoped batching and MVCC publication.
- `.eml` email files, with attachments as child documents.
- Retrieval: a bounded query planner, URL anchors and exact occurrences
  (endpoint references), evidence with hard filters and re-verified
  citations, Greek lexical normalization, implementation passages, an
  optional multilingual embedding profile (`multilingual-e5-small`), and
  a held-out multilingual evaluation.
- Audit remediation: dependency lifecycle, lookup errors, fencing,
  manifest persistence, relationship scaling, paged embedding passes and
  batched citation hydration.

## 0.3.0 – 0.3.16 (2026-09-17 – 2026-09-25)

- An exact, pinned, offline tokenizer: payload-aware chunk and table
  budgets, the tokenizer identity in the index version, a recoverable
  `rebuild --fresh`, and diagnostics proving that no embedding payload is
  truncated.
- The built-in local Admin UI and a background daemon with a dashboard.
- Indexing performance: a reproducible benchmark suite, complete scans,
  coalesced daemon scheduling, one verified file fingerprint, targeted
  local updates, bounded parallel code extraction with one transactional
  publisher, embeddings-only backfill, and measured SQLite and cross-link
  optimizations.

## 0.2.0 – 0.2.5 (2026-09-17)

- The application was renamed, as a clean break.
- Optional subscription AI providers (ChatGPT via Codex, GitHub Copilot)
  with a provider lifecycle, registry, runtime, CLI and doctor
  diagnostics.

## 0.1.1 – 0.1.21 (2026-09-11 – 2026-09-17)

- `uninstall` with optional data purge, Windows self-update, snippet
  output and safe source removal.
- The search-quality plan: a 72-query golden benchmark; native PDF
  normalization; token- and hierarchy-aware chunking; search and
  embedding text; table-aware extraction; OCR fallback for scanned PDFs;
  tiered lexical planning; weighted BM25; reciprocal rank fusion; context
  expansion; more file formats; an optional reranker; and version-aware
  incremental reuse.
- Automatic MCP client setup for agents.
