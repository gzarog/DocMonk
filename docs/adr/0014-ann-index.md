# ADR 0014: Persistent ANN index and semantic search

Status: accepted

## Decisions

1. **Engine.** A pure-Rust HNSW index with exact search as the fallback,
   with no C++ dependency.
2. **Fusion.** Reciprocal Rank Fusion is a tested primitive in
   `docmonk-ml`; hybrid search (ADR 0016) uses it.

## Index (`docmonk_ml::hnsw`)

- Standard HNSW over L2-normalized vectors, scored by inner product.
- Parameters: `m` = 16 (32 on layer 0), `ef_construction` = 128,
  `ef_search` = 96.
- Neighbours are chosen with the diversity heuristic.
- Levels are drawn from a seeded generator, so the same inserts give the
  same graph (a test asserts this).
- **Deletes are tombstones.** Tombstoned nodes still route searches but are
  never returned, and the search beam widens by the tombstone count. The
  index compacts (rebuilds without tombstones) once their share exceeds
  25%.
- No `unsafe`: the dot product uses 8 accumulators so the compiler can
  vectorize it.

## Persistence (`docmonk_ml::ann`)

- **Location.** `<project>/ann/index.hnsw` holds one index per project.
- **File format (version 1).** A magic number, then a header:
  - format version, model fingerprint, build id and dimensions;
  - a content digest: sha256 over the build's ordered
    `(subject_type, subject_id, text_hash)` set;
  - then labels, tombstones, vectors and adjacency lists;
  - then a trailing sha256 over everything before it.
- **Atomic writes.** Temp file, fsync, rename.
- **SQLite is authoritative; the file is a cache:**
  - A missing, truncated, corrupt or other-version file, or one for another
    model or dimension count, is discarded and rebuilt from SQLite.
  - Queries use the file only when its build id, fingerprint and content
    digest all match the database. Otherwise they run exact search over
    SQLite. This covers a crash between writing vectors and syncing the
    index, which incremental builds (same build id) would otherwise hide.
- **Sync** runs in the `EmbeddingFinalizer` after embedding, and on warm
  passes:
  - it diffs the index labels against SQLite, then adds and tombstones;
  - it is a no-op, with no write, when the digest already matches;
  - a sync failure only logs, and search stays exact.

## Semantic search (`docmonk_ml::semantic`)

- Embeds the query, takes the candidates (`candidate_k` ≥ `limit`) and
  attaches display metadata:
  - entities: qualified name, signature and line;
  - chunks: title > heading, a snippet, the ordinal and the attachment
    index.
- Ordering: `(-score, path, id)`.
- It never fails because semantic search cannot run.
  A missing model, an empty query or a build without vectors is reported
  in `reason`.

## Evidence

- `fixtures/expected/semantic.json` holds exact cosine rankings for 8
  queries over the linking corpus (32 subjects), including a Greek query.
  HNSW and exact search both reproduce the top-10 order: scores within
  1e-3, and swaps allowed only between near-ties under 1e-4.
- The ANN tests cover:
  - round trip, corruption, truncation and version mismatch;
  - atomic save;
  - a truncated file falling back to exact search, then being rebuilt on the
    next warm pass;
  - stale content (rows changed behind the index) falling back to exact
    search, then resyncing;
  - deleted files leaving the index;
  - a model change rebuilding the index;
  - recall of at least 0.95 on random data, and with tombstones before and
    after compaction;
  - determinism.

## Benchmark

`benchmarks/ann-20000.json`: 20,000 clustered 384-dimension unit vectors
and 200 queries. Build about 11 s single-threaded, query about 0.6 ms,
recall@10 about 0.73, exact query about 7.7 ms.

- Build cost is about 0.5 ms per vector, under 2% of the time to embed the
  same text.
- Sync is incremental, so steady-state cost tracks the change.
- Load takes 0.24 s for a 33 MB file.

## Limitations

- Index build is single-threaded.
- The digest check reads the build's key set once per query, which is
  O(n) over short rows.
