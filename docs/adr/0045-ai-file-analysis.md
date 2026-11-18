# ADR 0045: AI file analysis

Status: accepted and implemented (plan `plans/ai-file-analysis.json`).

## Context

Every document is analysed in-process: docling.rs, the PDF text layer and
`ocrs` OCR produce Docling JSON (ADR 0011), the normalizer and the chunker
turn it into chunks (ADR 0010), and `DocumentMetadata` picks the title. That
path is deterministic, offline and fast, but it reads only what a text layer
or OCR can recover: scanned layouts, complex tables, slides and forms lose
structure.

Users who already pay for Claude or ChatGPT, by subscription or API key,
want those models to read their documents instead, the way they choose a
storage engine: with one switch.

## Decision

1. **One flag, shaped like `storage.mode` / `storage.server.engine`.**

   ```yaml
   documents:
     analysis:
       mode: ai            # native (default) | ai
       provider: claude_code   # anthropic | openai | claude_code | codex
       model: ""           # empty: the provider's default
       timeout_seconds: 300
       max_file_bytes: 33554432
       concurrency: 2      # analyses in flight, 1-16
       max_output_tokens: 16000
   ```

   It is settable in YAML, as `DOCMONK_DOCUMENTS__ANALYSIS__MODE=ai`, and with
   `docmonk config set documents.analysis.mode ai`. `mode: ai` without a
   provider is a configuration error. With `native`, nothing changes.

2. **Full bypass for documents.** With `ai`, docling.rs, the PDF text layer,
   OCR, the normalizer, the chunker and `DocumentMetadata` do not run on a
   document. The model returns, per file, one JSON object
   (`docmonk_ai::analysis`, schema revision 1): title, author, language,
   summary, keywords, page count, the chunks in reading order (kind, heading
   path and level, pages, verbatim text, embedding text, table rows) and the
   code the file names. Only non-analysing guards stay native: format
   detection, `max_file_bytes`, `documents.max_pages` (page count only), the
   email MIME split, schema validation and the embedding budget split
   (decision D3). Code files keep the native code intelligence.

