# ADR 0039: A normalized lexical projection, detected languages and per-page PDF OCR

Status: accepted and implemented (local-model retrieval plan, phase 3).

## Context

Lexical search matched words exactly as the tokenizers cut them:

* SQLite's `unicode61` folds Latin diacritics, but not Greek ones. The
  server's `standard` analyzer folds no diacritics at all. So `αδειας`
  did not find `άδειας`, `cafe` found `café` locally but not on a
  server, and an inflected Greek form (`άδεια`, `άδειας`, `άδειες`)
  matched only itself.
* Chinese and Japanese text has no spaces between words. Locally a whole
  run (`経費精算は30日以内に提出`) was a single token; on a server every
  Han character was a token. A query word matched neither unless it
  equalled a whole run. The held-out evaluation
  (`benchmarks/heldout-eval/phase-2.json`) put lexical recall@10 for
  Japanese and Chinese queries at 0.0.
* Grounding support compared raw lowercase substrings, so an inflected
  answer did not support its question. Evidence abstained on 19% of
  answerable queries.
* A PDF was OCR'd only when the whole document looked scanned. A mostly
  textual PDF with a few scanned pages lost those pages silently, and OCR
  output, when used, replaced the text layer. A scanned PDF indexed
  without OCR models looked like an empty document with no explanation.
* Sentences split only at `.`, `!`, `?` followed by a space. A long
  Chinese or Japanese paragraph was cut mid-word by the token budget.

## Decision

1. **A projection, not an analyzer.** `docmonk_core::lexnorm` computes
   normalized forms in DocMonk itself. Both backends index the forms next
   to the original text: the local FTS `words` column (with the
   identifier parts it already held) and a new server `norm` field. The
   `norm` field uses a whitespace analyzer with lowercase and the same
   English stemmer as `docmonk_text`. No engine plugin or language
   analyzer is needed, and both engines match the same forms. The rules:
   * NFKC and lowercase;
   * diacritics removed after Latin, Greek, Arabic and Hebrew letters,
     but never after kana (a dakuten changes the sound) or Han.
     Cyrillic loses only `ё`'s diaeresis and stress accents;
   * `ß` → `ss`, final `ς` → `σ`;
   * a light Greek inflectional stemmer;
   * Arabic letter-variant normalization and a light prefix/suffix
     stemmer;
   * Han, kana and Hangul runs are indexed as their characters and every
     overlapping character pair.

   A word whose normal form is its lowercased self adds nothing, so plain
   English text and code identifiers are indexed as before.
2. **Queries get normalized tiers.** When some query words have other
   forms, the lexical plan follows every tier (phrase, all terms, prefix,
   "most terms") with the same tier over the normalized words. A
   Han/kana run is searched by its character pairs, a one-character run
   by the character. Greek and Arabic function words are not content
   terms. A plain English query plans exactly as before. The grammar the
   server parser reads is unchanged.
3. **The original text is untouched.** Snippets, citations and exact
   symbol and path matching use the stored text. A local chunk snippet
   that FTS5 drew from the `words` column falls back to the chunk body's
   start, as the server already does when only `norm` matched.
4. **Normalized term matching for checks that read text.** Grounding
   support and the shared term-coverage order (ADR 0038) use
   `TermMatcher`. A term matches as written, by its normal form, or (for
   a Han/kana/Hangul term) when at least half of its character pairs
   occur.
5. **Detected language and script.** Each document records its language
   (ISO 639-1) and script (ISO 15924) where detection is reliable:
   * by script where one language dominates it (Greek, kana, Han without
     kana, Hangul, Hebrew, Arabic or Persian);
   * by function words for English, German, French, Spanish, Italian,
     Portuguese and Dutch, when one language clearly leads;
   * never from fewer than 20 letters.

   Each chunk records its own language, so a mixed English/Greek file
   keeps both. `docmonk documents --json` shows the document's language
   and script, and evidence citations show their chunk's language.
6. **Sentences** also end at `。`, `！`, `？` (no space needed) and at the
   Arabic and Devanagari full stops. Chinese and Japanese pieces are
   rejoined without inserted spaces.
