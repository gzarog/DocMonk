# ADR 0038: One ranking path and canonical citation verification

Status: accepted and implemented (local-model retrieval plan, phase 2).

## Context

`search --hybrid`, `evidence`, `explore` and `ask` each assembled their
ranking differently:

* `search --hybrid` cut the lexical list down to the response limit
  *before* fusion, so RRF and the cross-encoder only ever saw the first
  10 lexical hits. `evidence` fetched its own pool and ordered unpinned
  hits by query-term coverage; `explore` ranked lexical hits on their own
  and then re-sorted its evidence by confidence and *path*, so the best
  hit of an unnamed-symbol question could land anywhere in the list.
* The cross-encoder scored each hit's snippet. A missing model, a
  disabled reranker and "nothing to reorder" all looked like the same
  unchanged ranking.
* Context was expanded only around document chunks. A code hit, or a hit
  found only by semantic search, had no context at all.
* `verify` checked that a cited record still existed. A citation at
  lines outside its record, or of a record whose stored content had
  changed since it was cited, still counted as support.

The held-out evaluation (`benchmarks/heldout-eval/phase-1.json`) showed
the effect: on the same snapshot and question, `explore` reached
recall@10 0.797 and `line_provenance@1` 0.407, while `evidence` reached
0.854 and 0.926.

## Decision

1. **One fusion** (`docmonk_retrieval::pipeline::fuse`). Every surface
   that ranks evidence calls it with its own lexical and semantic
   candidates, already inside the request's hard filters:
   1. each stage fetches `candidate_budget(search.candidate_pool,
      limit)` candidates. The default `0` means `max(4 * limit, 40)`,
      never below the limit and never above the fusion cap of 100. The
      budget no longer depends on how many hits the response shows;
   2. merge per `(kind, id)`, exact matches pinned, RRF for the rest
      (`hybrid::merge` / `hybrid::rerank`, unchanged);
   3. with `search.reranker.enabled`, the cross-encoder rescores the
      hybrid tier on each hit's **stored** text: a chunk's text under its
      title and headings, or the body passages at a code hit's lines under
      their owner. It falls back to the snippet only when no stored text
      exists. Pinned hits never move;
   4. the final limit;
   5. for fact-seeking routes (`document_fact`, `conceptual`,
      `ambiguous`), the final unpinned hits are ordered by how many of the
      question's content terms they contain, with the fused rank
      breaking ties. This was `evidence`'s rule and is now the shared
      one. Per-source ranks are not comparable across sources. Only hits
      that survived the cut are reordered. Ordering the wider pool let
      lower-ranked hits that repeat the question's words displace
      better-located ones (top-hit line provenance fell from 0.889 to
      0.852 for `search --hybrid` on SQLite).

   `evidence` fuses each subquery this way without step 5, then fuses
   the subqueries (RRF), deduplicates, collapses sibling hits (at most
   three per file), and only then orders by term coverage. Ordering by
   coverage before the collapse changed which hits of a file it kept,
   and top-hit line provenance fell from 0.926 to 0.889. `explore` uses the fused
   ranking as its evidence when the question names no symbol and has no
   exact anchor. A named symbol's definition is its own evidence when the
   graph has no edges to answer with. Equal-confidence evidence keeps the
   fused order (`build_context` no longer re-sorts by path).
2. **The reranker's state is reported** as `RerankerStatus`: `applied`,
   `disabled`, `unavailable` (no local model), `failed` (no scores) or
   `nothing_to_rescore` (fewer than two unpinned hits). It also reports
   `top_n`, the number of hits scored and the time taken. It appears
   under `diagnostics.reranker` (`evidence`, `explore --explain`) and
   `explain.reranker` (`search`). An enabled reranker that did not apply
   is also listed under `diagnostics.degraded`.
3. **Context for every kind** (`pipeline::expand_context`): a chunk gets
   its heading and sibling chunks as before. A code hit gets its matched
   passage, its owner and its neighbouring passages of the same owner
   (`search.context.previous_chunks` / `next_chunks`), within the same
   token budget (about 4 characters per token). Semantic-only hits are
   included.
