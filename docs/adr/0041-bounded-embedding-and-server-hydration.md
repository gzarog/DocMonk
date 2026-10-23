# ADR 0041: Paged embedding passes, batched citation hydration and live ANN recall

Status: accepted; implementation in progress (local-model retrieval plan,
phase 5). The held-out evaluation on all three backends and the
embedding memory benchmark are still to be run.

## Context

* An embedding pass loaded every pending subject of a build (entity
  signatures, chunk texts, passage lines) into memory before embedding
  the first window. On a first index or after a model switch (ADR 0040),
  that is every text of the source at once, so peak memory grew with
  the corpus rather than with the batch.
* Evidence read each citation's canonical content (ADR 0038) once to
  fingerprint it and again to verify it. For a code citation that is an
  entity read plus a passage read, so on a server an answer with ten
  code citations cost about forty extra requests.
* `evidence` reported no backend costs: on a server it was not visible
  how many requests an answer took.
* Server kNN recall had been measured only for the local HNSW index
  (`benchmarks/ann-20000.json`), not for OpenSearch or Elasticsearch
  after generations are published and garbage-collected, nor under hard
  filters.

## Decision

1. **Keyset-paged embedding passes.**
   `ProjectStore::pending_embedding_subjects_page` reads at most `limit`
   pending subjects after a cursor. The cursor is a keyset position
   (subject kind, file, ordinal, id), never an offset. `embed_build`
   reads a page of 256 subjects, embeds it (cache first), writes its
   vectors and only then reads the next page.
   * Memory is bounded by the page and the model batch, not the build.
   * Vectors written between pages cannot shift the rows a later page
     reads.
   * An interrupted pass leaves complete pages behind, and the next pass
     embeds exactly what is left.
   * Blank texts are skipped but still advance the cursor.
   * `EmbedStats::max_window` records the largest page held.
2. **Citations are hydrated once, batched.**
   * `grounding::canonical_contents` reads the canonical content of all
     of an answer's citations per corpus. Entity rows and their body
     passages come in two batched reads (`entities_by_ids` and the new
     `KnowledgeRead::entity_passages_many`, a single terms query on a
     server).
   * `stamp_contents` fingerprints the citations, and `verify_with`
     reuses the same contents.
   * `verify` alone still re-reads everything, for citations a caller
     kept from an earlier answer.
3. **Backend costs are diagnostics.** `evidence` reports
   `diagnostics.backend` (kind, requests, bytes sent and received), like
   `explore --explain`.
4. **Live filtered ANN recall is measured**
   (`crates/docmonk-backends/tests/ann_recall.rs`, run by the server CI
   job). For each engine:
   * a synthetic clustered corpus of 384-dimension chunk vectors is
     published, then republished with a tenth of the vectors changed and
     the first generation garbage-collected;
   * the engine's filtered kNN is compared with exact cosine search over
     the same visible vectors, unfiltered, restricted to a quarter of the
     corpus and to one file (2.5%);
   * the request is the one a default semantic search makes: 60 hits
     (`search.semantic_top_k` 30, doubled per record), of which the top
     10 are compared;
   * the test fails below a mean recall@10 of 0.95. The recall of a bare
     `k = 10` request is reported next to it, not gated.
5. **OpenSearch kNN asks for the candidate pool.** OpenSearch's Lucene
   kNN searches with a beam of `k`, so `k = 10` returned a poor top-10.
   The query now asks for `k = candidates` (capped at the read limit)
   and returns the top `size`, like Elasticsearch's `num_candidates`.
6. **Task waits survive a server timeout.** OpenSearch answers a task
   wait that outlasts its timeout with a 500 `timeout_exception`; that
   reply (and a 408) means "still running", so the wait keeps polling
   instead of failing the publication.
7. **Semantic search runs on the bounded pool.** The sources of one
   request are searched by at most `search.max_concurrency` workers
   (`pool::map_corpora`, already used by lexical search), inside the
   request deadline, and merged in source order, so rankings never
   depend on scheduling. SQLite sources still run on the calling thread.
8. **Diagnostics name how each source answered.** Search (`semantic`
   timing, `engines`) and evidence (`diagnostics.semantic_engines`)
   report per source `hnsw` or `exact` (local index), `server` (engine
   kNN), `no_vectors` or `unavailable`. A missing local model is the
   outcome's reason, and a widening cut off at its cap is listed in
   `degraded`.

## Measurement

Live filtered ANN recall (`benchmarks/server-ann-recall-20000.json`):
20,000 vectors of 384 dimensions, two published generations with the
first garbage-collected, 40 queries, recall@10 of the default request
against exact cosine search over the same visible vectors:

| | OpenSearch 2.17 | Elasticsearch 8.15 |
|---|---|---|
| Unfiltered recall@10 / p95 | 0.977 / 28.9 ms | 1.0 / 26.3 ms |
| One directory (25%) recall@10 / p95 | 1.0 / 22.2 ms | 1.0 / 22.3 ms |
| One file (2.5%) recall@10 / p95 | 1.0 / 14.2 ms | 1.0 / 16.2 ms |
| Unfiltered, bare `k = 10` request | 0.675 | 0.93 |
| Publishing both generations | 46.9 s | 42.0 s |

The bare `k = 10` recall depends on how the engine laid out its
segments and changes from run to run: an earlier run of the same corpus
gave 0.955 (OpenSearch) and 0.953 (Elasticsearch), and the CI runner's
3,000-vector Elasticsearch run gave 0.92. That is why the gate measures
the request semantic search actually makes, whose wider candidate pool
(600) held at 0.977 or above in every run. Before decision 5, the bare
OpenSearch recall@10 was 0.448.

Citation hydration (`crates/docmonk-service/tests/server_hydration.rs`):
for the same entity citations, reading each one's canonical content to
fingerprint it and again to verify it took 41 requests; the batched path
takes 2, on both engines.

Paging (`crates/docmonk-ml/tests/finalize.rs`): a pass in pages holds at
most one page (`max_window`) and computes the same vectors as one window
over the whole build.

Still to be measured: the held-out evaluation on SQLite and both engines
with this phase's changes, and peak memory of a paged against a
one-window embedding pass on a large corpus.

## Compatibility

No schema, index or derivation change, so no rebuild or reindex. The
new diagnostics fields (`diagnostics.backend`,
`diagnostics.semantic_engines` and the search timing's `engines`) are
additive.