3. **Four providers, each through its official client.**

   | Provider | Account | How files reach the model |
   |---|---|---|
   | `anthropic` | `ANTHROPIC_API_KEY` | Messages API (`anthropic-version: 2026-10-21`): PDF as a `document` block, images as `image` blocks, structured output (`output_config.format`), refusal fallbacks (`fallbacks: "default"`). Default model `claude-opus-5-5`. |
   | `openai` | `OPENAI_API_KEY` | Responses API: `input_file` / `input_image`, strict `json_schema`. Default model `gpt-4o-mini`. |
   | `claude_code` | Claude Pro/Max | The official `claude` CLI, headless: `claude -p --output-format json --json-schema … --restricted --tools Read --strict-mcp-config --no-session-persistence --permission-mode dontAsk`, in a fresh private directory holding only the file under an internal name. |
   | `codex` | ChatGPT plan | The official `codex app-server` (ADR 0026): a fresh thread, approvals `never`, a read-only sandbox limited to a private directory. Text and `localImage` inputs with `outputSchema`; a PDF is sent as its pages rendered to PNG (hayro, pure Rust, longer side 1600 px, at most `documents.max_pages`), since the runtime has no PDF input. |

   Subscription runtimes are found by fixed executable name on `PATH`, never
   a configured path. `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
   `ANTHROPIC_BASE_URL`, `OPENAI_API_KEY`, the DocMonk key variables and
   GitHub tokens are removed from the `claude` child's environment, so the
   CLI's own sign-in is the only credential and a subscription is never
   billed as API usage. DocMonk never reads, copies or replays a runtime's
   credentials. `claude_code` is also a `docmonk ask` provider and a
   subscription runtime for `docmonk ai status|login|logout|models` (sign-in
   stays with the CLI: `claude`, then `/login`).

4. **Privacy first.** Every analysis provider sends file content off the
   machine and needs `privacy.external_ai_allowed: true`.
   `docmonk_convert::check_analysis` runs before an index run (CLI and
   daemon) and stops it with the policy error (exit 8), or a configuration
   error (exit 3) for a missing provider or key, before any file is read. A
   registry built without the check fails every document file with
   `ai_analysis_not_configured`. Secret redaction (ADR 0042) applies to
   stored text, not to the file bytes sent for analysis.

5. **No native fallback.** A file whose analysis fails is recorded failed
   with its code, and has no document rows:

   | Code | When | Retried |
   |---|---|---|
   | `ai_analysis_quota` | usage or rate limit | next index |
   | `ai_analysis_timeout` | no answer in `timeout_seconds` | next index |
   | `ai_analysis_failed` | provider error, 5xx, connection | next index |
   | `ai_analysis_not_signed_in` | 401/403, CLI not signed in or not installed | next index |
   | `ai_analysis_invalid_output` | not a valid analysis, cut off at `max_output_tokens` | when the file changes |
   | `ai_analysis_unsupported_format` | the provider cannot take the input | when the file changes |
   | `ai_analysis_too_large` | over `max_file_bytes` or a provider limit | when the file changes |
   | `ai_analysis_rejected` | HTTP 400/404/422 | when the file changes |
   | `ai_analysis_refused` | the model declined | when the file changes |

6. **Identity and cache.** The converter identity is
   `ai-analysis.1+<provider>+<model>+schema.<n>+prompt.<sha256[..12]>`
   (`+eml-attachments` when on), and the chunker identity gains `+ai`, so
   switching mode, provider, model, prompt or schema rebuilds every document
   once. Validated results are cached under `<home>/cache/conversion/ai`,
   keyed by the file's SHA-256 and that identity: re-indexing an unchanged
   file makes no model call and yields identical rows.

7. **Rows.** Model kinds map to the stored ones: `heading`, `table`, and
   `text`/`list`/`caption` as `paragraph`. A chunk keeps the model's verbatim
   text, heading path, pages and table rows; its parent is the last heading
   at the enclosing depth. Every chunk's embedding text starts with the
   document context (title, summary, keywords, section) and the code names
   it verifiably mentions; the header takes at most half the embedding
   budget. The provenance is the file's converter identity, which every file
   record already stores.

8. **No schema change.** Summary and keywords live in the embedding text,
   provenance in the file's converter version, and code references in the
   chunk text the knowledge linker already scans: a reference is kept only
   when its quote and name occur in the chunk, and the linker links it only
   when it resolves to an existing entity (ADR 0012). Local databases and
   server indexes stay compatible; turning AI analysis on or off is a
   rebuild, never a reset.

## Decisions taken with the owner

* **D1** Office, OpenDocument and EPUB files are unzipped and their content
  XML/HTML members sent as text (styles, themes, settings, relationships and
  layouts skipped), capped by `max_file_bytes`. Opening the zip is not
  analysis.
* **D2** (revised by the owner) ChatGPT reads PDFs: `codex` gets a PDF as
  its rendered page images, in order, with a note naming the page count.
  PNG and JPEG images go as they are. The renderer identity is part of the
  cache key, so a renderer change re-analyses those PDFs.
* **D3** A chunk over the embedding budget is split by the bundled tokenizer
  (sentences, then words), keeping its heading path and pages.
* **D4** Analysis has its own provider settings (`documents.analysis.*`),
  separate from `ai.*`: changing `ask` never rebuilds the index.
* **D5** Quota, timeout, provider and sign-in failures are transient and
  retried on the next index; the rest are permanent until the file changes.
* **D6** The flag is global, like `storage.mode`; a per-source override is a
  follow-up.

## Surfaces

* `docmonk ai analyze FILE [--json]` analyses one file with the configured
  provider and prints the result, without indexing or caching it.
* `docmonk ai providers` shows `analysis=true|false` per provider.
* `docmonk status` prints `Document analysis: AI (...)` in AI mode; failed
  files appear with the other errors under their codes.
* `docmonk doctor` checks, in AI mode, consent and the provider's key or
  runtime.
* The Admin UI configuration page lists `documents.analysis` like every
  other section.

## Verification

* `crates/docmonk-ai/src/analysis.rs`: schema validation, error codes and
  retry classes.
* `crates/docmonk-ai/tests/analysis/cases.rs` (in the `golden` binary): the
  Anthropic and OpenAI requests against a local HTTP mock (headers, native
  PDF blocks, structured output, no sampling parameters, no forced tool
  choice) and the classing of 401/429/529/400, refusals and cut-off answers;
  a fake `claude` on a test search path proving the lock-down flags, the
  internal file name and that API keys never reach the child.
* `crates/docmonk-convert/tests/it/ai_analysis.rs`: the bypass, the input per
  format, one analysis per unchanged file, failure codes with no rows, the
  budget split, the run-start privacy check and an index run.
* `benchmarks/ai-analysis/live.json` (owner-run, `commands.ai_live`):
  native against AI on the fixture corpus with real accounts.

## Consequences

* Indexing speed and cost now depend on the provider; the cache and
  `concurrency` bound both.
* Output is not deterministic across providers or models; it is stable for
  one identity because results are cached.
* Rollback: set `documents.analysis.mode: native`; the next index rebuilds
  documents natively.
