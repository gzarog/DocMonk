# ADR 0016: Lexical and hybrid search

Status: accepted

## Scope

The `docmonk-retrieval` crate, plus storage projections in
`docmonk_storage::search`. Symbol and graph queries are ADR 0017;
explore, impact and attachment provenance are ADR 0018.

### Lexical search (`lexical`)

- **Rank tiers:** exact symbol > qualified symbol > alias > title/heading
  > FTS > path.
- **Query-structure tiers** (`LexicalTier`): phrase > all terms > prefix
  > OR fallback. The plan runs most precise first and stops once `limit`
  distinct rows were seen; repeated expressions are skipped.
- **Tie-breaks:** FTS ordinal, entity kind, recency, source, path, id.
- **`(kind, id)` merge** keeps each result's best-ranked hit.
- **Matching:**
  - Word tokens are runs of Unicode letters, digits, marks and `_`.
  - Alias lookups match a dotted query against the last two segments of
    qualified names with at least three segments, without needing a
    schema column.
  - Chunk FTS uses BM25 column weights (heading 5, body 1,
    title 8) and match-centred `snippet()`.
  - Path search ANDs tokens, falling back to a substring scan.

### Hybrid search (`hybrid`)

- **Merge.** Lexical and semantic candidates are deduplicated per
  `(kind, id)`, keeping both ranks. Each input is capped at its fusion
  budget (50/50, pool 100).
- **Pinned tier.** Exact, qualified, alias and title/heading matches always
  rank first and are never fused.
- **Hybrid tier.** Everything else is ordered by RRF (k = 60).
- **Neural pass (optional).** The cross-encoder from ADR 0015 rescores the
  top `search.reranker.top_n` hits.
- **Degradation.** Without the embedding model, fusion is lexical-only and
  `SemanticOutcome` reports why. Without the reranker, the RRF order is
  kept. Neither case is an error.
- **Multiple sources.** Multi-source search takes a list of `Corpus`
  (store + build).

### Indexing rules

- A heading chunk is indexed under its own text, so heading queries reach
  the title/heading tier.
- A code entity without a signature is indexed under its name, so modules
  are not skewed in BM25 by an empty field.
- A change to what is indexed bumps `CODE_DERIVATION_VERSION` (currently
  `rust-code-2`) or `CHUNKER_VERSION`, which forces a rebuild.

## Evidence

`fixtures/expected/search.json` covers 105 queries:
- the 72 search-quality queries over `fixtures/search_quality/project`;
- 33 edge cases over the linking corpus: aliases, headings, case-folded
  titles, path fragments, mid-word fragments, punctuation, FTS operators,
  non-ASCII and token-less queries.

Lexical (top 20), hybrid RRF (top 20) and hybrid + cross-encoder results
are all checked.

### How results are compared

- **Pinned mtimes.** The test indexes with every file mtime pinned, so
  recency tie-breaks are reproducible.
- **Tie keys.** Each expected hit carries its tie key (the sort key
  without ids). Only hits with equal tie keys may appear in either order.
- **Deterministic FTS ties.** Rows with equal BM25 would otherwise come
  back in insertion order, which follows worker scheduling and differs
  between machines. Such ties are broken by path, then qualified name and
  line (code) or chunk order (documents).
- **Path hits** are titled with their source-relative path (ADR 0006).

## Benchmark

72 queries × 5 rounds over the search-quality corpus: lexical p50
0.75 ms, p95 1.28 ms.

## Not included

- An in-process result cache. It would only pay off in a long-lived
  server process.
