# ADR 0023: CLI query commands

Status: accepted

## Commands

- **`symbol`, `callers`, `callees`, `references`.**
  - Each runs over every indexed source.
  - `--json` returns `{query, matches, edges}`.
  - Text output is plain tables.
  - `--max-depth` is limited to 1–10 and `--limit` to 1–1000.
- **`impact`.** The impact payload (ADR 0018). The text view lists defined at,
  callers, callees, tests, documentation, confidence and blast radius.
- **`explore`.**
  - Runs the explore planner and payload (ADR 0018).
  - When the plan includes semantic search and the model is installed,
    semantic search runs.
  - The context budget comes from the `context.*` settings.
- **`search`.**
  - Supports `--json`, `--snippets`, `--table`, `--explain`, `--hybrid`
    and `--limit` (1–200). The default output mode is
    `search.output.fallback[0]`.
  - `search.lazy_semantic` skips semantic search when lexical confidence
    is already high.
  - `--hybrid` merges lexical and semantic results and reranks them, with
    the optional cross-encoder pass.
  - `--explain` reports:
    - the query kind;
    - the lexical confidence;
    - whether semantic search was skipped;
    - per-stage timings;
    - the tokenizer identity.
- **`link add`, `remove`, `list`.**
  - `add` resolves the entity (by id, name or qualified name) and the
    document (by id, path, path suffix or filename) in exactly one source,
    and reports an error when the match is ambiguous. `--section` takes a chunk id.
  - Manual links (ADR 0012) are stored by qualified name
    and path, and survive every rebuild.
  - `list` shows a user link's manual id, which is what `remove` takes.
  - Automatic links are listed but cannot be removed: the next pass would
    recreate them.

## Supporting work

- **Absolute paths.**
  - Retrieval reports paths relative to the source root; the CLI prints
    absolute ones.
  - The CLI joins search result paths, path-hit titles, semantic hit
    paths, and every `path`, `paths` and `source_location` field in
    `explore` and `impact` payloads with the source's path.
  - It does this in the CLI so the retrieval crate's API and its tests stay
    source-relative.
- **Query classification (`retrieval::classify`).** `classify_query` (symbol, path, keyword or
  conceptual) and `estimate_confidence` (high, medium or low).
- **Context expansion (`retrieval::context`).**
  - `expand_chunk_context`:
  - A matched document chunk gets its enclosing heading and its nearest
    siblings under that heading, within `search.context.max_tokens`.
  - Token costs use the bundled tokenizer. The matched chunk is never
    dropped. Siblings are taken nearest-first, so the window stays
    contiguous.
  - A table's text is its row-aware rendering.
- **Chunk parents (storage).**
  - `chunks.parent_ordinal` stores the chunker's `parent_index`.
  - `ProjectStore::chunk`, `chunk_at` and `chunk_siblings` read it.
  - The stored chunker identity carries a `+chunk-parents.1` suffix, so
    the chunk layout is part of the rebuild decision.

## Tests

- `docmonk-cli/tests/queries.rs` covers, end to end:
  - text and JSON modes;
  - absolute paths;
  - range checks;
  - search output modes and `--explain`;
  - link add, list and remove, including duplicates, errors and survival
    across a rebuild;
  - `references`, `impact`, `explore` and a `search` that exercises context
    expansion on a document.
- `docmonk-retrieval/tests/context.rs` covers heading and sibling
  structure, table rendering and the token budget, on a real indexed
  Markdown document.
