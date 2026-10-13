# ADR 0018: explore, impact and attachment provenance

Status: accepted

## Scope

`docmonk_retrieval::explore` holds:

| Function | Behavior |
|---|---|
| `plan` | ordered regex rules (impact > callers > callees > document hint > identifier > general). Semantic search is only ever *added* to document and general plans. |
| `build_context` | dedupe per location (higher confidence wins, ties keep the first), sort by confidence then path/line/page/entity with `None` last, cap by `max_files` / `max_chars` (snippet characters) / `max_graph_nodes`, and report every cap in the truncation reasons |
| `Evidence`, `EvidenceLocation`, `EvidenceItem`, `GraphPath` | the evidence model |
| `linked_document_evidence` | one item per distinct (document, chunk) per corpus |
| `impact` | the impact JSON payload, including the blast-radius buckets (≤ 2 LOW, ≤ 7 MEDIUM, else HIGH; score = distinct callers + distinct documented sections) |
| `explore` | the explore JSON payload. Semantic results come from the caller, which holds the embedder. |

**Attachment provenance.** Lexical and semantic hits carry the full
attachment provenance: name, content type, index, format and parent title.

## Link choice

When several links connect an entity to the same (document, section), the
strongest wins: highest confidence, then the linker's resolver order
(exact, qualified, alias, filename, route), then link type. The result is
deterministic regardless of the order in which links were written.

## Evidence

- **`fixtures/expected/explore.json`** covers the `linking` and `graph`
  corpora:
  - `impact` for every entity name and qualified name plus a missing name,
    at depths 1 and 3: 196 payloads;
  - `explore` for 20 planner-shaped queries (callers, callees, impact,
    document, identifier and general).

  Ids are stripped. Link-, id- and insertion-ordered lists are compared as
  multisets: documentation, documents, evidence, call flows and
  dependencies.
- **`fixtures/expected/attachments.json`** uses the `fixtures/attachments`
  corpus: an email with text, Markdown, Office and archive attachments.
  Every lexical hit's kind, tier and attachment provenance is checked in
  order; the top-5 semantic hits' provenance as a set.
- **Unit tests** cover the planner examples, context deduplication,
  prioritization and truncation, and the blast-radius buckets.
