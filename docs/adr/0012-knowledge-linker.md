# ADR 0012: Cross-domain knowledge linker and manual links

Status: accepted

## Context

The linker connects code entities to document chunks. Manual links are
persisted.

## Decisions

### Linker

- **Crate.** `docmonk-knowledge` provides the `KnowledgeLinker`, which
  implements `BuildFinalizer`. It is registered after the code finalizers in
  `docmonk_convert::registry`.
- **Finalizer interface.** `BuildFinalizer::finalize` receives the ids of
  the files written in the build. This allows incremental (touched-only)
  linking.
- **Matchers.** Five matchers: exact identifier, qualified name, alias,
  filename and route heuristic. Each one sets its own resolver and
  confidence values.
- **Matching index.** An inverted word index over document units narrows the
  candidates before the boundary check.
- **Boundary rule.** The rule is `(?<![\w.])needle(?![\w.])`, where `\w` means
  Unicode categories L* and N* plus `_`.
- **Trailing dot.** A mention followed by `.` (for example `cancelOrder.` at
  the end of a sentence) does **not** match, because the boundary rule
  treats `.` as part of a dotted name. Relaxing this would change the link
  set and needs updated expectations.
- **Deduplication.** Links are deduplicated on (entity, document, chunk,
  resolver), and the first link wins. Rows are written in batches of 5000
  with `INSERT OR IGNORE`.

### Incremental builds

- Links of touched files are removed with their rows. Touched files are then
  matched in both directions against the whole corpus.
- A test checks that incremental relinking produces exactly the same link set
  as a cold build.

### Manual links

- **Storage.** Manual links live in `manual_links`, keyed by:
  - entity qualified name;
  - document relative path;
  - attachment index;
  - chunk ordinal.
- **Key.** Attachment index and chunk ordinal default to -1 (whole
  document); the full key is UNIQUE.
- **Behavior.** Every build re-applies manual links as resolver `user`, with
  confidence `exact` and relation `documented_by`. Manual links survive
  reprocessing and rebuilds until they are removed explicitly.

## Evidence

- `fixtures/expected/links.json` holds the 23 expected links for
  `fixtures/linking`; the linker's link set must equal it.

## Benchmark

`benchmarks/linking-2100.json`: 2,000 code modules and 100 markdown
documents (14,000 entities, 4,100 units, 12,000 links). Linking with every
file touched takes about 2.7 s; a cold index end to end about 6.6 s.

## Interfaces

- The CLI exposes manual links as `docmonk link add/remove/list`, on top
  of the library API `docmonk_knowledge::manual::{add, remove}`.
