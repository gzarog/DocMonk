# ADR 0009: Code intelligence and transactional builds

Status: accepted

## Code intelligence

- `docmonk-code` holds the parser wiring, query-driven extractor, resolver,
  framework rules, code processor and graph traversal. Extraction is driven
  by one tree-sitter query file per language (`crates/docmonk-code/queries/*.scm`).
- Grammars: `tree-sitter` 0.25 with the official grammar crates (C#, Go,
  Java, JavaScript, Python, Rust, TypeScript/TSX). The grammars are C code
  compiled by `cc`; the project itself still has no `unsafe`.
- Evidence: `fixtures/expected/code.json` pins extraction over
  `fixtures/code`. That corpus covers 8 grammars, overloads,
  decorators/attributes, routes, impl/receiver methods, cross-file
  references, a syntax error and an unsupported extension. Entities (kind,
  name, qualified name, parent, signature, full range) and relationships
  (type, endpoints, symbol, resolver, confidence, location, evidence) must
  match exactly, both for the pure pipeline and end to end through the
  indexer.
- Identity: entity IDs are `record::entity_id(file_id, kind, qualified_name,
  ordinal)`. Relationship IDs are `record::relationship_id(source, type,
  reference text or target, ordinal)`. Both are stable across rebuilds
  (tested by wiping all state and rebuilding).
- Parse failures: a tree with syntax errors fails only that file,
  permanently, with `parse_error`.
  Unsupported code extensions are indexed with no entities.
- Source locations are `<relative path>:<line>`, not absolute paths, so the
  knowledge store is portable.

## Whole-build resolution

Resolving cross-file references while files are processed would make the
outcome depend on processing order, and it would never be revisited when a
target appears or disappears later. Instead:

1. Workers resolve only same-file references.
2. Other references are stored as `pending`, with their raw
   `reference_text`.
3. A `BuildFinalizer` (`CrossFileResolver`) re-resolves every cross-file
   reference against the complete build before it becomes visible. This
   includes unchanged files, so a newly added definition resolves old
   callers, and a deleted one leaves no dangling edges.

## Transactional builds

- **One transaction per build.** Each build runs inside one SQLite
  transaction, a "session".
  - A full rebuild writes a new build next to the visible one, commits it
    while it is still invisible (the control plane points at the old build),
    then publishes and garbage-collects.
  - An incremental pass updates the active build in place. Readers keep the
    last committed state until COMMIT; any failure or crash rolls the whole
    pass back.
  - Nested writes use savepoints. Leftover `building` builds from a crash
    between commit and publish are garbage-collected.
  - Each page is written about once instead of once per batch, and a
    single edit never copies the whole build.
- **FTS rows share their base row's rowid**, so per-file and per-build
  deletes are rowid lookups instead of full FTS scans (which would make
  indexing quadratic).
- The writer batches up to 64 already-prepared files per write, and hot
  statements use the prepared-statement cache.

## Benchmarks

`benchmarks/code-csharp-{300,3000}.json` record indexing runs over 300 and
3,001 C# files. On the 3,001-file corpus a cold index takes about 4–5 s, a
single edit about 0.35 s and a warm pass with no change 0.06 s.

Most of an incremental pass is whole-build resolution: about 0.25 s
over 82k relationships. Narrowing it to the affected symbols is a possible
later optimization.
