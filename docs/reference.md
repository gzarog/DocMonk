# DocMonk reference

Detail behind the [README](../README.md) pitch deck.

## Document formats

| Format | What you get |
|---|---|
| **PDF** | Layout model: headings, tables, page numbers · OCR for scanned pages (`documents.ocr: auto`) |
| **DOCX / ODT** | Heading structure with page provenance |
| **PPTX / ODP** | Slide titles and body text, slide-number provenance |
| **XLSX / ODS / CSV** | Tables rendered and chunked row-by-row |
| **HTML / Markdown** | Wikis, docs-as-code, ADRs, runbooks |
| **TXT / EPUB** | Plain text and e-books |
| **PNG / JPG / TIFF** | Opt-in OCR (`documents.image_ocr: true`) |
| **EML** | Subject, headers and body · supported attachments indexed as child documents (see below) |

- A corrupt or unsupported file is isolated and recorded as failed; the run continues.
- Older binary `.doc` / `.ppt` / `.xls` files and `.rtf` are detected but not converted.
- PDF conversions are cached by content hash; unchanged PDFs skip the layout model.
- Chunk sizes follow the embedding model's real tokenizer (`documents.chunking.max_tokens: auto`); tables split at row boundaries.

## Email attachments

An `.eml` file's MIME attachments in any supported format above (PDF, DOCX, XLSX, PPTX, TXT, …) are converted with the same pipeline as standalone files and indexed as **child documents of the email**. The `.eml` stays the only indexed file: no extra file rows, queue jobs or status counts are created for attachments.

- Search results for attachment text keep the real `.eml` path and add `location.attachment` (`name`, `content_type`, `format`, `index`, `parent_title`); the CLI prints `Attachment: <email.eml> -> <name>`.
- Changing or deleting the `.eml` replaces or removes all of its attachment knowledge.
- Unsupported (`.zip`, `.exe`, …), empty, oversized or corrupt attachments are skipped or reported without affecting the email body. Archives are not unpacked and attached emails (`message/rfc822`) are not recursed into.
- Filenames are display metadata only; attachments are written under internal temporary names and removed after conversion.

| Setting | Default | Effect |
|---|---|---|
| `documents.email_attachments` | `true` | `false` restores body-only `.eml` indexing. |
| `documents.email_attachment_max_bytes` | `26214400` (25 MiB) | Largest decoded attachment converted. |
| `documents.email_attachment_max_count` | `50` | Attachments processed per email. |
| `documents.email_attachment_total_max_bytes` | `104857600` (100 MiB) | Total decoded attachment bytes per email. |

Toggling `email_attachments` reprocesses `.eml` files (only those) on the next index; previously indexed emails pick up their attachments without being touched.

## Search

```bash
docmonk search "remote work equipment allowance"   # --table · --json · --limit N · --explain · --hybrid
docmonk docs [--source <id>]                       # list indexed documents
```

### Evidence, hard filters and grounding

```bash
docmonk evidence "who calls SettlementService.process" --json
docmonk evidence "compare billing and ledger retry limits" --source <id> --source <id>
docmonk search "refund policy" --path docs/ --kind document --no-attachments --explain --json
```

`docmonk evidence` (MCP: `docmonk_evidence`) routes the query to a typed intent (`exact_symbol`, `path_lookup`, `code_navigation`, `impact_analysis`, `document_fact`, `conceptual`, `cross_source_comparison`, `multi_hop`, `ambiguous`) without any model or network call, then runs only the strategies that intent needs.

