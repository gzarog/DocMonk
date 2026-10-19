# ADR 0036: Typed anchors, exact occurrences and bounded retrieval

<!-- clean-slate-audit: allow-start (an external plan id and a user's API URL with a version segment) -->
Status: accepted and implemented in code (plan DOCMONK-RETRIEVAL-PERFORMANCE-V1,
phases P00–P08, in one change). This change measures planner timings only.
Live OpenSearch/Elasticsearch parity runs and real 150-repository runs are
operator steps (see "Measurement"). Until they are recorded, the
end-to-end latency targets of the plan stay hypotheses.

## Context

A question such as

> Is the URL path /sb/v1/content/groups/image used, referenced, or related
> to any code or documentation in the indexed DocMonk project? Trace
> matching route/client/API references and relevant call paths.

was slow and answered poorly:

<!-- clean-slate-audit: allow-end -->

* **F01** `lexical::query_plan_with` built all `C(n, ceil(n/2))`
  minimum-should-match groups before it checked `MAX_MSM_GROUPS`. With 25
  content terms that is 5,200,300 vectors per lexical stage and corpus.
* **F02** `explore` used its own planner, in which the word
  "documentation" outranked everything, so the question became a document
  query.
* **F03/F04** Nothing pulled the path out of the sentence. Explore sent
  the whole question to full-text search, and a tokenizer splits a path
  into words, which proves nothing about the path.
* **F05/F06** Server mode prefetched every lexical tier for every source
  and started one thread per source. Explore read graph freshness for
  every source and was benchmarked as one opaque block.
* **F07** The planned `graph_references` strategy never ran.
* **F08** ASP.NET controller and action route attributes were recorded
  separately and never composed.

## Decision

1. **Bounded planner (P01).** A capped, overflow-safe binomial is computed
   before any group is enumerated. Planning is also capped at 64 content
   terms (identifiers kept first), 16 OR terms and 4 KiB per expression.
   Plans for short queries are unchanged. No reindex is needed.
2. **One router (P02).** `route::route` extracts typed anchors anywhere
   in a query (`QueryAnchors`: URL paths, file paths, symbols, HTTP
   methods, quoted literals) before applying intent rules. A URL path
   produces the `endpoint_reference` intent. Explore, ask and MCP use
   `explore::plan_routed`, built on the same router as `evidence` and
   `search --explain`. `explore::plan` remains for compatibility, and its
   plans equal the routed plans of queries without anchors. URL paths and
   prose are told apart deterministically (`docmonk_core::urls`): a path
   must start with `/` or `~/`, be an http(s) URL, carry a `{param}`, or
   start with `api`/`v<N>`. "route/client/API" is prose.
3. **Exact occurrences (P03).** Graph derivation `graph-3` records:
   * every URL string literal in code, with its owning entity, as a
     `references` edge to `url_literal:<key>`. This includes client
     attributes such as Refit's `[Get("/x")]`; server route attributes are
     recorded as composed routes instead. Interpolated literals
     (`$"..."`, f-strings, template strings) get a `{}` placeholder and are
     only ever heuristic;
   * every statically composed framework route as `route:<key>`: ASP.NET
     class `[Route]` × action `[Http*]`/`[Route]`, with `[controller]` and
     `[action]` replaced and absolute action templates honoured.
     Flask/FastAPI route decorators compose with no prefix.

   `key` is `url_match_key`: path only, `{}`/`<>` parameters written `{}`,
   ASCII lower-cased, segment boundaries kept. `/x/image2` and
   `/x/image/thumbnail` never match `/x/image`. The rows live in the
   existing relationships table and index, so there is **no SQLite DDL
   change and no server mapping change**. The lookup is the existing
   indexed `target_symbol` equality read (`unresolved_edges_many`),
   scoped to the request's build and graph generation on all three
   backends. Documents are checked by verification: full-text search
   proposes candidate chunks, and only chunks whose stored text contains
   a token with the anchor's key are kept.
4. **Capability negotiation.** `KnowledgeRead::graph_derivation` reports
   each source's derivation. Sources whose derivation is older than
   `graph-3`, or whose graph is not visible, are listed in
   `exact_capabilities` / `exact_unchecked_sources`, and
   `exact_complete: false` is set. An empty result from such a source
   never means "absent".
5. **Staged lexical search (P04).** For endpoint questions, lexical search
   runs on the anchor, not the sentence. When verified exact evidence
   exists, the loose OR tier is skipped. Server readers prefetch only the
   precise tiers. Loose tiers are read on demand, and `_msearch` batches
   are capped at 32 items.
6. **Selective graph (P05).** Once exact seeds exist, lexical and graph
   stages read only the sources holding them. Traversal starts from the
   owning entities (at most `search.selective_graph.max_seed_entities`)
   with the existing depth and edge caps, and reports
   `traversal_truncated`. The planned `graph_references` strategy now runs.
7. **Bounded concurrency (P06).** `pool::map_corpora` replaces one thread
   per source with at most `search.max_concurrency` workers, and returns
   results in corpus order so rankings do not depend on scheduling. Graph
   freshness for server sources is read through the same pool. The
   transport counts bytes received as well as requests and bytes sent.
8. **Lazy semantic search and cache (P07).** Explore's semantic stage is a
   callback that runs after the exact, lexical and graph stages. An
   endpoint question with verified evidence never embeds the query.
   `search.lazy_semantic` skips the stage after a confident lexical
   answer. Long-lived processes (MCP, UI) cache explore payloads. The
   cache key covers the query, the budget, every source's root and build,
   each graph's state and generation, the search, context and graph
   settings, and the binary version. Entries expire by TTL and LRU, are
   cleared together with pooled stores, and are never used for `--explain`.
9. **Diagnostics (P00).** `docmonk explore --explain` (MCP `explain:
   true`) adds a `diagnostics` object: the route, per-stage timings, source
   counts considered, searched and expanded, exact reads and rejections,
   graph seeds and edges, the semantic decision, backend request and byte
   counters, build ids, and, for the CLI, process costs. Without the flag
   the output is unchanged.

Each evidence item carries one of the evidence classes
`direct_exact_literal`, `statically_composed_route`,
`resolved_code_relationship`, `document_reference`, `heuristic_match` or
`unresolved_or_not_indexed`. A composed route is a static reading of
attributes. It is never presented as proof of runtime routing.

## Rollout and rollback

| Flag | Default | Off means |
|---|---|---|
| `search.anchors.enabled` | `true` | Route as before anchors existed |
| `search.exact_occurrences.enabled` (`.max_results` 50) | `true` | No exact stage; endpoint questions use focused lexical search and graph only |
| `search.adaptive_lexical.enabled` | `true` | Prefetch every tier; never skip the OR tier |
| `search.selective_graph.enabled` (`.max_seed_entities` 25) | `true` | Later stages read every source |
| `search.cache.enabled` (`.max_entries` 64, `.ttl_seconds` 300) | `true` | No explore cache |
| `search.max_concurrency` | `8` | (cap on server workers) |

The P01 planner bound is always on. Bumping the graph derivation to
`graph-3` recomputes relationship graphs on the next index pass. Content
is not reindexed, and the old graph keeps being served (flagged stale)
until the new one is promoted. Rolling back the binary is safe: older
binaries ignore the extra `references` rows.

## Measurement

Measured in this change (development container, debug build, not
reference hardware): the planner benchmark (`docmonk-search-bench
--planner`) and the unit tests that bound planner time for the
25-term question and a 150-term stress input.

Operator steps not run here:

* `docmonk-search-bench --modes explore,evidence --conditions warm,cold
  --planner --server` on the synthetic corpus at 1, 10 and 150 sources,
  before and after, on fixed hardware (record CPU, RAM, disk, OS and index
  size).
* The same query suite against live OpenSearch and Elasticsearch
  (`crates/docmonk-service/tests/server_live.rs`, opt-in), comparing
  evidence identity (source, path, line, matched literal, generation)
  with SQLite.
* An opt-in real-workload run (150 .NET repositories). Proprietary paths
  are never committed.
