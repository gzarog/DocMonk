# ADR 0015: Cross-encoder reranker and embedding-model baseline

Status: accepted

## Decisions

1. **Reranker.** The `cross-encoder/ms-marco-MiniLM-L-6-v2` cross-encoder
   runs in pure Rust (Candle).
2. **Embedding model.** Keep `sentence-transformers/all-MiniLM-L6-v2` and
   record its measured search quality as the baseline.

## Reranker (`docmonk_ml::reranker`)

- **Pinned assets.** The model is pinned in the manifest by revision
  `233902d2` plus sha256 of `config.json`, `model.safetensors` and
  `tokenizer.json`. Every file is verified on load, like the embedder.
- **Shared encoder.** It reuses the embedding BERT encoder (ADR 0013) via
  `forward_typed`. Sentence pairs need segment ids: 0 for the query, 1 for
  the passage.
- **Scoring.** The pooler (dense + tanh on `[CLS]`) feeds a one-logit
  classifier. The raw logit is the score, with no activation applied.
- **Preprocessing:**
  - each passage is capped at 4,000 characters;
  - the pair is truncated longest-first at 256 tokens;
  - batches hold 16 pairs.
  
  Batches are formed in order of length. Padding is masked, so scores do
  not depend on batching; a test asserts this.
- **`rerank()`:**
  - it reorders only the first `top_n` items, sorting stably by descending
    score, and leaves the remainder in its original order;
  - with `top_n = 0` or fewer than two items, it returns the input
    unchanged;
  - if scoring fails (no model, or a score-count mismatch), it logs at
    info level and returns the input unchanged. An optional rerank never
    fails a search.
- **Hit metadata.** The reranker scores the snippet (else the title):
  - a chunk's title is the document title, else its path;
  - the snippet is the first 280 characters of the text, with a table's
    cells joined by spaces;
  - the heading path moves to `section`.
- **Hybrid search** runs lexical + semantic → RRF → exact-match pinning →
  rerank, behind a config switch (ADR 0016).

## Evidence

- **Logits.** `fixtures/expected/reranker-ms-marco.json`: 8 queries × 20
  passages, including over-length, Greek, Japanese and emoji text. The
  assertion allows a logit difference of 2e-3, and the reranked orders must
  be identical apart from swaps between near-ties under 4e-3.
- **Quality baseline.** 72 labelled queries over the search-quality
  fixture project (`fixtures/search_quality`). Two arms are scored:
  semantic top 10, and the top 20 reranked by the cross-encoder, then
  top 10 (`benchmarks/semantic-quality.json`):

| | Recall@1 | Recall@3 | Recall@5 | Recall@10 | MRR | NDCG@10 |
|---|---|---|---|---|---|---|
| semantic | 0.706 | 0.822 | 0.863 | 0.917 | 0.858 | 0.838 |
| semantic + rerank | 0.678 | 0.891 | 0.919 | 0.924 | 0.855 | 0.859 |

  The reranker lifts Recall@3–10 and NDCG@10 but costs a little Recall@1.
  The test fails if any metric drifts by more than 0.03 from this record.

## Benchmark

`benchmarks/rerank-20pairs.json` (4 threads): each call scores 20 pairs,
including texts of 3,000–5,000 characters; p50 is about 758 ms per call
and the model loads in 0.76 s. Batching in length order brought this from
2.3 s to 0.76 s. On the realistic workload, the top 20 snippets of 280
characters or fewer, the rerank stage takes 246 ms p50 (303 ms p95).

## Limitations

- Inference is CPU-only at f32.
- The quality fixture is small (6 files, 72 queries). It is a regression
  baseline, not a model-selection study. Revisit the model only with a
  larger labelled corpus.
