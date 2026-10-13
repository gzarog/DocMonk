# ADR 0017: Symbol search and graph queries

Status: accepted

## Scope

The `docmonk_retrieval::graph` module answers symbol and graph queries.
Storage support lives in `docmonk_storage::search`: `symbol_entities`,
`entity_edges`, `unresolved_edges` and `file_rel_path`, all scoped to one
build.

| Function | Behavior |
|---|---|
| `find_symbol_matches` | exact name or qualified name, across corpora, ordered by `(qualified name, source, path, line, id)` |
| `traverse` | BFS with `max_depth` and `limit`, per-frontier edge sort `(type, target, id)`, visited set |
| `unresolved_symbol_edges` | name-only edges for a symbol |
| `traverse_symbol` | `callers` / `callees` |
| `references` | CALLS / IMPORTS / REFERENCES, both directions |
| `resolved_incoming` / `resolved_outgoing` | the neighbour entity and its source-relative path |
| `is_test_file` / `find_tests_referencing` | test-file detection and tests touching a symbol |

Rules:

- **Unresolved edges on incoming walks.** Incoming walks also merge the
  unresolved (name-only) edges recorded under the query and under each
  match's bare name, once per (corpus, name).
- **No match at all.** When nothing matches, incoming falls back to
  unresolved edges in every corpus.
- **Limits.** Depth 1 and limit 100 are the defaults, and a limit stops the
  BFS mid-frontier.
- **Test files.** `is_test_file` recognizes seven naming patterns (a test
  checks 18 paths).
- **Stable order.** Same-named overloads are ordered by path and line, so
  results are deterministic.

## Evidence

`fixtures/expected/graph.json` covers three corpora:

- `fixtures/graph`: call chains, a cycle, unresolved callees, and test files in
  three indexed languages (`.py`, Go and TypeScript);
- `fixtures/code`;
- `fixtures/linking`.

Each corpus is queried with every entity name, every qualified name,
unresolved-only names and a missing name: 298 queries. Each query runs at
depths 1 and 3 (596 checks) and records symbol matches, callers and
callees, references, resolved incoming and outgoing edges, and tests.

Everything is compared in id-free form: entity = path, qualified name,
kind, line; edges include the resolver and confidence. Within one depth,
edges are compared as a multiset; the depth sequence must match. The
stored graph is resolved against the whole build (ADR 0009), so it does
not depend on scan order.

These queries are exposed as `docmonk symbol`, `callers`, `callees` and
`references`, and through the MCP tools.
