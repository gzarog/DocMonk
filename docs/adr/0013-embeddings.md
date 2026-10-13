# ADR 0013: Pure-Rust embeddings, model manifest and embedding storage

Status: accepted

## Decisions

1. **Runtime.** Embeddings run on Candle (`candle-core`/`candle-nn`
   `=0.9.1`). It is pure Rust, with no native ONNX Runtime library, and it
   uses one build path on Windows, Linux and macOS. Version 0.11 needs NEON
   f16 intrinsics that are unstable on the pinned Rust 1.90 for aarch64
   (macOS CI); 0.9.1 builds there.
2. **Model.** `all-MiniLM-L6-v2` is the pinned embedding model; the
   measured comparison with the other candidates is recorded in ADR 0015.

## Model assets

- **Pinning.** `docmonk_ml::manifest` pins each model by Hugging Face id,
  exact revision and the sha256 of every file the runtime reads:
  `config.json`, `model.safetensors` and `tokenizer.json`.
- **Verification.** Files are read and verified before loading. A tampered
  or missing asset is a typed error.
- **Location.** Assets are not compiled into the binary. They are loaded
  from `DOCMONK_MODELS_DIR` (default `<home>/models`) under `<slug>/`;
  release archives and the container image ship them (ADR 0030).
- **No `unsafe`.** Weights load through
  `VarBuilder::from_buffered_safetensors`, not mmap, so the project keeps
  `unsafe_code = "forbid"`.
- **Fingerprint.** The fingerprint is sha256 over the model id, revision,
  file digests and preprocessing contract. It is stamped on every vector.
- **Preprocessing contract (`PREPROCESSING_VERSION` 1):**
  - cap each text at 4000 characters;
  - truncate at 256 tokens;
  - pad to the longest text in the batch;
  - mean-pool over the attention mask (CLS pooling for BGE-style specs);
  - L2-normalize.

## Encoder

- `candle-transformers`' generic BERT ran at 5.2 texts/s here; its batched
  3-D linear layers reached only about a third of plain GEMM throughput.
- `docmonk_ml::bert` is a lean encoder that keeps the standard BERT math:
  post-norm layers, exact-erf GELU and an `f32::MIN` padding bias.
- Restructured for the CPU:
  - activations stay 2-D, so each projection is one contiguous GEMM against
    a pre-transposed weight;
  - Q, K and V are fused into one projection;
  - the bias add with GELU and the masked softmax are parallel custom ops,
    in safe Rust (`CustomOp1`);
  - layer norm uses Candle's fused kernel.
- Texts are batched in length order and returned in input order. Padding is
  masked, so a vector does not depend on its batch; a test asserts this.

## Bounded inference

- Inference runs in fixed batches (`indexing.embedding_batch_size`, default
  16) and is serialized per embedder.
- The finalizer processes `EMBED_WINDOW` = 256 texts per window, so memory
  does not grow with build size.

## Storage

- **`embeddings` table.** Keyed by build, subject type and subject id. Each
  row stores the model fingerprint, text hash, dimensions and a
  little-endian f32 blob.
- **Build scoping.** Vectors are deleted with their file and carried forward
  with their rows, like all other derived data.
- **`embedding_cache` table.** Keyed by `(text_hash, model_fingerprint,
  embedding_text_version)`. Text versions are code `1` and document `2`.

## Rebuild semantics

A subject is *pending* when it has no vector under the current fingerprint.
The `EmbeddingFinalizer` (registered when `search.semantic` is on) embeds
every pending subject after linking. This one rule covers four cases:

| Case | What happens |
|---|---|
| Touched files | Their rows were replaced, so their subjects are pending. |
| Model change | Stale-fingerprint vectors are counted, logged (`model_changed_rebuild`), dropped and rebuilt explicitly. |
| Crash or missing model | Repaired on the next run, including warm passes through the `BuildFinalizer::on_warm_pass` hook. |
| Identical texts | Embedded once per window. Unchanged texts come from the cache. |

A missing model never fails indexing. Vectors stay pending and a warning is
logged.

## Evidence

- `fixtures/expected/embeddings-minilm.json` holds 20 expected vectors.
  The texts include multilingual, emoji, SQL, over-length and
  over-character-cap inputs. The test asserts a cosine of at least 0.9999
  against each and a unit norm.
- The finalizer tests cover:
  - completeness;
  - cache reuse on an incremental run;
  - a model change rebuilding every vector;
  - crash recovery;
  - the model-unavailable path.
- CI fetches the pinned model, verifies its sha256 and caches it, and sets
  `DOCMONK_REQUIRE_MODELS=1` so these tests cannot be skipped.

## Benchmark

`benchmarks/embedding-1000.json`: 1,000 mixed code and document texts,
batch 16, 4 threads, on an Intel Xeon at 2.8 GHz with AVX-512: model load
about 0.7 s, throughput about 28 texts/s.

- The pure-Rust `gemm` crate uses AVX2 on stable Rust; the encoder's GEMMs
  run at the crate's measured peak (about 130 GFLOPS).
- Indexing embeds only pending subjects, with cache reuse, so steady-state
  cost is proportional to change.

## Limitations

- There is no in-product model download. Release archives and the
  container ship the assets; developers fetch them with
  `scripts/fetch_models.sh`.
