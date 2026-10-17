# DocMonk architecture

DocMonk is a native Rust application. This Cargo workspace is the whole
product: one `docmonk` binary built from the crates below, plus the `xtask`
helper for packaging and repository audits.

| Crate | Responsibility |
|-------|----------------|
| `docmonk-cli` | `docmonk` binary: `version`, `config`, `server schema/init`, `daemon`, `init`, `source`, `index`, `status`, `docs`, `watch`, `search`, `symbol`, `callers`, `callees`, `references`, `impact`, `explore`, `link`, `doctor`, `health`, `backup`, `restore`, `rebuild`, `update`, `uninstall`, `vectors`, `ask`, `ai`, `ui` and `serve --mcp` |
| `docmonk-service` | application services shared by the CLI, Admin UI and MCP server: the single storage-mode resolver (`backend::open`), source catalog, bounded parallel indexing runs, server-mode staged indexing and publication (with run heartbeats), queries, the status facade, `ask`, daemon control |
| `docmonk-status` | the single status model, the local SQLite and batched OpenSearch/Elasticsearch collectors, run liveness, the health evaluator and the globally ordered error feed (ADR 0034) |
| `docmonk-ops` | operations: `doctor`, backup/restore, full rebuilds, vector maintenance, uninstall |
| `docmonk-mcp` | stdio MCP server (`docmonk serve --mcp`) exposing the read-only `docmonk_*` tools |
| `docmonk-ui` | local Admin UI (`docmonk ui`), an axum server with embedded templates |
| `docmonk-ai` | AI providers for `ask`: OpenAI, Anthropic, Ollama, OpenAI-compatible, Codex (stdio JSON-RPC) and Copilot (CLI) |
| `docmonk-backends` | OpenSearch/Elasticsearch index set with strict mappings: the authoritative server catalog, writer leases with fencing, bounded adaptive bulk writes, generation-numbered immutable records (unchanged records are never rewritten), atomic per-source publication with grace-period GC, and the `ServerReader` implementation of the knowledge read port |
| `docmonk-code` | tree-sitter code intelligence: `.scm` queries, entity extraction, resolution, framework rules, code graph, and the post-index graph stage (per-file edges, cross-file resolution, versioned graph generations) |
| `docmonk-documents` | normalized-document model, pinned tokenizer, token-budget splitting, row-aware tables, chunker |
| `docmonk-convert` | Rust-native document conversion (docling.rs, no ML/network), Docling-JSON normalizer, email and attachments, document processor |
| `docmonk-knowledge` | cross-domain linker (code entities <-> document chunks) and persistent manual links |
| `docmonk-ml` | pinned model assets, pure-Rust (Candle) embeddings, embedding cache, persistent HNSW index, semantic search, RRF and the cross-encoder reranker |
| `docmonk-retrieval` | lexical and hybrid search, exact-match pinning, reranking, symbol search, graph queries, explore and impact, context expansion and query classification |
| `docmonk-indexing` | scanner, ignore rules, incremental diff, retries, per-source run locks, bounded parallel executor, process-wide resource governor, bounded transactional build coordinator, per-source progress and heartbeats, process probes, fair multi-worker daemon, filesystem watcher, network polling, targeted passes |
| `docmonk-storage` | SQLite control plane (`state/control.db`), per-project knowledge stores (`projects/<id>/knowledge.db`) and the storage-neutral `KnowledgeRead` port every retrieval stage reads through |
| `docmonk-config` | `config.yaml` loading, layering (defaults < user < project < env < CLI), typed validation |
| `docmonk-core` | errors and exit codes, domain models, home layout, stable IDs, secret filter, path guard, version |
| `docmonk-update` | native self-update: strict release tags, `update.json` cache, SHA-256 + optional minisign verification, `versions/<ver>` + `current` layout with rollback (ADR 0029) |
| `docmonk-telemetry` | JSON-lines logging, URL/credential redaction |

Releases are built by `.github/workflows/rust-release.yml` with
`cargo xtask package`, and the container image comes from the root
`Dockerfile` (ADR 0030).

## Quality gates
```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo xtask branding-audit
cargo xtask clean-slate-audit
```

## Test fixtures
`fixtures/` holds the code and document corpora the tests index, and
`fixtures/expected/` the expected outputs that snapshot tests compare
against. Snapshot tests that support it regenerate their expectations with
`DOCMONK_BLESS=1`.

## Storage modes

`storage.mode` is resolved in one place (`docmonk_service::backend`). In
local mode the SQLite stores are authoritative; in server mode the
OpenSearch/Elasticsearch indexes are, and nothing local is read for the
catalog, builds or knowledge (ADR 0033). Retrieval is written once against
`KnowledgeRead`, so lexical, graph, context and semantic stages behave the
same on both backends (ranking scores are each engine's own).

Design decisions are recorded in [`docs/adr/`](adr).