4. **Canonical citations.** Each citation records a `content_fingerprint`
   of its record's stored content (`grounding::canonical_content`):
   * a chunk's text;
   * a passage's lines;
   * for a code entity, the body passages overlapping the cited lines,
     else its qualified name and signature;
   * a file's path.

   `verify` reloads the record from the request's snapshot and rejects a
   citation with one of these reasons:
   * `unknown_source`: the source is not in the request;
   * `stale_build`: another build or generation;
   * `filtered`: outside the hard filters;
   * `missing_record`;
   * `wrong_location`: the cited lines or pages lie outside the record;
     an entity's span includes the comments its body passages start at;
   * `content_mismatch`: the stored content changed since it was cited.

   Each evidence item also has a `retrieval` object (pinned, lexical and
   semantic ranks, RRF and neural scores). The payload has a top-level
   `provenance` summary: citations checked, valid, invalid, and invalid
   by reason. Only verified citations count as support.

## Compatibility

* No index, schema or derivation change. Nothing needs to be rebuilt or
  reindexed, and both backends read through `KnowledgeRead`, so SQLite,
  OpenSearch and Elasticsearch share every line of this change.
* Output changes are additive: `content_fingerprint`, `retrieval`,
  `provenance`, `diagnostics.reranker`, `explain.reranker` and `context`
  on `search --hybrid` entries. The MCP `docmonk_evidence` schema lists
  `provenance`. Rankings change where the old paths disagreed. The
  snapshots `fixtures/expected/{explore,ai_cli,mcp,ui}.json` were
  re-recorded. The Admin UI's config page shows the new
  `search.candidate_pool` setting.
* Rollback: nothing persisted changes except the configuration. Older
  binaries reject unknown settings, and this version writes
  `search.candidate_pool` into `config.yaml` whenever it saves the file
  (`config set`, the Admin UI). Delete that line before running an older
  binary.

## Measurement

`crates/docmonk-service/tests/heldout_eval.rs` measured this change on
SQLite, OpenSearch 2.17 and Elasticsearch 8.15
(`benchmarks/heldout-eval/phase-2.json` against `phase-1.json`, release
build, pinned MiniLM models). SQLite unless noted. The servers gave the
same numbers except where shown:

| Metric | Phase 1 | Phase 2 |
|---|---|---|
| `explore` recall@10 | 0.797 | 0.866 |
| `explore` top hit covers the labelled line (`line_provenance@1`) | 0.407 (servers 0.333) | 0.852 (servers 0.889) |
| `explore` `code_symbol` recall@10 | 0.167 | 1.0 |
| `search --hybrid` recall@1 | 0.754 (servers 0.765) | 0.799 |
| `search --hybrid` MRR | 0.814 (servers 0.820) | 0.835 |
| `search --hybrid` with the reranker, `line_provenance@1` | 0.852 | 0.889 |
| `search --hybrid` with the reranker, MRR | 0.826 | 0.823 |
| `evidence` (every metric, every backend) | unchanged | unchanged |
| Citation validity, false support / abstention | 1.0, 0.083 / 0.193 | 1.0, 0.083 / 0.193 |

Latency (p95) did not change measurably without the reranker. With the
reranker it rose from 0.59 s to 1.90 s (servers 0.65 s to about 1.85 s).
The cross-encoder now reads up to 256 words of stored text per hit
instead of a short snippet. For that cost it improved the top hit's line
provenance and left MRR unchanged. `search.reranker.enabled` stays off by
default.

The 72 golden queries (`fixtures/search_quality/gates.json`), the P0
evaluation (`fixtures/p0/eval-gates.json`) and the repository
evaluation keep their floors. The reproducibility gate now prints the
rankings that differ. On a server, the harness merges the test indexes'
segments before measuring (see `docs/evaluation.md`).
