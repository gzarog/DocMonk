# ADR 0007 — OpenSearch/Elasticsearch schema

Status: accepted

## Decision
* **Names.** Indexes are named `{storage.server.index_prefix}-{kind}`
  (default prefix `docmonk`, e.g. `docmonk-chunks`).
* **Seven specialized indexes**: `-source-state`, `-files`, `-code`,
  `-documents` (top-level documents and EML attachment children with
  `parent_document_id`/`attachment_*` provenance), `-chunks` (retrieval
  text and vectors), `-relationships` (code edges and cross-domain
  links, distinguished by `record_kind`) and `-runtime` (one heartbeat
  document per source written by the host holding its writer lease,
  fenced by the lease token; never build-scoped, never garbage-collected
  by publication; ADR 0034). Files carry `last_error_at` (when the
  file's latest error happened) and source-state carries `published_at`. Mappings are `dynamic: strict`;
  bulky text (`text`, `embedding_text`, `evidence`, `table_rows`) is stored
  but not indexed.
* **Schema identity** is stored in each index's `_meta` (`schema`,
  `schema_version`, vector spec). `docmonk server init` creates missing
  indexes, waits for primaries, and verifies existing ones; any mismatch is
  refused — indexes are never mutated in place. A schema change means a
  fresh index set (or prefix) and a full rebuild. The current identity is
  `schema_version` 3; `docmonk status` and every other command verify all
  seven identities with one `_mapping` request.
* **Vectors** are defined at creation (`knn_vector`+HNSW/lucene/cosine on
  OpenSearch, `dense_vector`+HNSW/cosine on Elasticsearch), 384 dims for the
  pinned embedding model, m=16, ef_construction=128.
* **IDs.** Records use `docmonk_core::ids::record` IDs; the document `_id`
  is `<build_id>:<record id>`, deterministic and idempotent, and builds
  never overwrite each other.
* **Atomic publication.** A source-state document holds `active_build_id`
  and `pending_build_id`. Publishing refreshes the build-scoped indexes and
  switches the active build with an optimistic-concurrency write
  (`if_seq_no`/`if_primary_term`; a concurrent publisher gets a conflict),
  then deletes every other build of the source. Every read filters on
  `(source_id, active_build_id)` pairs; with no published build nothing
  matches. Starting a new build deletes the leftovers of an abandoned one;
  aborting keeps the previous build visible.
* **Bulk writes** never exceed `storage.server.bulk.max_actions` or
  `max_bytes` per request, shrink on 413/429, grow back on success, retry
  only 429/503 items with backoff up to `max_retries`, and treat a delete of
  a missing document as success. Request telemetry counts requests per
  endpoint, bytes and bulk actions; the live tests assert every record write
  goes through `_bulk`.
* **Build mode** (`enter_build_mode`) sets `refresh_interval: -1` and zero
  replicas on build-scoped indexes for a large rebuild and restores the
  saved serving settings explicitly or on drop.

## Evidence
`crates/docmonk-backends/tests/live.rs` runs the full lifecycle against real
OpenSearch and Elasticsearch (CI job "Server integration"), including
injected crashes/aborts and visibility checks.