- **Hard filters** (`--source`, `--path`, `--kind`, `--document`, `--no-attachments`, also on `search`) are applied before retrieval and to every stage: lexical, semantic, graph callers, context and every subquery. No stage can widen them.
- **Decomposition**: comparisons, flows and multi-part questions are split into at most `search.decomposition.max_subqueries` (default 4) subqueries; each comparison side keeps its best hit.
- **Duplicates** are removed by provenance and content; at most `search.diversity.per_document_cap` chunks per file. `search.diversity.enabled` (MMR, off by default) is opt-in: on the P0 evaluation it lowered comparison coverage.
- **Citations**: every result is an `[E#]` with source, build, record, path, line/page and a content fingerprint, re-verified against the same snapshot; a deleted file, a stale build or a filtered record is reported `valid: false`.
- **Verdict**: `supported`, `partial` (a part of a multi-part question has no support), or `insufficient_evidence` (nothing reaches `search.grounding.min_coverage` of the query's content terms). `docmonk ask` abstains without calling the provider on `insufficient_evidence`, gives the model only verified evidence as untrusted data, and reports uncited sentences and unknown citation ids under `grounding.answer_check`.
- The whole run is bounded by `search.max_query_budget_ms` (default 5000); skipped stages are listed under `diagnostics.degraded`.

| Setting | Default |
|---|---|
| `search.routing.enabled` | `true` |
| `search.decomposition.enabled` / `.max_subqueries` / `.llm_enabled` | `true` / `4` / `false` |
| `search.diversity.enabled` / `.lambda` / `.per_document_cap` / `.per_source_cap` | `false` / `0.7` / `3` / `0` |
| `search.grounding.enabled` / `.min_coverage` | `true` / `0.5` |
| `search.max_query_budget_ms` | `5000` |
| `search.ann.ef_search` / `.num_candidates` | `400` / `0` (automatic: `max(10 * k, 100)`) |
| `search.anchors.enabled` | `true` |
| `search.exact_occurrences.enabled` / `.max_results` | `true` / `50` |
| `search.adaptive_lexical.enabled` | `true` |
| `search.selective_graph.enabled` / `.max_seed_entities` | `true` / `25` |
| `search.cache.enabled` / `.max_entries` / `.ttl_seconds` | `true` / `64` / `300` |
| `search.max_concurrency` | `8` |

### URL anchors and exact occurrences

<!-- clean-slate-audit: allow-start (API URL paths with a version segment are user data) -->

```bash
docmonk explore "Is /sb/v1/content/groups/image used anywhere? Trace the callers." --json
docmonk explore "/sb/v1/content/groups/image" --explain
```

An API path written anywhere in a question (`/a/b`, `~/a/b`, `https://host/a/b?q`, `api/...`, `v1/...`, or any path with a `{param}`) is the question's primary anchor. The intent is `endpoint_reference` in `explore`, `ask`, MCP, `evidence` and `search --explain`, and documentation keywords never override it. Slash-separated prose ("route/client/API", "and/or") and file paths (`src/a.cs`, `C:\repo\a.cs`) are not URL anchors. Anchors appear under `anchors` (explore) and `plan.anchors` (evidence).

Exact occurrences are looked up by a match key. The key is the path only, with parameters (`{id}`, `{id:int}`, `<int:id>`) written `{}` and letters lower-cased. Segment boundaries are kept, so `/x/image2` and `/x/image/thumbnail` never match `/x/image`. Each occurrence has an `evidence_class`:

| Class | Meaning |
|---|---|
| `direct_exact_literal` | The path appears verbatim in a code string literal (owning entity and line given) |
| `statically_composed_route` | An ASP.NET controller `[Route]` prefix plus an action `[Http*]`/`[Route]` template (or a Flask/FastAPI decorator) composes to the path. This is static evidence, not runtime proof |
| `document_reference` | A document chunk whose stored text contains the path (verified, not just a full-text match) |
| `heuristic_match` | A template (`/x/{id}`) or interpolated literal (`$"/x/{id}"`) that may produce the path |
| `resolved_code_relationship` | A graph relationship (caller, reference) of a matched entity |
| `unresolved_or_not_indexed` | A source that could not be checked |

Code occurrences come from the relationship graph, derivation `graph-3` or later. A source with an older or invisible graph is listed under `exact_capabilities` / `exact_unchecked_sources`, and `exact_complete` is `false`: zero hits there do not mean "unused". Rebuilding the graph adds the projection (`docmonk index` recomputes graphs after an upgrade) without reindexing content.

Once exact seeds exist, explore reads only the sources that hold them. Callers of the owning entities (handlers, client methods) are traversed within the usual depth and edge caps. Lexical search runs on the anchor, not on the sentence, and skips its loosest tier. Semantic search never runs for an endpoint question with verified exact evidence.

`--explain` (MCP `explain: true`) adds `diagnostics`:

- the route and its signals;
- per-stage `duration_ms` and hits for exact occurrences, lexical search and its sub-stages, symbols, graph stages, linked documents and semantic search;
- service stages: plan, graph freshness, absolutize;
- source counts considered, searched and expanded;
- graph seeds and edges, and `traversal_truncated`;
- exact reads and rejected document candidates;
- backend kind, requests and bytes;
- build ids;
- for the CLI, config load and `open_sources` time.

Without `--explain` the output is unchanged and no diagnostics are kept.
<!-- clean-slate-audit: allow-end -->

Optional profiles (set with `docmonk config set`; none enables an expensive model by default):

| Profile | Settings |
|---|---|
| code-heavy | defaults; `search.semantic false` |
| document-heavy | `search.semantic true`, `search.lazy_semantic true`, `search.grounding.min_coverage 0.6` |
| mixed | `search.semantic true`, `search.reranker.enabled true` (measure latency first: the reranker raised p95 in `benchmarks/semantic-quality.json`) |

**Long questions.** The lexical planner is bounded. The "at least half the terms" tier is expanded into AND groups only while there are at most 20 groups. Beyond that it becomes an OR of at most 16 terms, identifiers first. At most 64 content terms are planned, and an expression longer than 4 KiB is dropped. Server readers prefetch only the precise tiers in one `_msearch` (at most 32 searches per request) and read looser tiers only when needed (`search.adaptive_lexical.enabled`). At most `search.max_concurrency` sources are read at once.

**Lexical matching.** Identifiers are also indexed as their words
(`cancelOrder`, `CancelOrder2` → `cancel order 2`) with English stemming, so
a prose query ("cancel orders") finds `cancelOrder`. A multi-word query
first requires every term, then falls back to "most terms" before any
looser match. A path typed exactly ranks first. Hard filters are pushed
into the SQL (server: the query), not applied afterwards.

**Warm queries.** A long-lived process (MCP, Admin UI, daemon) keeps the
embedder, the reranker and each project's vector index in memory, embeds
a query once per request and reuses pooled read connections. The vector
index is reused until the build's vectors change. The reranker scores
only the hybrid tier, on passages of at most 256 words.

Quality is gated in CI by the 72-query golden set and by a 250-query generated evaluation (`crates/docmonk-service/tests/p0_eval.rs`, held-out floors in `fixtures/p0/eval-gates.json`, last report in `benchmarks/p0-retrieval-eval.json`). Its labels come from the corpus generator, not from independent human labeling.

## Code

<!-- clean-slate-audit: allow-start -->

Full extraction for Python, JavaScript, TypeScript/TSX, Go, Java, Rust and C#; HTTP routes from Python decorators and C# route attributes. Other languages are indexed for full-text search only.

<!-- clean-slate-audit: allow-end -->

```bash
docmonk symbol | callers | callees | references | impact  <Symbol>
docmonk link list
docmonk link add <entity> <document> [--section ID]
docmonk link remove <ID>
```

## Indexing speed

| Setting | Default | Effect |
|---|---|---|
| `documents.pdf_mode` | `accurate` | `fast` reads only the PDF text layer (no layout or table models, no model loading). On a 24-PDF test corpus: 81s to 1.6s. Loses heading/table structure and multi-column reading order; low-text PDFs still follow `documents.ocr`. |

Changing `pdf_mode` reprocesses PDFs on the next index (each mode has its own conversion-cache entries).

Cross-domain linking (code identifiers mentioned in documents) uses a word index plus a pattern cache instead of testing every identifier against every document section: a 2,100-file cold index spent 222s linking before and 0.7s after, with identical links.

## Indexing behaviour

- `docmonk index` keeps going when one source fails, reports it, and exits non-zero at the end.
- **Two stages per run, with a barrier.** Stage 1 indexes and publishes every selected source (scan, diff, conversion, code entities, chunking, embeddings). Only after **all** of them finished does stage 2 build relationship graphs (code edges, cross-file resolution, code-document links, manual link projection), for the sources that published. Search over a newly published index is available as soon as its stage-1 pass publishes; a graph never holds it back. The daemon applies the same barrier: graph work runs once no index pass is queued or running. `indexing.pipeline_relationships: true` (off by default) drops the barrier for `docmonk index`: each source's graph is built as soon as its own index publishes, so one large source no longer holds back the graphs of small ones.
- **`indexing.relationships_enabled: false`** skips stage 2 entirely. Existing graphs are hidden from graph queries (callers, callees, impact, links) but never deleted, and no other indexed knowledge is touched. Toggling the setting never reindexes content: re-enabling it re-validates the retained graph against the published index.
- **Every graph belongs to one exact published index.** A graph records the base build and generation it was derived from. While a newer graph is pending or building after a base update, graph queries keep serving the last committed graph of that source and flag it in their JSON output (`graph`: one entry per source with `state`, `generation`, `visible` and `stale: true`); `graph.require_current: true` returns nothing instead, until the graph matches the active base. `status` says why a graph is not current: `pending`, `building`, `stale`, `failed` or `disabled`. Dependency analysis always requires a current graph. Graphs are published atomically (local: one SQLite transaction; server: an invisible graph generation promoted by one compare-and-swap fenced by the writer lease and the expected base build), so a failed or interrupted graph build is never visible.
- **Stage 2 never redoes stage 1.** It derives edges from the published files, re-reading each code file and checking it against the published content hash: a file edited after indexing makes the graph `stale` (nothing is published, never a mix of snapshots) until the next index pass. Updates are incremental: only changed files are re-derived, and per-file symbol dependencies (what each file defines and which qualified/bare names its cross-file references look up) limit re-resolution to the changed files plus the unchanged files that reference a symbol whose definitions changed; candidates are looked up per symbol and only changed outcomes are written. Changes are semantic: a file counts as changed only when its kind, status or content hash changed (never its write time), and a reference is re-resolved only when the candidates of a name it looks up changed (a definition added, removed, renamed or reordered; a body-only edit re-resolves no other file). In server mode a new graph generation copies unchanged files' edges and links forward server-side (in place, no duplicate records) and bulk-writes only changed files.
- **Graphs outlive base builds.** Each graph keeps a dependency manifest (semantic file keys, per-file symbols, an input digest that also covers the manual link intent). A source whose graph is current is skipped (in server mode without taking the writer lease); a republished base with identical inputs is *rebound* to the new snapshot without parsing or resolving anything; a full base rebuild carries the graph into the new build and re-derives only what changed. The server stores the manifest with the graph generation, so a host that lost its staging cache (or a new writer host) adopts the published graph instead of deriving it again. A missing, outdated or corrupt manifest falls back to a full graph recomputation of that source (reason-coded, still without reindexing).
- **Cross-source dependencies are explicit.** A consumer source's references that stay unresolved locally are looked up, by exact qualified name only, in the producer sources it depends on: C# `<ProjectReference>` items pointing into another registered source are discovered, anything else is declared in `indexing.relationship_dependencies` (`"<consumer> -> <producer>"`, source ids or paths). Package references are listed as unsupported, never guessed; sources outside a dependency are never consulted, and a name defined in several producers is recorded as ambiguous. Bindings are a separate layer: the consumer's own graph rows are unchanged. A consumer is updated only when a candidate it looks up changed (a producer's implementation-only edit leaves it alone), also when the run indexed only the producer (`--source`); unavailable or disabled consumers and producers are reported `stale`/`pending`, and the next run retries them.
- **`indexing.graph_fact_cache: true`** (experimental, off by default) has stage 1 keep each code file's graph facts (its edges and unresolved references, from the parse it already makes) in a disposable cache, `<home>/cache/graph-facts.db`, keyed by file, path, content hash and derivation. Stage 2 still reads and hashes every file it derives, so a file edited after indexing still makes the graph `stale`; only the second parse is skipped when the bytes match a cached key. Turning it on never reindexes anything (files indexed before simply fall back to parsing), entries of superseded content are dropped after each graph build, and deleting the file or turning the setting off only restores the parse. `docmonk relationships build --json` reports `facts_hits`/`facts_misses` per source.
- **Graph failures are reported separately.** A source whose index published but whose graph failed or went stale is a partial success: its index stays published and searchable, the run prints a `relationships ...` line and a `Relationships:` total, and the run exits with the partial-failure code. `docmonk relationships build [--source ID] [--json]` retries or initializes graphs without reindexing anything.
- **Full builds commit in segments and resume.** A full build commits every 250 files (or 64 MB) while it is still invisible, so the write lock is released regularly and the WAL stays bounded (64 MB `journal_size_limit`, passive checkpoints). If the process dies, the next run resumes the pending build when its versions match: files already committed with an unchanged size and mtime are not processed again. Embedding runs after the last segment, with no write transaction open. Incremental passes stay one atomic transaction.
- **Change detection.** A file is hashed only when its size or mtime changed, or when its mtime falls within 2 s of the moment it was indexed (the racy-clean rule: a same-size rewrite in the same timestamp tick is still caught). New files are hashed by the worker that processes them.
- **Watcher.** A folder created, moved in or moved out is reflected by a targeted pass (its files are walked or dropped); a walk of more than 200 files becomes a full pass. A watcher error or lost events (queue overflow) queue a full pass.
- **Vector index.** An incremental update appends its changes to `ann/index.hnsw.delta` instead of rewriting the index; the index is rewritten once the log reaches a quarter of its size.
- **Last Scan** is the last completed filesystem scan; a later failure shows up as **Last Error**.
- An unreachable source is marked **OFFLINE** and its knowledge is never deleted.

### Where a relationship build spends its time

`docmonk relationships build --json` (and the `relationships` object of `docmonk index --json`) reports per source what the graph stage did and how long each phase took, alongside the existing fields:

- `timings`: `state_load`, `clear`, `derivation` (wall clock), `read_hash` and `parse` (summed over parser threads), `cache_load`, `sqlite_write`, `symbol_manifest`, `resolution`, `linking` and `commit` seconds. A skip or rebind leaves them at zero.
- `counters`: files of the build and files derived, bytes read, fact-cache hits/misses, resolution work (`references_examined`, `candidate_queries` statements, `candidate_rows`, `changed`) and the SQL statements the build issued.
- `linking`: entities, units and routes loaded (`narrow` when only the touched files' were), candidates, and load/index/match/write/manual seconds with the match time per resolver.
- Server mode, `server_publish`: `timings` (`staging`, `bulk`, `close`, `refresh`, `promotion` seconds), `files_kept` / `files_written` / `records_closed`, and `counters` (staging SQL statements, HTTP requests and bytes, close and bulk requests, staged files/edges/links). `rebound: true` with no records written means the visible generation was pointed at the new base: either its inputs were identical, or the change altered no graph record.

The run summary adds the same phase totals over all sources. While a graph is building, `docmonk status` shows the pass as `relationships: <phase>` (`deriving`, `resolving`, `linking`, `committing`; server: `staging`, `writing`, `closing`, `promoting`) with the files derived so far: a pass at `40/40 files` that is `resolving` has not finished.

**Troubleshooting a long `building` or `publishing` graph.** Look at the phase first. `deriving` scales with the changed code files (each is read, hashed and parsed; `indexing.graph_fact_cache` removes the parse). `resolving` scales with the files that reference a changed symbol: renaming a method called by bare name from thousands of files re-resolves all of them (in bounded batches, so it stays a few seconds). `linking` after a document edit matches that document against every entity. On a server, `writing` and `closing` grow with the changed files only: unchanged records are never rewritten. A pass whose heartbeat stays fresh is working, whatever the phase; `stalled` means it stopped heartbeating.

`docmonk-graph-bench` (in `docmonk-service`) reproduces these measurements on a generated C# and Markdown corpus of any size, with no proprietary source: see `benchmarks/relationships/README.md`.

## Status and observability

`docmonk status` answers: is indexing running, progressing, stalled or failing, where, and what failed. It is built by one crate (`docmonk-status`, ADR 0034); the CLI, the MCP `docmonk_status` tool and the Admin UI (`/`, `/api/status`, `/ready`) all show the same report.

| Flag | Effect |
| --- | --- |
| *(none)* | Backend and health, totals, every active pass, one row per source (past 25 sources, completed ones fold into one line), problems, newest errors |
| `--watch [--interval 2]` | Redraws in place; throughput only between comparable samples; Ctrl+C exits cleanly |
| `--errors` | Only sources that need attention, problems and recent errors. Never says "No problems found" when part of the status could not be read |
| `--verbose` / `-v` | Full ids and paths, builds, run ids, permits, home lock, last run and diagnostics (request count, cache age, consistency) |
| `--json` | The canonical report (`schema_version` 1) in the CLI envelope; not combinable with `--watch` |

Exit code: `0` when a report was produced (its health is in the report), `4` when the authoritative backend cannot be inspected (an unreachable server is never shown as an empty or stale report).

**Where the numbers come from.** Local mode reads only the home's SQLite catalog and project stores, plus this home's progress file and locks. Server mode reads only the configured OpenSearch/Elasticsearch prefix: the catalog, the active builds, published counts, file errors and the `{prefix}-runtime` heartbeats of every indexing host. Server mode never opens local SQLite for status.

**Meaning of the counters**

- `sources[].published`: the source's **published** (active) build only: files by status (`indexed`, `failed`, `retrying`), code entities, documents, chunks, relationships, links, the highest attempt count and the next retry time. A pending build is never counted. `null` when nothing is published or the section could not be read.
- `sources[].relationship_state`: the source's relationship graph, independent of `index_state`: `state` (`disabled`, `pending`, `building`, `ready`, `stale`, `failed`; a graph of an older base reports `stale`), the published `generation`, the `base_build_id` it was derived from, `last_success_at`, `last_error` and `stale_reason`. A `failed`/`stale` graph is a `relationships_not_current` warning; the index itself remains searchable. Relationship and link counts in `published` are those of the visible graph only.
- `sources[].live` and `indexer.workers`: passes **in progress**: run id, host, stage (`starting`, `scanning`, `processing`, `finalizing`, `publishing`), `scanned`, `planned` (files to process, known once the scan finished), `processed`/`indexed`/`failed`/`retry`, `percentage` (`null` without a planned total) and the heartbeat age. Several sources show several passes; nothing overwrites another pass.
- `summary.files.pending_in_current_runs`: planned minus processed over live passes; `null` when a live pass does not know its planned total yet.
- `last_scan_at`: a filesystem scan finished. It does not mean the source was indexed; a source is `completed` once a build is published and no pass is running.
- `recent_errors`: newest first by the time the error happened (`occurred_at`; server: the file's `last_error_at`), ties by source id and path, selected across all sources before truncating (25 by default, at most 200).
- Values that cannot be observed (pre-scan totals, remote PIDs, server disk usage, in-memory queues, ETAs) are `null` / `N/A`, never `0`. A section that failed to load is listed in `diagnostics.missing_sections` and the report is `partial`.

**States**

- **Access** (`access`): `online`, `offline`, `disabled`, `unknown`: whether the root is reachable, independent of indexing.
- **Index state** (`index_state`), first match wins: `disabled` > `offline` > a live pass (`queued`, `scanning`, `indexing`, `finalizing`, `publishing`) > `stalled` (a pass stopped heartbeating, or its server lease expired) > `unknown` (counters unreadable) > `retrying` > `failed` (the build failed and nothing is published) > `completed` > `not_indexed`. A failed pending build whose previous build still serves stays `completed` with a warning.
- **Liveness of a pass**: `live` (local: its process exists and its heartbeat is fresh; server: its lease token is still the source's live lease and its heartbeat is fresh), `stalled` (heartbeat older than `indexing.status_stall_threshold_seconds`, default `120`), `expired` (process gone, or lease expired/superseded: not running), `finished`. Heartbeats come from an independent ticker every 5 s, so a long OCR, embedding or linking stage stays alive without counting files; raise the threshold only if a host is overloaded.
- **Health**: `failed` on any error problem (backend unreachable, cluster `red`, stalled pass, crashed or failed run, a build failure with nothing published, all sources offline); `unknown` when the report is partial; `degraded` on any warning (cluster `yellow`, failed or retrying files, an offline source, an expired pass, a failed build that still has a previous build); else `healthy`. Every problem has a stable `code`, `severity`, `scope`, optional `source_id`/`host`, a hint and its observation time.

**Server mode cost.** One full snapshot reads the catalog once (paged by 1000), then per 1000 sources one file aggregation, one record aggregation, one relationship-graph aggregation and one error query, plus run heartbeats, cluster health and one consistency re-check: 8 HTTP requests for 150 idle sources. `--watch` keeps one connection and re-reads only the catalog, heartbeats and cluster health (3 requests) while no build is published; published counts are re-read on publication or after 30 s (the age is shown).

**Server schema.** Status adds `{prefix}-runtime`, `files.last_error_at` and `source-state.published_at` (schema identity 3). An existing prefix created by an earlier build is refused, never converted: use a fresh `storage.server.index_prefix`, or delete the DocMonk indexes of the old prefix yourself (`DELETE /<prefix>-*` on the cluster) and run `docmonk server init` and `docmonk rebuild`. DocMonk never deletes indexes automatically.

## Index locking

- Each source has its own writer lock, `locks/index-<source_id>.lock`: one pass per source at a time (CLI, daemon/watch, Admin UI, rebuild), while different sources index in parallel. Readers/search are never blocked.
- Whole-home operations (backup, restore, vector maintenance) take `locks/index.lock` and then every source lock in id order, so they never interleave with a pass; locks are always taken in that one order.
- In server mode a source pass also holds the source's **server writer lease** (`storage.server.lease_seconds`, renewed while the pass runs). Each grant increments a fencing token: a superseded or expired holder (for example a stalled process on another host) can no longer publish, abort or delete newer state.
- Waiting for a lock is bounded by `indexing.lock_timeout_seconds` (default `30`, must be > 0 and <= 3600). On timeout the error names the likely owner (PID, operation, source), e.g. `Another DocMonk process holds index-src_example.lock (PID 44884, operation=index, source=src_example); timed out after 30s`. The daemon retries a contended source after a short backoff.
- Find the competing process with `docmonk doctor` or the PID in the error. Owner metadata is informational only (PIDs can be reused).
- Do **not** delete lock files to recover: the OS lock is released automatically when its owner exits or crashes, and deleting the file can break exclusion.

## Parallel indexing and resource budgets

`docmonk index`, `docmonk rebuild`, the Admin UI and the daemon run up to `indexing.max_parallel_sources` sources at once. All of them draw from one process-wide set of bounded permits, so adding sources never multiplies worker pools beyond the host:

| Setting | Default | Bounds |
|---|---|---|
| `indexing.max_parallel_sources` | `0` = `min(4, max(1, cpus/2))` | source passes running at once (1..=32) |
| `indexing.cpu_workers` | `0` = available CPUs | CPU-bound file processing across all sources |
| `indexing.ocr_workers` | `1` | heavy document conversions (PDF, scans, images) at once |
| `indexing.embedding_workers` | `1` | embedding model batches at once |
| `indexing.io_concurrency` | `8` | server requests at once |
| `indexing.max_in_flight_mb` | `512` | estimated bytes of files queued or being processed |
| `indexing.fairness_max_wait_seconds` | `120` | daemon: longest a queued source waits behind small watcher-triggered passes |
| `indexing.max_targeted_paths` | `512` | daemon: touched paths kept per source before the pass becomes a full scan |

A small code edit keeps making progress during bulk OCR (OCR holds its own permit). A failing, offline or panicking source never blocks the others; permits and locks are returned on every error path. `docmonk status` shows every active source pass and, for local runs, per-class permit counters (`indexer.resources`: capacity, in use, peak, waits).

The daemon runs one worker per `max_parallel_sources`. A source has at most one queued follow-up while its pass runs; small watcher-targeted passes go first, but a source that has waited `fairness_max_wait_seconds` is started before anything else, so no source starves.

## Admin UI

`docmonk ui` → http://127.0.0.1:8765. Manage sources, index with live progress, browse document chunks, test search, explore the code graph, edit config, control the daemon, backups and logs. Binds to `127.0.0.1` only, with CSRF protection and host-header validation. See [ui.md](ui.md).

## MCP (AI agents)

```bash
docmonk serve --mcp   # register this command as a stdio MCP server in your client
```

Tools: `docmonk_explore`, `docmonk_search`, `docmonk_documents`, `docmonk_symbol`, `docmonk_callers`, `docmonk_callees`, `docmonk_impact`, `docmonk_status`, `docmonk_ask`.

## Optional AI

```bash
docmonk config set search.semantic true                  # local semantic search
docmonk config set ai.provider ollama
docmonk config set ai.model llama3.2
docmonk ask "how are settlement retries handled?"
```

Providers: Ollama, OpenAI-compatible, OpenAI and Anthropic (require `privacy.external_ai_allowed: true`), Codex and GitHub Copilot (beta, `docmonk ai login <provider>`; see [providers/](providers/)).

## Server storage (OpenSearch / Elasticsearch)

```bash
docmonk init --storage-mode server --storage-engine opensearch \
    --storage-url https://search.internal:9200
```

- Server support is built into every binary and the container image; there is nothing extra to install.
- `init` validates the real cluster first (reachable, right engine, permissions) and creates the DocMonk indexes. Tested with OpenSearch 2.15 and Elasticsearch 8.15 in CI.
- Credentials only via env vars: `DOCMONK_OPENSEARCH_{USERNAME,PASSWORD,API_KEY}` / `DOCMONK_ELASTICSEARCH_{USERNAME,PASSWORD,API_KEY}`. Never stored in `config.yaml`.
- **The cluster is authoritative.** The source catalog (registration, include/exclude patterns, enabled flag), build lifecycle, writer leases and all indexed knowledge (files, code entities, documents, chunks, relationships, links, vectors) live in `{prefix}-*` indexes. No local SQLite registry or knowledge store is read or written. Local files are limited to `config.yaml`, logs, locks, models and a **disposable** extraction cache under `cache/server-staging/` (deleting it only costs reconversion).
- An unreachable or uninitialized cluster is an error (exit code 4 / 3, typed `BackendUnavailableError` / `BackendSchemaMismatchError` in MCP); DocMonk never falls back to local data.
- Every pass publishes a new generation atomically (compare-and-swap of the source state, the only visibility switch). Records are immutable and carry the generations they are valid for (ADR 0035): files whose derived knowledge did not change are **not touched** (nothing is rewritten, reconverted or re-uploaded); changed and new files are written as the new generation and the records they replace (and deleted files' records) are closed. A failed, interrupted or superseded pass never becomes visible, whatever it wrote; the replaced generation stays readable for `storage.server.gc_grace_seconds` (default `60`) and is then garbage-collected.
- Closing replaced records, sweeping an aborted attempt and garbage collection run as server tasks (`wait_for_completion=false`, `slices=auto`) that DocMonk polls with short requests, so no request waits longer than `storage.server.request_timeout_seconds`. Each touches only the changed files' records. A task still running after `storage.server.task_timeout_seconds` (default `3600`) is cancelled and the pass fails, leaving the previous generation visible.
- **Schema version 5.** Generation-numbered records are a breaking server schema change: `docmonk server init` refuses indexes of an older schema and never alters them. Create the new indexes under a fresh `storage.server.index_prefix` (or delete the old prefix explicitly once you no longer need it for rollback) and run a full `docmonk index`. Manual links live in the old indexes: save them first (`docmonk link list --json`) and re-add them after the reindex if you want to keep them.
- Search requests per source are batched (`_msearch`): one lexical round trip per source instead of one per tier.
- Source roots are paths on the host that indexes them.
- Dev clusters: `docker compose -f docker/docker-compose.opensearch.yml up -d` (or `docker-compose.elasticsearch.yml`).

Capability matrix (server mode):

| Command / tool | Server mode |
|---|---|
| `source add/list/info/enable/disable/remove` | ✅ server catalog |
| `index`, `rebuild`, `daemon`, `watch`, Admin UI indexing | ✅ |
| `status`, `doctor`, Admin UI status | ✅ counts, retries and errors from the published builds; cluster health |
| `search` (lexical, semantic, hybrid), `explore`, `ask`, `docs` | ✅ |
| `symbol`, `callers`, `callees`, `references`, `impact` | ✅ |
| `link list/add/remove` | ✅ (manual links stored in the cluster) |
| MCP tools | ✅ same data as the CLI |
| `backup`, `restore` | ❌ exit 3 (`LocalStorageModeRequiredError`): use the cluster's snapshot API |
| `vectors rebuild/backfill` | ❌ exit 3: vectors are written by every build; use `docmonk rebuild` |

## Operations

```bash
docmonk backup [PATH]
docmonk restore ARCHIVE
docmonk rebuild [--source ID]
docmonk doctor
docmonk update check|status|install|rollback
docmonk vectors rebuild
docmonk uninstall [--keep-data]
docmonk config get|set KEY [VALUE]
```

## Updates

`docmonk update` follows GitHub's latest **full** release. To also receive
pre-releases, for example to test a release candidate, run:

```bash
docmonk config set updates.channel prerelease
```

The default is `stable`. Any other value is treated as `stable`.
`updates.enabled`, `updates.check_interval_hours` and `updates.notify`
control the background check and the "newer version" notice.
