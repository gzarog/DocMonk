//! Pure-Rust (Candle) sentence embeddings for BERT-family models.
//!
//! The preprocessing contract is fixed: cap each text
//! at `max_chars` characters, tokenize with truncation at `max_tokens`, pad
//! to the longest text of the batch, pool (`Mean` over the attention mask or
//! `Cls`) and L2-normalize. Inference is serialized per embedder and runs in
//! fixed-size batches, so memory is bounded by one batch.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use sha2::{Digest, Sha256};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::bert::{Bert, BertConfig};
use crate::manifest::{EmbeddingModelSpec, Pooling};

#[derive(Debug, thiserror::Error)]
pub enum MlError {
    #[error("model asset {path} is missing: {source}")]
    Missing {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "model asset {path} failed integrity verification (expected sha256 {expected}, got {actual})"
    )]
    Integrity {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("model load failed: {0}")]
    Load(String),
    #[error("inference failed: {0}")]
    Inference(String),
}

pub(crate) fn inference(e: impl std::fmt::Display) -> MlError {
    MlError::Inference(e.to_string())
}

pub(crate) fn load_err(e: impl std::fmt::Display) -> MlError {
    MlError::Load(e.to_string())
}

/// Reads `dir/name` and checks its sha256.
pub fn read_verified(dir: &Path, name: &str, sha256: &str) -> Result<Vec<u8>, MlError> {
    let path = dir.join(name);
    let bytes = std::fs::read(&path).map_err(|source| MlError::Missing {
        path: path.clone(),
        source,
    })?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != sha256 {
        return Err(MlError::Integrity {
            path,
            expected: sha256.to_owned(),
            actual,
        });
    }
    Ok(bytes)
}

/// Default batch size.
pub const DEFAULT_BATCH_SIZE: usize = 16;

pub struct Embedder {
    spec: EmbeddingModelSpec,
    fingerprint: String,
    tokenizer: Tokenizer,
    model: Bert,
    device: Device,
    batch_size: usize,
    gate: Mutex<()>,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("model", &self.spec.hf_id)
            .field("fingerprint", &self.fingerprint)
            .field("batch_size", &self.batch_size)
            .finish()
    }
}

impl Embedder {
    /// Loads `spec` from `dir`, verifying every pinned file first.
    pub fn load(dir: &Path, spec: EmbeddingModelSpec) -> Result<Self, MlError> {
        let file = |name: &str| {
            let f = spec
                .files
                .iter()
                .find(|f| f.name == name)
                .ok_or_else(|| MlError::Load(format!("manifest has no {name}")))?;
            read_verified(dir, f.name, f.sha256)
        };
        let config: BertConfig = serde_json::from_slice(&file("config.json")?).map_err(load_err)?;
        let mut tokenizer = Tokenizer::from_bytes(file("tokenizer.json")?).map_err(load_err)?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: spec.max_tokens,
                ..Default::default()
            }))
            .map_err(load_err)?;
        let pad_id = tokenizer.token_to_id("[PAD]").unwrap_or(0);
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id,
            pad_token: "[PAD]".into(),
            ..Default::default()
        }));
        let device = Device::Cpu;
        let vb =
            VarBuilder::from_buffered_safetensors(file("model.safetensors")?, DType::F32, &device)
                .map_err(load_err)?;
        let model = Bert::load(vb, &config).map_err(load_err)?;
        Ok(Self {
            fingerprint: spec.fingerprint(),
            spec,
            tokenizer,
            model,
            device,
            batch_size: DEFAULT_BATCH_SIZE,
            gate: Mutex::new(()),
        })
    }

    /// Overrides the batch size (`indexing.embedding_batch_size`); values
    /// below 1 keep the default.
    pub fn with_batch_size(mut self, batch_size: i64) -> Self {
        if batch_size > 0 {
            self.batch_size = usize::try_from(batch_size).unwrap_or(DEFAULT_BATCH_SIZE);
        }
        self
    }

    pub fn spec(&self) -> &EmbeddingModelSpec {
        &self.spec
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// One L2-normalized vector per text, in order.
    ///
    /// Texts are batched in order of length so each batch pads to a similar
    /// length (padding is masked out, so a vector does not depend on its
    /// batch), then returned in input order.
    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, MlError> {
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for idx in order.chunks(self.batch_size) {
            let batch: Vec<&str> = idx.iter().map(|&i| texts[i]).collect();
            for (&i, v) in idx.iter().zip(self.embed_batch(&batch)?) {
                out[i] = v;
            }
        }
        Ok(out)
    }

    fn embed_batch(&self, batch: &[&str]) -> Result<Vec<Vec<f32>>, MlError> {
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        let capped: Vec<String> = batch
            .iter()
            .map(|t| t.chars().take(self.spec.max_chars).collect())
            .collect();
        let encodings = self
            .tokenizer
            .encode_batch(capped, true)
            .map_err(inference)?;
        let rows = encodings.len();
        let seq = encodings.first().map_or(0, |e| e.get_ids().len());
        let mut ids = Vec::with_capacity(rows * seq);
        let mut mask = Vec::with_capacity(rows * seq);
        for e in &encodings {
            ids.extend_from_slice(e.get_ids());
            mask.extend_from_slice(e.get_attention_mask());
        }
        let ids = Tensor::from_vec(ids, (rows, seq), &self.device).map_err(inference)?;
        let mask = Tensor::from_vec(mask, (rows, seq), &self.device).map_err(inference)?;
        let hidden = self.model.forward(&ids, &mask).map_err(inference)?;
        let pooled = match self.spec.pooling {
            Pooling::Cls => hidden.i((.., 0)).map_err(inference)?,
            Pooling::Mean => {
                let m = mask
                    .to_dtype(DType::F32)
                    .and_then(|m| m.unsqueeze(2))
                    .map_err(inference)?;
                let summed = hidden
                    .broadcast_mul(&m)
                    .and_then(|x| x.sum(1))
                    .map_err(inference)?;
                let counts = m
                    .sum(1)
                    .and_then(|c| c.clamp(1e-9f32, f32::MAX))
                    .map_err(inference)?;
                summed.broadcast_div(&counts).map_err(inference)?
            }
        };
        let norms = pooled
            .sqr()
            .and_then(|x| x.sum_keepdim(1))
            .and_then(|x| x.sqrt())
            .and_then(|x| x.clamp(1e-12f32, f32::MAX))
            .map_err(inference)?;
        let normalized = pooled.broadcast_div(&norms).map_err(inference)?;
        normalized.to_vec2::<f32>().map_err(inference)
    }
}

/// Models root: `RAGMONK_MODELS_DIR`, else `<home>/models`.
pub fn models_root(home_models: Option<&Path>) -> PathBuf {
    std::env::var_os("RAGMONK_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| home_models.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("models"))
}

/// Cosine similarity of two vectors (0 for mismatched or empty input).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}
