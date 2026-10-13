# ADR 0010: Document core

Status: accepted

## Scope

- `docmonk-documents` holds the normalized-document model, the chunker, the
  table renderer, document metadata and the pinned tokenizer.
- The chunker consumes only the **canonical intermediate JSON**: a normalized
  document is a flat, index-addressed unit list (`title`, `page_count`,
  `is_scanned`, `units[]` with `kind`, `text`, `heading_level`,
  `heading_path`, `parent_index`, `page_start`/`page_end`, `table_rows`,
  `caption`, `header_row_count`).
  - Conversion that produces this format lives in `docmonk-convert`
    (ADR 0011).

## Tokenizer

- **Library.** The Hugging Face `tokenizers` crate, pinned at `=0.23.2`. It
  is built without the `onig` C dependency; the `fancy-regex` feature is
  enough for the BERT pipeline.
- **Assets.** The `all-MiniLM-L6-v2` tokenizer assets (revision
  `1110a243…`) are compiled into the binary. They are verified against a
  SHA-256 manifest before first use. The tokenizer never touches the
  network.
- **Identity.** `chunker_version` (`2+tok:<revision>:<fingerprint>:max256`)
  records the tokenizer revision and fingerprint, so a tokenizer change
  forces a rebuild.

## Text semantics

- Whitespace is Unicode White_Space plus U+001C–U+001F, for trimming and
  splitting alike.
- Sentences split after `.`, `!` or `?` followed by whitespace (implemented
  without lookbehind, which the `regex` crate lacks).
- Sub-word splits cut at the tokenizer's own byte offsets.

## Evidence (`fixtures/expected/documents.json`)

The expected file covers:

- Markdown, HTML and TXT fixtures, including Greek/CJK/emoji/RTL text, a
  60-row table, a wide oversized cell and long headings.
- Synthetic documents: an oversized captioned table, an oversized single
  cell, a multi-row header, an empty table, very long title/breadcrumbs,
  Unicode paragraphs, and an empty document.

All of them are chunked under three configurations (default, small and
tiny), giving 30 document × configuration cases, compared exactly: every
chunk field and every diagnostics counter. A 20-string Unicode corpus pins
token counts with and without special tokens, sub-word splits, budget
splits and sentence splits.

## Boundaries

- Every paragraph and table chunk's payload stays within
  `max_tokens - safety_tokens`.
- Documented exceptions: headings are stored whole, and so is a single
  table-cell fragment that cannot be cut.
- Splits never cut inside a character.
- Oversized content (5k words plus a 20k-character word) chunks
  deterministically within the ceiling.

## Benchmark

`benchmarks/chunking-golden.json`: all expected documents × configurations
chunk in about 0.3 s; tokenization dominates.
