# ADR 0035: Server publication by MVCC generations

Status: proposed. Acceptance needs a live-cluster measurement of today's
copy-forward at 8k and 50k files (see "Measurement"). Interim hardening
(below) has shipped.

## Context

Server mode publishes each pass as a new immutable build (ADR 0033). Files
whose knowledge digest did not change are **copied forward**: an
`_update_by_query` adds the new build id to the records' multi-valued
`build_id`. Readers filter on `build_id = <active build>`. The graph
generations in the relationships index work the same way.

Copy-forward is cheap on the client and expensive on the cluster. An
update by query rewrites every matched document, so a one-file edit in a
50k-file source reindexes about 50k records per index, vectors included.
Every rewritten chunk or entity with a `knn_vector` / `dense_vector` field
goes back into a new segment, so its HNSW graph is rebuilt during merges.
Publication cost therefore grows with the size of the source, not the size
of the change. Large sources also ran into client timeouts while one
synchronous `_update_by_query` call ran for minutes.

## Decision

Records become immutable and carry a validity interval in **generation
numbers**. A generation is a per-source integer that increases by one at
each successful publish.

* Every build-scoped record (files, entities, documents, chunks, and graph
  edges and links) gets `valid_from: long` and `valid_to: long`.
  `valid_to = i64::MAX` means "still current". The multi-valued `build_id`
  field goes away.
* The source-state document records `generation: G` (the published one)
  next to `active_build_id`. The graph record gets its own
  `graph_generation` counter.
* **Readers** filter `source_id = s AND valid_from <= G AND valid_to > G`.
  They already resolve the source's state once per request (ADR 0033), so
  this replaces the build-id term with two range clauses on `long`
  fields. Range filters on numeric doc values are cached per segment.
* **Publish** of generation `G+1`:
  1. Begin the pending build, fenced by the writer lease (unchanged).
  2. Bulk-write the changed and new files' records with
     `valid_from = G+1, valid_to = MAX`. These records are invisible: no
     reader uses `G+1` yet.
  3. Close the replaced records (changed and deleted files) with a
     partial update by id: `valid_to = G+1`. That is one small bulk
     `update` action per record, built from the ids the staging store
     already knows. There is no query and no script. Readers at `G` still
     see them, because `G < G+1`.
  4. Compare-and-swap the state document to `generation = G+1` (fenced by
     the lease and the expected `G`). This is the only visibility switch.
* **Unchanged records are not touched.** A one-file edit writes about two
  times that file's records (the new versions plus the closing updates)
  and copies nothing.
* **GC** deletes records with `valid_to <= G - retained` after
  `gc_grace_seconds`. That is one `_delete_by_query` on a numeric range,
  run as a detached task (interim hardening below). Records of an aborted
  pending generation (`valid_from = G+1` when the published generation
  is still `G`) are deleted the same way.
* **Rollback**: a failed publish never moved `generation`. The new records
  are unreachable and GC removes them. Closing updates of step 3 must be
  undone. Before the CAS, step 3 writes `valid_to` into a staging field
  (`closing_at = G+1`), and readers ignore `closing_at`. The CAS promotes
  the generation, and the closes are then applied by a by-id update that
  is idempotent and resumable from the pending build record. A crash
  between the CAS and the closes leaves duplicates visible, both old and
  new versions with `valid_to = MAX`. Readers therefore also require
  `closing_at` to be absent or greater than `G`, which hides them. This
  keeps the CAS the only switch.
* **Graph indexes** use the same scheme with `graph_generation`. The
  graph's "base build" fence becomes "base generation": a graph derived
  from base `G` is promoted only while the source's base generation is
  still `G`.
* **Lease fencing** is unchanged. Every write carries the lease token
  check of ADR 0033, and the CAS compares lease, generation and (for
  the graph) base generation.

### kNN pre-filtering

Vector queries filter on the same interval. On OpenSearch (Lucene engine)
and Elasticsearch, `knn` with a `filter` pre-filters exactly. The filter's
cost is a bitset over matching docs, per segment, and it is cached. Today
the filter is a `term` on `build_id`, with about the same selectivity. The
risk is in sources with many closed versions before GC runs: the HNSW
search explores more filtered-out nodes. GC with a short retention keeps
closed versions below a few percent of live ones. The measurement below
must confirm a recall@10 and p95 latency change of no more than 5%.

### Migration

New mappings (`valid_from`, `valid_to`, `closing_at`; no multi-valued
`build_id`) mean a server schema version bump. Under ADR 0007 an existing
prefix is never modified in place. `docmonk server init` refuses
mismatched indexes, and operators rebuild into a new prefix (or delete and
re-init the old one) and run a full index. No dual-read period.

## Interim hardening (shipped with this ADR)

Until MVCC lands, the existing by-query operations no longer depend on a
single long HTTP request:

* `_update_by_query` (copy-forward, build detach) and `_delete_by_query`
  (GC) run with `wait_for_completion=false&slices=auto`. The client
  then polls `GET _tasks/<id>?wait_for_completion=true&timeout=<w>`, with
  `w = min(10 s, request_timeout / 2)`. Each poll is a bounded request
  and also serves as a heartbeat. An HTTP 408 from a poll means the task
  is still running.
* A per-task budget, `storage.server.task_timeout_seconds` (default
  3600), cancels a task that runs past it (`POST _tasks/<id>/_cancel`).
  The error aborts the pass, and the previous build stays visible.
* A server that answers inline (no task id) is still accepted, so mocks
  and older proxies keep working.

## Measurement (gate for acceptance)

On a live OpenSearch and Elasticsearch cluster, at 8k and 50k files:

1. Today: wall time, records rewritten and cluster CPU of a one-file-edit
   publish (copy-forward). Repeat with a forced merge afterwards, to
   capture the HNSW rebuild cost.
2. A prototype of steps 2–4 on a copy of the indexes: the same numbers,
   plus recall@10 and p95 for kNN with the interval filter, against the
   build-id filter.

The decision becomes accepted when (2) writes at most 2 times the changed
file's records, copies none, and keeps the kNN deltas within 5%. Until
then, P7.3 (the implementation behind the schema bump) stays open.

## Consequences

* Publication cost follows the change, not the source size. Vectors of
  unchanged records are never re-indexed.
* Reads gain one range clause. The `build_id` term and the copy-forward
  code (and its `ATTACH_SCRIPT` / `DETACH_SCRIPT`) go away.
* GC becomes a numeric range delete. Retention is counted in generations,
  not build ids.
* The graph and base share one mechanism, with independent counters.
