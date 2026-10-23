# ADR 0040: An optional multilingual embedding profile

Status: accepted and implemented (local-model retrieval plan, phase 4).

## Context

DocMonk embeds with `sentence-transformers/all-MiniLM-L6-v2`. It is an
English model: its WordPiece vocabulary breaks Greek, Arabic, Chinese and
Japanese into fragments, and it has no cross-lingual training. Phase 3
fixed same-language lexical matching, but a question in one language
about a document in another can only be answered by embeddings. The
held-out evaluation (`benchmarks/heldout-eval/phase-3.json`) put
`cross_language` recall@10 at 0.231 for hybrid search and 0.154 for
semantic search.

The plan named two local candidates: `intfloat/multilingual-e5-small`
and `sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2`.
Model cards and equal vector sizes are not evidence that a model works
in DocMonk's in-process Candle BERT, so each candidate was first run
through a compatibility check, then the held-out evaluation.

## Compatibility

Both are 12-layer `BertModel` exports (384 dimensions, mean pooling)
with an XLM-R SentencePiece vocabulary of 250,037 tokens. They load
unchanged into `docmonk_ml::bert`, with absolute positions from 0 and
the same weight names. Two additions were needed:

* The tokenizer pads with `<pad>` when the vocabulary has no `[PAD]`.
  The attention mask hides padding either way.
* `EmbeddingModelSpec` gains `query_prefix` and `passage_prefix`. E5 is
  trained on `query: ` / `passage: ` inputs: `Embedder::embed` writes the
  passage prefix and `Embedder::embed_queries` the query prefix. Both
  are part of the model fingerprint when they are set. Models without
  prefixes keep the fingerprint they had: MiniLM is still
  `0db613ee41822c8b`, so existing vectors stay valid.

`crates/docmonk-ml/tests/multilingual.rs` loads each pinned candidate
from verified files, checks sizes and determinism, and checks that five
English questions are closer to their Greek, Chinese, Japanese, Arabic
and German answers than to an unrelated text in the same language. Both
candidates pass 5/5; MiniLM gets 3/5.

## Adoption rule

Fixed before the candidates' held-out numbers were read. A candidate
becomes a profile only if all of these hold:

1. hybrid `cross_language` recall@10 and NDCG@10 each improve by at
   least 0.10 absolute;
2. no category or query language loses more than 0.05 recall@10 in the
   semantic or hybrid arm, and no code category loses any;
3. semantic p95 latency stays at most 3x, and a clean build at most 4x,
   the default's.

## Measurement

SQLite, release build, 4 CPU threads, no GPU
(`benchmarks/heldout-eval/phase-4-*.json` against `phase-3.json`; the
E5 report also covers both server engines;
`benchmarks/embedding-models/phase-4.json` for throughput):

| | MiniLM-L6 (default) | multilingual-e5-small | paraphrase-multilingual-MiniLM-L12 |
|---|---|---|---|
| Hybrid `cross_language` recall@10 / NDCG@10 | 0.231 / 0.179 | 1.0 / 0.729 | 1.0 / 0.804 |
| Semantic `cross_language` recall@10 / NDCG@10 | 0.154 / 0.074 | 1.0 / 0.757 | 1.0 / 0.905 |
| Hybrid recall@10 / recall@1, all queries | 0.854 / 0.811 | 0.979 / 0.845 | 0.996 / 0.867 |
| `evidence` recall@10 | 0.854 | 0.979 | 0.985 |
| Category drops (semantic or hybrid) | n/a | none | semantic `code_implementation` 1.0 → 0.938, `multilingual` 1.0 → 0.944 |
| Top hit covers the labelled code line (hybrid) | 0.889 | 0.852 | 0.852 |
| `evidence` false abstention / false support | 0.136 / 0.083 | 0.148 / 0.083 | 0.159 / 0.083 |
| Semantic query p95 | 24.8 ms | 40.2 ms | 31.6 ms |
| Clean build of the corpus | 6.0 s | 17.1 s | 8.6 s |
| Model load | 0.27 s | 3.9 s | 2.4 s |
| Embedding throughput (1000 texts) | 35 texts/s | 17 texts/s | 24 texts/s |
| Peak memory, embedding benchmark | 0.32 GB | 1.22 GB | 1.22 GB |
| Max input | 256 tokens | 512 tokens | 128 tokens |

On OpenSearch 2.17 and Elasticsearch 8.15 (same report),
`multilingual-e5-small` gives the same quality as on SQLite: hybrid
recall@10 0.979 and `cross_language` recall@10 1.0. Semantic p95 rises
from 90 ms to 113 ms (OpenSearch) and from 77 ms to 95 ms
(Elasticsearch).

`multilingual-e5-small` passes all three conditions.
`paraphrase-multilingual-MiniLM-L12-v2` ranks higher overall but fails
condition 2: its semantic arm loses `code_implementation` and
same-language `multilingual` recall. Its 128-token input also truncates
DocMonk's chunks of up to 256 tokens. Its numbers were measured with the
same harness while it was selectable on this branch.

## Decision

* `search.embedding_model` selects the embedding model: `all-minilm-l6-v2`
  (default) or `multilingual-e5-small`. The default does not change: the
  multilingual model triples build time and quadruples memory, and
  English and code retrieval do not need it.
* The paraphrase model stays pinned in `EVALUATED_EMBEDDING_MODELS`, so
  the spike test and the benchmark can reproduce its numbers, but it is
  not selectable.
* The configured model is part of every file's identity
  (`embedding_model_id = <slug>@<fingerprint>`). Changing it is a full
  rebuild on both backends: locally every vector is re-embedded, and on
  a server no unchanged record is copied forward with the old model's
  vectors. Vectors of different models are never searched together:
  queries are filtered by fingerprint on every backend.
  `crates/docmonk-service/tests/embedding_model_switch.rs` switches a
  source from MiniLM to E5 and checks that every vector was re-embedded
  and none of the old model is left.
* The cross-encoder stays `ms-marco-MiniLM-L-6-v2`. No multilingual
  cross-encoder was evaluated.
* Models are only ever loaded from verified local files
  (`scripts/fetch_models.sh` fetches the pinned E5 files with the
  others). Nothing is downloaded at query time.

## Rebuild and rollback

Introducing `embedding_model_id` changes every file's identity once.
That rebuild coincides with phase 3's. Switching between the two models
is a full rebuild in either direction. An older binary does not know the
`search.embedding_model` setting: remove it from `config.yaml` before
rolling back.
