# ADR 0037: Implementation passages and the code text fallback

Status: accepted and implemented (local-model retrieval plan, phase 1).

## Context

Code search matched entity names, qualified names and signatures, and an
entity's embedding was its signature (else its qualified name). A question
about what a function *does* ("which header prevents a retried refund
from refunding twice") could only reach the function if its name or
signature happened to share a word with the question. The held-out
evaluation (`benchmarks/heldout-eval/phase-0.json`) put this at 56% for
"the top hit covers the labelled implementation line".

Files without a grammar (`.rb`, `.sql`, shell, ...) were indexed with no
searchable content at all. A file with syntax errors failed outright,
even though its text was perfectly searchable. Phase 0 measured
`code_fallback` recall@10 at 0.0 on every backend.

## Decision

1. **A separate projection.** Each code file gets *implementation
   passages* (`code_passages` / `passage_fts` locally, the `passages`
   index on the server). A passage stores exact source lines (line endings
   normalized), its line span, its owner and how it was extracted. Passages
   never add an entity or a relationship, so exact symbol lookups, graph
   traversal and graph generations are unchanged.
2. **Body passages** (`kind = body`) cover one function or method: the
   documentation comments and attributes directly above it, then its
   signature and body. A body longer than 40 lines or 2,400 characters is
   split at its top-level statement boundaries, which tree-sitter records
   during extraction (`Extraction::bodies`). A statement that is still too
   long is cut into 40-line windows that overlap by 5 lines. Every part
   carries its owner's qualified name and signature as its `header`, so a
   part deep inside a long function still knows what it belongs to.
3. **Text passages** (`kind = text`) are 40-line windows that overlap by
   5 lines, over files without symbol extraction:
   `extraction = unsupported_language` for extensions without a grammar,
   or `parse_failed` for source with syntax errors. They claim no symbol.
4. **Syntax errors degrade, they do not fail.** A file with syntax errors
   is recorded `indexed`. Its `last_error` says it was indexed as plain
   text without symbols, the error feed logs `parse_degraded`, and the
   run's `degraded` counter counts it. The graph stage gives it no
   relationships.
5. **Bounds.** At most 256 passages per file. A passage has at most 40
   lines and 2,400 characters; a single longer line is kept as its
   prefix. `count("code_passages")` and the server `passages` index make
   both observable.
6. **Retrieval.** Lexical search has a `passages` stage, prefetched in the
   same `_msearch` as the other tiers on a server. A body-passage hit is
   reported as an `entity` hit on its owner, located at the passage's
   lines, with a snippet of the matched text. It merges with that entity's
   name hits by `(kind, id)`, and among equally ranked hits it sorts after
   name matches (`BODY_KIND_RANK`). A text-passage hit is a new `passage`
   kind titled by its path, so it is never mistaken for parsed symbol
   data. Each passage is also embedded (`subject_type = passage`, text
   assembly `passage-1`: header, newline, lines) with the current local
   MiniLM model. A semantic hit on a body passage names its owner and the
   passage's lines. Semantic hits are deduplicated per record, keeping the
   best-scoring vector. Hard filters treat body passages as `entity` and
   text passages as `passage`.

## Rebuild and rollback

* `CODE_DERIVATION_VERSION` is `rust-code-3`: every source's next run is
  a full rebuild (the local schema fingerprint changes too, so an existing
  knowledge database is refused with the usual reindex instruction). The
  graph derivation includes the code derivation, so graphs are rederived
  once.
* The server `SCHEMA_VERSION` is 6 and adds the `passages` index. Indexes
  of schema 5 are refused, never mutated. Use a fresh `index_prefix` (or
  reset the DocMonk indexes) and reindex.
* Vectors: passages are new subjects, so existing entity and chunk vectors
  are reused from the embedding cache.
* Rollback: an older binary refuses the new local and server schemas, so
  rolling back means reindexing with that binary (or keeping its old
  index prefix until the rollback window ends).

## Measurement

`crates/docmonk-service/tests/heldout_eval.rs` measured this change on
SQLite, OpenSearch 2.17 and Elasticsearch 8.15 (`benchmarks/heldout-eval/phase-1.json`
against `phase-0.json`, release build, pinned MiniLM models). SQLite, hybrid
arm unless noted; both engines gave the same numbers:

| Metric | Phase 0 | Phase 1 |
|---|---|---|
| Top hit covers the labelled implementation line (`line_provenance@1`) | 0.556 | 0.889 |
| A top-10 hit covers it (`line_provenance@10`) | 0.741 | 0.963 |
| `code_fallback` recall@10 (every arm and backend) | 0.0 | 1.0 |
| `code_implementation` recall@10, lexical arm | 0.563 | 0.938 |
| All queries recall@10 | 0.780 | 0.854 |
| `evidence` false abstention on answerable queries | 0.352 | 0.193 |
| Clean build / one-file update (SQLite) | 3.1 s / 675 ms | 5.1 s / 818 ms |

Query latency did not change measurably. The build cost is the added
passage vectors and FTS rows (33 passages for 60 entities and 51 chunks).
The 72 golden queries keep their floors (`fixtures/search_quality/gates.json`):
`lexical/cross_document ndcg@10` moved 0.94 → 0.928, inside its 0.91
floor. The ranking snapshots in `fixtures/expected/{search,explore,semantic}.json`
were re-recorded for the new passage stage and passage vectors.
