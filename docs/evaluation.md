# Retrieval evaluation

DocMonk's retrieval quality is measured by four test suites. Each one
indexes a fixed corpus and checks the result against labels committed to
the repository.

| Suite | Corpus and labels | Gates |
|---|---|---|
| `docmonk-retrieval --test quality` | `fixtures/search_quality` (72 golden queries) | `fixtures/search_quality/gates.json` |
| `docmonk-service --test repo_eval` | DocMonk's own repository | `fixtures/search_quality/repo-eval-gates.json` |
| `docmonk-service --test p0_eval` | generated, 250 planted-fact queries | `fixtures/p0/eval-gates.json` |
| `docmonk-service --test heldout_eval` | `fixtures/eval/corpus`, 100 held-out queries | `fixtures/eval/heldout-gates.json` (approved labels only) |

## The held-out evaluation

`fixtures/eval/corpus` is a small versioned corpus with:

<!-- clean-slate-audit: allow-start (indexed source languages) -->

* code in Python, TypeScript, C# and Rust where the answers sit in function
  bodies, comments and constants (not in names or signatures), plus
  distractor files with overlapping vocabulary;
* a Ruby script and a SQL script (code extensions without a grammar) and
  a Python file with syntax errors;

<!-- clean-slate-audit: allow-end -->

* English, Greek, German, French, Japanese, Chinese and Arabic documents, a
  mixed English/Greek runbook, a Markdown and a CSV table and a scanned PDF
  (OCR).

`fixtures/eval/queries.json` holds 100 queries in 13 categories:
`code_implementation`, `code_symbol`, `code_fallback`, `document_fact`,
`table`, `numeric`, `negation`, `multilingual` (query and document in the
same non-English language), `cross_language` (query and document in
different languages), `mixed_language`, `ocr`, `ambiguity` and
`unanswerable`. Each label lists the relevant source-relative paths, and
for code a line the cited span must cover.

### Label review

The labels were written by a model from the corpus facts. They are **not**
human-reviewed: every query has `"origin": "model_authored"` and
`"review_status": "pending_human_review"`. The harness reports metrics for
every query, but the floors in `fixtures/eval/heldout-gates.json` apply only
to queries whose `review_status` a reviewer changed to `approved` (and
added `reviewed_by`). To approve labels:

1. Read the query, the `expected` paths and lines, and the corpus file.
2. Fix the label if needed, set `"review_status": "approved"` and
   `"reviewed_by": "<name>"`.
3. Run the evaluation and add floors for the approved set under
   `floors.<backend>.<arm>` in `heldout-gates.json`, a margin below the
   measured values. Floors are never lowered to make a change pass.

Running the evaluation never writes `queries.json` (the test checks its
SHA-256 before and after).

### Arms and metrics

Every arm reads the same snapshot through the service entry points the CLI
uses:

| Arm | Entry point |
|---|---|
| `lexical` | `query::run_search`, lexical results |
| `semantic` | `query::semantic_value` (needs the embedding model) |
| `hybrid` | `query::run_search` with RRF fusion |
| `hybrid_reranked` | the same with the cross-encoder pass |
| `evidence` | `evidence::evidence_value` (routed, grounded) |
| `explore` | `query::explore_value`, its `evidence` list |

Per arm, category and query language the report has Recall@1/5/10, MRR and
NDCG@10 over distinct paths, and `line_provenance@1` / `@10`: whether the
top hit (or any top-10 hit) on the labelled file covers the labelled line
with a span shorter than 80 lines. For `evidence` it also has citation
validity (citations that re-verify against the snapshot),
`abstention_unanswerable`, `false_support_rate` (an unanswerable query
reported `supported`) and `false_abstention_rate` (an answerable query
reported `insufficient_evidence`).

The report also records latency p50/p95 per arm, the process's peak
resident memory, clean-build and one-file update time, extraction coverage
(files by kind and status, record counts, degraded files), which local
models were available and every degraded stage. A query whose required
capability is missing (`"requires": ["ocr"]` without OCR models) is listed
as skipped, never scored as a failure.

### Hard gates

These hold for every query, approved or not:

* two passes over the same snapshot return identical rankings, citations
  and verdicts;
* every citation `evidence` returns re-verifies;
* the label file is unchanged after the run.

### Running it

```bash
scripts/fetch_models.sh /tmp/models          # optional: pinned local models
export DOCMONK_MODELS_DIR=/tmp/models DOCMONK_OCR_MODELS_DIR=/tmp/models/ocrs
DOCMONK_EVAL_OUT=/tmp/heldout.json \
  cargo test --release -p docmonk-service --test heldout_eval
```

Without the models the semantic arm is skipped and the hybrid arms fall
back to lexical-only fusion; the report says so under `models` and
`degraded_stages`. Latency is only meaningful in a release build (the
report's `build_profile`).

Server backends run when `DOCMONK_TEST_OPENSEARCH_URL` /
`DOCMONK_TEST_ELASTICSEARCH_URL` point at disposable clusters (see
`CONTRIBUTING.md`); each gets a fresh home and index prefix, the same
corpus and the same labels. Otherwise the report lists them as `not_run`.

### Recorded results

`benchmarks/heldout-eval/` keeps the report of each change that moved the
numbers, so before/after comparisons stay reproducible from the repository.
