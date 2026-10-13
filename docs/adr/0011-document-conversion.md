# ADR 0011: Rust-native document conversion

Status: accepted

## Decisions

1. **PDF.** Text-layer conversion by default; the docling.rs ML pipeline is
   opt-in only.
2. **OCR.** Pure-Rust `ocrs`.

## Converter

- `docmonk-convert` integrates `docling` (docling.rs), pinned at `=1.89.0`,
  with `default-features = false`. That means no ONNX Runtime, no model
  weights, no ASR/VLM and no network access.
- It sits behind a `DocumentConverter` trait. The trait returns Docling
  document JSON (`export_to_json_value`).
- Formats: Markdown, TXT (routed through the Markdown backend), HTML, CSV,
  DOCX, PPTX, XLSX, ODT/ODS/ODP and EPUB, plus PDF, images and EML as
  described below. Recognized formats without a converter (for example the
  older binary Office formats) are indexed with no derived content.
- A backend panic is contained per file. Corrupt input fails only that
  file, permanently, with `conversion_error`.

## Normalizer

`normalize.rs` turns Docling JSON into the canonical normalized document
(ADR 0010), walking the document items in reading order:

- only the body layer is included;
- groups are flattened;
- picture children are limited to captions;
- table captions are consumed;
- header-row detection, the heading stack and title selection are applied;
- the low-text-density scanned check is included.

## Evidence

- `fixtures/expected/docling.json` (14 documents) pins the normalizer
  output, and docling.rs plus the normalizer must reproduce the expected
  normalized documents exactly, so any drift in the pinned converter is
  caught.
- End to end, documents indexed through the coordinator store the expected
  chunks: text, search text, contextual text, token counts, heading paths
  and table rows.

## Workspace note

`docling-core` enables serde_json's `preserve_order` workspace-wide.
Production JSON is unaffected. Tests that compare collections use an
order-independent key.

## Benchmark

`benchmarks/documents-600.json`: 600 mixed documents
(md/html/txt/docx/pptx/xlsx) index cold in about 7.5 s; a warm pass takes
0.02 s and a single edit 0.03 s.

## PDF and OCR

### PDF text layer

- PDFs are converted through docling.rs's `pdf-text` feature: pure Rust
  (`lopdf` plus a text parser), with no ML models and no network access.
- `documents.pdf_mode` `accurate` and `fast` share this path. The ML
  layout/table pipeline stays opt-in.
- A PDF with no text layer becomes an empty document that keeps its pages, so
  it is flagged `is_scanned`.

### OCR policy

- `always` tries OCR first.
- `auto` OCRs only when the plain result looks scanned. It uses the same
  low-text-density check as `is_scanned`.
- `off` never OCRs.
- If OCR is unavailable or fails, the plain result is kept.

### OCR engine

- **Engine:** `ocrs` `=0.12.0` on `rten` 0.24. Newer `ocrs`/`rten` releases
  need Rust 1.94, while the workspace pins 1.90, so we stay on these.
- **Input:** page images are extracted from the PDF with `lopdf`. JPEG and
  JPEG 2000 images are decoded with `image`. Flate-compressed 8-bit
  DeviceRGB and DeviceGray images are decoded as raw pixels.
- **Models:** they are not bundled. They load from `DOCMONK_OCR_MODELS_DIR`,
  else `<home>/models/ocrs`. Both files are verified against pinned SHA-256
  digests (`OCR_MODEL_MANIFEST`). Missing or modified models make OCR
  unavailable; that is logged once and the plain result is kept.
- **CI:** the workflow downloads the pinned models (cached and
  SHA-verified) and sets `DOCMONK_REQUIRE_OCR=1`, so the OCR tests can never
  silently skip.
- **Images:** `.png`/`.jpg`/`.tif` files are OCR'd only when
  `documents.image_ocr` is on. Otherwise they are indexed with no derived
  content.

### Page limit

- PDFs with more pages than `documents.max_pages` are recorded as
  `skipped_limit`. Processors report this with the `SKIPPED_LIMIT` error
  code, and the coordinator records it as a limit, not a failure.

### Conversion cache

- A file cache under `<home>/cache/conversion` covers PDF and OCR
  results.
- The key is the content hash plus the OCR/settings key, the converter
  version and the OCR engine version.
- Writes are atomic (temp file + rename). Corrupt entries are ignored and
  rebuilt.
- Workers use it without touching SQLite.

### Debug-build performance

`rten`, `ocrs`, image decoding and `tokenizers` are built with `opt-level=3`
in the dev profile. That brought the OCR test suite from 72 s to 7 s.

### Benchmark

`benchmarks/pdf-ocr-100.json`: 100 PDFs, half of them image-only scans
needing OCR, index cold in about 14.6 s, recovering the scanned text.

## EML and attachments

### Parent email

- The email body is converted by docling.rs's email backend.

### Attachment enumeration

`email.rs` enumerates attachments on top of `mail-parser`:

- **Ordering:** MIME leaves are walked in document order, and
  `message/rfc822` parts are treated as leaves. Ordinals are stable across
  re-parses and count skipped parts too.
- **What counts as an attachment:** a part with `Content-Disposition:
  attachment`, or a part with a filename that is not the selected text or
  HTML body.
- **Filenames:** decoded per RFC 2047 / RFC 2231, then sanitized:
  directory parts (POSIX and Windows, including drive prefixes),
  control/format characters and surrounding whitespace are removed, and the
  result is capped at 255 characters. A filename is display metadata only.
- **Limits:** byte, count and total-byte limits apply to decoded payloads.
- **Skip reason codes:** `nested_message`, `empty`, `too_large`,
  `count_limit`, `total_limit`, `unsupported_format`, `image_ocr_disabled`
  and `page_limit`.

### Child documents

- Each attachment is written into an OS temp directory under the internal
  name `attachment<ext>`. The sender's filename never touches the disk, and
  the directory is removed on every path.
- The extension comes from the filename's suffix first; an explicit
  unsupported suffix wins over the MIME fallback.
- The attachment then goes through the same convert → normalize → chunk path
  as a file. The title is the document's own title, else the attachment's
  display name.
- Rows: `document_id(file_id, ordinal)`. Provenance records parent, name,
  content type, ordinal and Content-ID; `content_hash` is the payload's
  SHA-256.
- A failed or corrupt attachment is logged (without payload) and dropped.
  The parent email is never failed.
- Enabling attachments adds `+eml-attachments.1` to `converter_version`.

### Evidence

- `fixtures/expected/email.json` pins parent bodies; attachment ordinals,
  names (including RFC 2231 non-ASCII), content types, sizes and
  Content-IDs; skip lists under default and tight limits; and
  per-attachment outcomes (format, title and chunk texts, or
  skipped/failed).
- Nested messages, empty parts and path-traversal names are covered by
  dedicated tests.
- Text attachments are decoded by `mail-parser`, so the size of a non-UTF-8
  text part is measured after charset decoding.

### Benchmark

`benchmarks/email-200.json`: 200 emails with 500 attachments index cold in
about 1.4 s.
