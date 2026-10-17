# ADR 0035: Server publication by MVCC generations

Status: accepted and implemented (schema version 5). The live-cluster
performance gate (8k/50k files, recall@10 and p95 within 5%, see
"Measurement") is still to be run and recorded; until it is, the
performance acceptance of audit finding F10 stays open.

## Context

Server mode publishes each pass as a new immutable build (ADR 0033). Until
schema version 5, files whose knowledge digest did not change were
**copied forward**: an `_update_by_query` added the new build id to the
records' multi-valued `build_id`, and readers filtered on
`build_id = <active build>`. The graph generations in the relationships
index worked the same way.

Copy-forward is cheap on the client and expensive on the cluster. An
update by query rewrites every matched document, so a one-file edit in a
50k-file source reindexed about 50k records per index, vectors included,
and every rewritten `knn_vector` / `dense_vector` went back into a new
segment whose HNSW graph is rebuilt during merges. Publication cost grew
with the size of the source, not the size of the change.

## Decision

Records are immutable and carry a validity interval in **generation
numbers** of their *lineage*.

* Every build-scoped record gets `valid_from: long` and `valid_to: long`
  (`valid_to = i64::MAX`: still current). `build_id` stays, single-valued,
  naming the build that wrote the record (diagnostics and raw counts
  only; never a visibility filter).
* Two lineages per source: the **base** (files, code, documents, chunks)
  and the **graph** (relationship edges and links; manual links are not
  generation-scoped). Each is stored in the source-state document under
  `mvcc.base` / `mvcc.graph`:
  `{ published, next, pending, ids: {id: generation}, aborted: [...] }`.
* **Every publish attempt allocates a fresh generation** from `next`. Two
  attempts never share one: a superseded writer and its successor write
  different generations.
* **Aborted attempts are remembered.** An attempt is aborted when it fails,
  or when a newer attempt begins while it is pending. Readers ignore
  records an aborted attempt *wrote* (`valid_from` in `aborted`) and the
  *closes* it made (`valid_to` in `aborted` counts as open).
* **Readers** pinned to generation `G` of a lineage filter
  `valid_from <= G AND valid_from NOT IN aborted AND (valid_to > G OR
  valid_to IN aborted)`. A request resolves its build (or graph
  generation) id to `G` once, from the state document, and uses the same
  filter for path, vector, graph and attachment reads.
* **Publish** of attempt `g` over the published `G`:
  1. Begin: a fenced compare-and-swap of the state document allocates `g`
     (aborting any pending attempt).
  2. Bulk-write the changed and new files' records with
     `valid_from = g, valid_to = MAX`. Invisible: no reader uses `g` yet.
  3. Close the records those files replace, and deleted files' records,
     with `valid_to = g`: an `_update_by_query` restricted to those
     files' ids (`valid_from < g` and still open). Readers at `G` still
     see them (`g > G`). **Unchanged records are never touched.**
  4. Compare-and-swap the state document: `published = g`, fenced by the
     lease, the pending build id and the pending generation. **This is the
     only visibility switch.**
* **Why no closing window.** Closes are made before the switch and are
  invisible to readers of `G`. If the attempt dies or is superseded at
  any point, its generation is (or becomes) aborted, which hides both its
  writes and its closes. A superseded writer that keeps running after
  losing its lease cannot make anything visible: whatever it writes or
  closes carries its own aborted generation. No staging field and no
  post-switch repair step are needed, and a crash never exposes duplicate
  or missing records.
* **GC** after a publication: delete the lineage's records with
  `valid_to <= oldest readable generation` (published, or retired within
  `gc_grace_seconds`), excluding closes made by aborted attempts. An
  aborted attempt's own records are swept (written records deleted,
  closes reopened) immediately as housekeeping, and the attempt is
  forgotten once it is older than `max(gc_grace, 10 min)`, after another
  sweep, so a writer stalled past its lease cannot leave visible records.
* **Graph** publication uses the same scheme on the graph lineage. The
  graph's "base build" fence is unchanged: a graph derived from a
  superseded base is never promoted, and promotion is fenced by the
  lease, the active base and the pending graph generation.
* **Low-level `publish_build`** (a complete set of records) closes every
  earlier open record before switching; incremental publication
  (`publish_from_store`) closes only the replaced files' records.

### Alternatives considered

* **A `closing_at` staging field** (the first draft of this ADR): closes
  are staged, promoted after the switch by a resumable by-id update, and
  readers hide records whose `closing_at <= G` until then. Rejected: it
  needs a post-switch step and a reader rule for the window between the
  switch and the closes, and it does not protect against a superseded
  writer that keeps closing records.
* **Immutable generation aliasing** (readers resolve each file's current
  generation from a per-generation manifest): the filter grows with the
  number of files (a `terms` clause per source of up to 50k ids), and
  manifests must be written for every generation. Rejected.

### kNN pre-filtering

Vector queries filter on the same interval. On OpenSearch (Lucene engine)
and Elasticsearch, `knn` with a `filter` pre-filters exactly; the filter
is a per-segment cached bitset over two numeric ranges. Closed versions
before GC make the HNSW search explore more filtered-out nodes; GC with a
short grace keeps them a small fraction of live records. The measurement
below must confirm a recall@10 and p95 change of no more than 5%.

### Schema change and cutover

New mappings (`valid_from`, `valid_to`, `mvcc`) bump the server schema to
version 5. Under ADR 0007 an existing prefix is never modified in place:
`docmonk server init` refuses indexes of another schema version, and
nothing is migrated or deleted automatically. Operators create the new
indexes under a fresh prefix (keeping the old prefix for rollback until
they cut over), or delete the old prefix explicitly, and run a full
index. Manual links live in the old indexes and must be listed
(`docmonk link list --json`) and re-added if they are to be kept.

## Measurement (performance gate)

On a live OpenSearch and Elasticsearch cluster, at 8k and 50k files,
before (schema 4) and after (schema 5): wall time, records written and
updated, HTTP requests and payload bytes, cluster CPU of a one-file edit,
a new file, a deletion and a full reindex; kNN recall@10 and p95 with the
interval filter against the build-id filter. Acceptance: zero unchanged
records rewritten on a one-file edit, at most 2x the changed file's
records written plus closed, recall@10 and p95 within 5%.

Verified so far (in-process engine, `crates/docmonk-backends/tests/
publication.rs` and `graph_mock.rs`): a one-file edit in a 50-file source
writes exactly that file's records and closes exactly its previous ones,
with no other record touched; failed, killed and superseded attempts are
never visible; readers pinned to a retired build keep their snapshot.
The live numbers are not measured yet.

## Consequences

* Publication cost follows the change, not the source size. Vectors of
  unchanged records are never re-indexed.
* Reads gain two range clauses (and a `terms` on the few aborted
  generations). The `build_id` term, copy-forward and its
  `ATTACH_SCRIPT` / `DETACH_SCRIPT` are gone.
* GC is a numeric range delete; retention is counted in generations.
* The base and the graph share one mechanism with independent counters.
