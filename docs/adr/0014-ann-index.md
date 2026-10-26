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
and 200 queries. Build about 8 s single-threaded, exact query about
5 ms. The earlier figure (recall@10 0.73 at 0.6 ms) was measured at
`ef_search` 96; the default is 400.

`benchmarks/ann-sweep.json` measures `ef_search` 64, 96, 200, 400 and 800
over the same index at 20,000 and 100,000 vectors (release, 4 threads):

| Vectors | ef_search | recall@10 | p50 ms | p95 ms |
|---|---|---|---|---|
| 20,000 | 64 | 0.628 | 0.32 | 0.52 |
| 20,000 | 96 | 0.731 | 0.39 | 0.64 |
| 20,000 | 200 | 0.879 | 0.68 | 1.00 |
| 20,000 | **400** | **0.961** | 1.33 | 1.96 |
| 20,000 | 800 | 0.994 | 2.35 | 3.13 |
| 100,000 | 64 | 0.262 | 0.86 | 1.47 |
| 100,000 | 96 | 0.321 | 0.97 | 1.81 |
| 100,000 | 200 | 0.434 | 1.62 | 3.21 |
| 100,000 | 400 | 0.604 | 2.53 | 4.53 |
| 100,000 | 800 | 0.809 | 4.10 | 6.56 |

The default `search.ann.ef_search: 400` stays: at 20,000 vectors it is
the lowest-latency setting with recall@10 >= 0.95. At 100,000 vectors no
measured setting reaches 0.95 on this data, and 800 doubles the latency
for 0.81, so changing the default would not meet the target either.
Closing that gap needs a build-side change (a larger `m` or
`ef_construction`, or exact re-scoring of a wider beam), and real
embeddings, which cluster less tightly than this synthetic set, need
measuring first. It stays open. Exact search over 100,000 vectors costs
about 50 ms per query.

- Build cost is about 0.5 ms per vector, under 2% of the time to embed the
  same text.
- Sync is incremental, so steady-state cost tracks the change.
- Load takes 0.24 s for a 33 MB file.

## Limitations

- Index build is single-threaded.
- The digest check reads the build's key set once per query, which is
  O(n) over short rows.
