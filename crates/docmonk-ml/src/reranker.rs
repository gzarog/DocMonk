//! Cross-encoder reranking.
//!
//! A cross-encoder scores each `(query, passage)` pair jointly:
//! - BERT runs over `[CLS] query [SEP] passage [SEP]`, with token types
//!   0 for the query segment and 1 for the passage.
//! - The pooler (dense + tanh on `[CLS]`) and a one-logit classifier
//!   produce the raw relevance logit. No activation
//!   is applied to it.
//!
//! Only a bounded prefix of the candidate list is rescored. If the model
//! is unavailable, the input order is kept: an optional reranking pass
//! never turns into a search failure.

use std::path::Path;
use std::sync::Mutex;

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{Linear, Module, VarBuilder};
use tokenizers::{
    EncodeInput, PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy,
};

use crate::bert::{Bert, BertConfig};
use crate::embedder::{inference, load_err, read_verified, MlError};
use crate::manifest::RerankerModelSpec;

/// Pairs scored per forward pass.
pub const BATCH_SIZE: usize = 16;

pub struct CrossEncoder {
    spec: RerankerModelSpec,
    tokenizer: Tokenizer,
    model: Bert,
    pooler: Linear,
    classifier: Linear,
    device: Device,
    gate: Mutex<()>,
}

impl std::fmt::Debug for CrossEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossEncoder")
            .field("model", &self.spec.hf_id)
            .finish()
    }
}

impl CrossEncoder {
    /// Loads `spec` from `dir`, verifying every pinned file first.
    pub fn load(dir: &Path, spec: RerankerModelSpec) -> Result<Self, MlError> {
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
                strategy: TruncationStrategy::LongestFirst,
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
        let h = config.hidden_size;
        let model = Bert::load(vb.clone(), &config).map_err(load_err)?;
        let pooler = candle_nn::linear(h, h, vb.pp("bert.pooler.dense")).map_err(load_err)?;
        let classifier = candle_nn::linear(h, 1, vb.pp("classifier")).map_err(load_err)?;
        Ok(Self {
            spec,
            tokenizer,
            model,
            pooler,
            classifier,
            device,
            gate: Mutex::new(()),
        })
    }

    pub fn spec(&self) -> &RerankerModelSpec {
        &self.spec
    }

    /// One relevance logit per passage against `query`, in input order.
    ///
    /// Passages are batched in order of length so each batch pads to a
    /// similar length (padding is masked out, so a score does not depend on
    /// its batch).
    pub fn score(&self, query: &str, passages: &[&str]) -> Result<Vec<f32>, MlError> {
        let mut order: Vec<usize> = (0..passages.len()).collect();
        order.sort_by_key(|&i| passages[i].len());
        let mut out = vec![0f32; passages.len()];
        for idx in order.chunks(BATCH_SIZE) {
            let batch: Vec<&str> = idx.iter().map(|&i| passages[i]).collect();
            for (&i, s) in idx.iter().zip(self.score_batch(query, &batch)?) {
                out[i] = s;
            }
        }
        Ok(out)
    }

    fn score_batch(&self, query: &str, batch: &[&str]) -> Result<Vec<f32>, MlError> {
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        let inputs: Vec<EncodeInput> = batch
            .iter()
            .map(|p| {
                let capped: String = p.chars().take(self.spec.max_chars).collect();
                EncodeInput::Dual(query.to_owned().into(), capped.into())
            })
            .collect();
        let enc = self
            .tokenizer
            .encode_batch(inputs, true)
            .map_err(inference)?;
        let rows = enc.len();
        let seq = enc.first().map_or(0, |e| e.get_ids().len());
        let (mut ids, mut types, mut mask) = (
            Vec::with_capacity(rows * seq),
            Vec::with_capacity(rows * seq),
            Vec::with_capacity(rows * seq),
        );
        for e in &enc {
            ids.extend_from_slice(e.get_ids());
            types.extend_from_slice(e.get_type_ids());
            mask.extend_from_slice(e.get_attention_mask());
        }
        let t = |v: Vec<u32>| Tensor::from_vec(v, (rows, seq), &self.device).map_err(inference);
        let (ids, types, mask) = (t(ids)?, t(types)?, t(mask)?);
        let hidden = self
            .model
            .forward_typed(&ids, Some(&types), &mask)
            .map_err(inference)?;
        let cls = hidden.i((.., 0)).map_err(inference)?;
        let pooled = self
            .pooler
            .forward(&cls)
            .and_then(|x| x.tanh())
            .map_err(inference)?;
        let logits = self.classifier.forward(&pooled).map_err(inference)?;
        logits
            .squeeze(1)
            .and_then(|x| x.to_vec1::<f32>())
            .map_err(inference)
    }
}

/// Reorders `items[..top_n]` by descending score (stable on ties), keeping
/// the remainder after it in its original order.
///
/// Returns `items` unchanged, with `None`, when there is nothing worth
/// rescoring (`top_n == 0` or fewer than two items) or when scoring fails.
/// The latter covers a missing model or a score-count mismatch.
pub fn rerank<T>(
    items: Vec<T>,
    top_n: usize,
    text: impl Fn(&T) -> &str,
    score: impl FnOnce(&[&str]) -> Result<Vec<f32>, MlError>,
) -> (Vec<T>, Option<Vec<f32>>) {
    if top_n == 0 || items.len() < 2 {
        return (items, None);
    }
    let n = top_n.min(items.len());
    let texts: Vec<&str> = items[..n].iter().map(&text).collect();
    let scores = match score(&texts) {
        Ok(s) if s.len() == n => s,
        Ok(s) => {
            tracing::info!(
                component = "reranker",
                event = "score_count_mismatch",
                expected = n,
                got = s.len()
            );
            return (items, None);
        }
        Err(e) => {
            tracing::info!(component = "reranker", event = "unavailable", reason = %e);
            return (items, None);
        }
    };
    let mut items = items;
    let rest = items.split_off(n);
    let mut head: Vec<(f32, T)> = scores.iter().copied().zip(items).collect();
    head.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (sorted_scores, mut out): (Vec<f32>, Vec<T>) = head.into_iter().unzip();
    out.extend(rest);
    (out, Some(sorted_scores))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorders_only_the_prefix_and_is_stable() {
        let items = vec!["a", "b", "c", "d", "e"];
        let (out, scores) = rerank(items, 3, |s| s, |_| Ok(vec![0.1, 0.9, 0.1]));
        assert_eq!(out, ["b", "a", "c", "d", "e"]);
        assert_eq!(scores.unwrap(), [0.9, 0.1, 0.1]);
    }

    #[test]
    fn falls_back_to_input_order() {
        let items = vec!["a", "b", "c"];
        let (out, s) = rerank(
            items.clone(),
            3,
            |s| s,
            |_| Err(MlError::Load("no model".into())),
        );
        assert_eq!((out, s), (items.clone(), None));
        let (out, s) = rerank(items.clone(), 3, |s| s, |_| Ok(vec![1.0]));
        assert_eq!((out, s), (items.clone(), None));
        let (out, s) = rerank(items.clone(), 0, |s| s, |_| unreachable!());
        assert_eq!((out, s), (items, None));
        let (out, _) = rerank(vec!["x"], 5, |s| s, |_| unreachable!());
        assert_eq!(out, ["x"]);
    }
}