7. **PDF pages.**
   * `documents.ocr: auto` OCRs exactly the pages whose text layer has
     fewer than 50 characters. `always` OCRs every page.
   * `pdf::merge_ocr` adds each recognized block to its page after the
     page's existing content. It skips a block whose folded words the
     page already holds at least 80% of (OCR misreads a few), and it
     never removes or rewrites the text layer.
   * Each PDF records how its pages were read (`pages.ocr`: `not_needed`,
     `applied`, `no_images`, `unavailable`, `failed` or `disabled`), the
     pages that gained OCR text, and the pages still without text.
   * When the OCR a page needed could not run, the PDF is indexed with a
     notice (`ocr_unavailable`, `ocr_failed`) and counted as degraded,
     like a file with syntax errors (ADR 0037). Blank pages (no image,
     or OCR found nothing) and pages left unread by `documents.ocr: off`
     are recorded on the document only, so blank print pages and a
     deliberate configuration do not flag files.

## Rebuild and rollback

* The projection rules have one identity, `LEXICAL_PROJECTION_VERSION =
  lexnorm-1`. It is folded into the code files' `parser_version`
  (`rust-code-3+lexnorm-1`; the graph identity is unchanged, since entity
  ids do not depend on the projection) and into the documents'
  `chunker_version`.
* `CHUNKER_VERSION` is 3, for the new sentence boundaries.
* The PDF page policy is `PDF_PAGES_VERSION = pdf-pages-1`, part of the
  converter's `parser_version`.
* The local schema gains the `documents.language`, `script`, `ocr`,
  `ocr_pages`, `empty_pages` and `chunks.language` columns. The schema
  fingerprint changes, so an existing knowledge database is refused with
  the usual reindex instruction. Every source is then rebuilt.
* The server `SCHEMA_VERSION` is 7 (the `norm` field and its analyzer,
  the language and page fields). Indexes of schema 6 are refused, never
  mutated: use a fresh `index_prefix` and reindex.
* Conversion caches stay valid: the text layer and OCR output are cached
  as before, under keys of the pages they cover, and the merge runs after
  the cache.
* Rollback: an older binary refuses the new local and server schemas, so
  rolling back means reindexing with it.

## Measurement

`crates/docmonk-service/tests/heldout_eval.rs` measured this change on
SQLite, OpenSearch 2.17 and Elasticsearch 8.15
(`benchmarks/heldout-eval/phase-3.json` against `phase-2.json`, release
build, pinned MiniLM and OCR models). The three backends agree except
where shown:

| Metric | Phase 2 | Phase 3 |
|---|---|---|
| Lexical recall@10, all queries | 0.763 (servers 0.775) | 0.820 |
| Lexical recall@10, Japanese / Chinese queries | 0.0 / 0.0 (servers 0.5) | 1.0 / 1.0 |
| Lexical recall@10, Greek queries | 0.75 | 0.875 |
| Hybrid recall@1 | 0.799 | 0.811 |
| `evidence` recall@1 | 0.799 | 0.811 |
| `evidence` false abstention on answerable queries | 0.193 | 0.136 |
| `evidence` false support on unanswerable queries | 0.083 | 0.083 |
| Citation validity; scanned-PDF (OCR) recall@10 | 1.0; 1.0 | 1.0; 1.0 |

No arm, category or query language lost recall. Exact symbol, path and
English results did not move (`multilingual_lexical.rs` checks this on
every backend). Lexical p95 latency did not change measurably on SQLite.
On the servers it rose from 110 ms to 138 ms (OpenSearch) and from 104 ms
to 149 ms (Elasticsearch): a query with normalized forms sends the
normalized tiers too. The other arms' latency did not change measurably.
Clean builds took 6.0 s on SQLite (5.6 s before), and a one-file update
took 1.1 s (0.7 s before).

The golden ranking snapshots (`fixtures/expected/{search,explore}.json`)
were re-recorded. Entities and documents whose text gained normalized
forms (`café_total`, `Λογαριασμός`, `docs/unicode.txt`) changed places
with near-tied neighbours, because BM25 length normalization counts the
added tokens. The 72 golden queries, the P0 evaluation and the repository
evaluation keep their floors.
