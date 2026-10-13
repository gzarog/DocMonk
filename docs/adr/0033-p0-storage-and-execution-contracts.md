# ADR 0033 — P0: storage-neutral ports, authoritative server mode, bounded parallel indexing and grounded retrieval

Status: accepted (P0-00, on the P0 completion release branch)

## Context
At `main@a92f9a1` DocMonk ran end to end only in local mode. In server mode:

* `docmonk-service` indexing still opened the local `StorageLayout`,
  `ControlPlane` and `run_source`, and every query path
  (`query::open_sources`) opened a SQLite `ProjectStore`. A configured
  OpenSearch/Elasticsearch cluster therefore never received the knowledge
  that queries read.
* `status::collect` reported `"server-mode status counts are not
  available yet"` instead of counts.
* `ServerBackend` had schema creation, staged build writes, CAS publish,
  raw/visible counts and a basic chunk/code `multi_match`, but no catalog,
  no read path the retrieval stack could use, and `active_builds` read at
  most 10000 sources (`size: 10000`, no paging).
* Sources were indexed one after another under one global `index.lock`;
  extraction workers defaulted to 1 and the daemon ran a single pass at a
  time.
* Retrieval classified only a few query shapes; there were no typed hard
  filters, no decomposition, no diversity control, and no citation
  verification or abstention.

These observations are recorded as baseline evidence in
`benchmarks/p0-baseline-summary.json` (with the measured local-mode
numbers) and in `fixtures/p0/server-mode-baseline.json`.

## Decision

### Source of truth
| Mode | Authoritative for catalog, build lifecycle and knowledge | Allowed local files |
|------|----------------------------------------------------------|---------------------|
| `local` | `state/control.db` and `projects/<id>/knowledge.db` | everything under the home |
| `server` | `{prefix}-source-state` (catalog, build lifecycle, leases) and `{prefix}-{files,code,documents,chunks,relationships}` (knowledge, vectors) | `config.yaml`, `logs/`, `locks/`, `models/`, and **disposable** caches under `cache/` (the extraction staging area). Nothing local is ever read to answer a query, list sources or report status. |

A server-mode operation that cannot reach the cluster fails with a typed
error (`BackendUnavailableError`, exit code 4); it never falls back to
SQLite. Deleting the staging cache only costs reconversion: the next pass
detects that the cache does not match the server's active build and
rebuilds it from the original files.

### Ports
Services depend on typed ports, not on `ProjectStore`:

* `KnowledgeRead` (`docmonk-storage::read`): the object-safe read port the
  whole retrieval stack (lexical tiers, graph traversal, context expansion,
  semantic candidates, links) uses. Implemented by `ProjectStore` and by
  the server `ServerReader`; every call is scoped to one
  `(source_id, build_id)` pair.
* Source catalog and build lifecycle: `ControlPlane` (local) or the server
  catalog (`ServerBackend::{register_source, list_catalog, ...}`), behind
  the service-level `Backend` factory.
* Build writing: the local transactional coordinator, or the server
  publisher that copies unchanged records forward server-side and bulk-
  writes only changed files.
* Status: `status::collect` reads the selected backend.

`docmonk_service::backend::open` is the only place that turns
`storage.mode` into a backend; CLI, Admin UI, MCP and daemon all go through
it.

### Build semantics (both modes)
* Per-source immutable pending build, explicit finalize, visible only
  after an atomic compare-and-swap publish. Any error keeps the previous
  active build readable.
* Server writers hold a source-scoped lease with a monotonically
  increasing fencing token; begin/publish/abort are refused for a
  superseded or expired lease.
* A replaced server build is retired, not deleted: it stays readable for
  `storage.server.gc_grace_seconds` so a query pinned to it finishes, and
  is garbage-collected by a later publish.
* Each query reads one snapshot: the active build map is read once per
  request.
* Stable IDs (`docmonk_core::ids::record`), deterministic ordering and
  bounded paging (`search_after`) everywhere.

### Execution
* Up to `indexing.max_parallel_sources` independent sources run at once,
  each under its own `locks/index-<source>.lock` (and, in server mode, its
  writer lease). One source failing never blocks another.
* A process-wide resource governor bounds CPU, OCR, embedding, I/O and
  in-flight bytes across all running sources.
* The daemon schedules sources fairly (per-source FIFO with aging) and
  keeps at most one queued follow-up per source.

### Retrieval
* Typed `QueryIntent` routing selects strategies; hard filters
  (`source_ids`, path prefixes, file kinds, document ids) are applied
  before every retrieval and traversal stage and cannot be widened by any
  later stage.
* Deterministic bounded decomposition (at most
  `search.decomposition.max_subqueries`), provenance/content dedup and an
  opt-in diversity pass that never displaces pinned exact matches.
* Typed `EvidenceRef` citations that are re-resolved against the same
  snapshot; unsupported or unresolvable evidence yields a partial or
  insufficient-evidence result instead of invented certainty.

### Unchanged
Fresh install, fresh index only (ADR 0032). The server schema identity is
bumped to `schema_version: 2`; indexes with any other identity are refused
with the existing reset/fresh-prefix instruction. Nothing is converted in
place, imported or carried over.

## Acceptance contract
Phases P0-S01 … P0-REL list their acceptance criteria in the plan; the
PR description maps each criterion to the test that covers it. A
criterion whose evidence needs hardware or data this repository cannot
carry (for example the 150-source/200000-file run on fixed hardware) is
marked as such with the exact command to reproduce it; it is not claimed
as passed.

## Consequences
* Every user-facing knowledge operation runs against either backend.
* Server-mode indexing keeps a disposable local staging cache; the cost of
  losing it is one reconversion.
* Benchmarks separate cold, warm, edit, query and failed-source scenarios;
  no speed claim is made without a measurement in `benchmarks/`.
