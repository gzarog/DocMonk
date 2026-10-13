//! A lean BERT encoder for CPU inference.
//!
//! Numerically it is the standard BERT forward pass (post-norm layers,
//! exact-erf GELU, `(1 - mask) * f32::MIN` attention bias), restructured
//! for Candle's CPU backend: activations stay 2-D `(tokens, hidden)` so
//! every projection is one contiguous GEMM with a pre-transposed weight,
//! Q/K/V are fused into a single projection, layer norm and softmax use
//! Candle's fused kernels, and GELU runs in parallel.

use candle_core::{CpuStorage, CustomOp1, Layout, Result, Shape, Tensor};
use candle_nn::VarBuilder;
use rayon::prelude::*;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct BertConfig {
    pub hidden_size: usize,
    pub num_attention_heads: usize,
    pub num_hidden_layers: usize,
    pub intermediate_size: usize,
    #[serde(default = "default_eps")]
    pub layer_norm_eps: f64,
    pub max_position_embeddings: usize,
    #[serde(default = "default_types")]
    pub type_vocab_size: usize,
    pub vocab_size: usize,
    #[serde(default = "default_act")]
    pub hidden_act: String,
}

fn default_eps() -> f64 {
    1e-12
}
fn default_types() -> usize {
    2
}
fn default_act() -> String {
    "gelu".into()
}

/// `y = x · W + b` with `W` stored transposed `(in, out)` and contiguous.
struct Dense {
    w: Tensor,
    b: Tensor,
    bias: Vec<f32>,
}

impl Dense {
    fn load(vb: VarBuilder, input: usize, output: usize) -> Result<Self> {
        let w = vb.get((output, input), "weight")?.t()?.contiguous()?;
        let b = vb.get(output, "bias")?;
        let bias = b.to_vec1::<f32>()?;
        Ok(Self { w, b, bias })
    }

    fn fused(parts: &[Dense]) -> Result<Self> {
        let ws: Vec<&Tensor> = parts.iter().map(|d| &d.w).collect();
        let bs: Vec<&Tensor> = parts.iter().map(|d| &d.b).collect();
        let b = Tensor::cat(&bs, 0)?;
        Ok(Self {
            w: Tensor::cat(&ws, 1)?.contiguous()?,
            bias: b.to_vec1::<f32>()?,
            b,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        x.matmul(&self.w)?.apply_op1_no_bwd(&RowBias {
            bias: &self.bias,
            gelu: false,
        })
    }

    /// `gelu(x · W + b)` with the bias add and activation fused.
    fn forward_gelu(&self, x: &Tensor) -> Result<Tensor> {
        x.matmul(&self.w)?.apply_op1_no_bwd(&RowBias {
            bias: &self.bias,
            gelu: true,
        })
    }
}

struct Norm {
    w: Tensor,
    b: Tensor,
    eps: f32,
}

impl Norm {
    fn load(vb: VarBuilder, size: usize, eps: f64) -> Result<Self> {
        Ok(Self {
            w: vb.get(size, "weight")?,
            b: vb.get(size, "bias")?,
            eps: eps as f32,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        candle_nn::ops::layer_norm(x, &self.w, &self.b, self.eps)
    }
}

struct Layer {
    qkv: Dense,
    attn_out: Dense,
    attn_norm: Norm,
    inter: Dense,
    out: Dense,
    out_norm: Norm,
}

pub struct Bert {
    word: Tensor,
    position: Tensor,
    token_type: Tensor,
    emb_norm: Norm,
    layers: Vec<Layer>,
    heads: usize,
    head_dim: usize,
    hidden: usize,
}

fn contiguous_f32<'a>(storage: &'a CpuStorage, layout: &Layout) -> Result<&'a [f32]> {
    let (start, end) = layout
        .contiguous_offsets()
        .ok_or_else(|| candle_core::Error::Msg("input must be contiguous".into()))?;
    Ok(&storage.as_slice::<f32>()?[start..end])
}

fn gelu_erf(v: f32) -> f32 {
    let x = f64::from(v);
    ((candle_core::cpu::erf::erf(x * std::f64::consts::FRAC_1_SQRT_2) + 1.0) * 0.5 * x) as f32
}

/// Adds a per-column bias to every row (optionally followed by exact-erf
/// GELU), in parallel over rows.
struct RowBias<'a> {
    bias: &'a [f32],
    gelu: bool,
}

impl CustomOp1 for RowBias<'_> {
    fn name(&self) -> &'static str {
        "ragmonk-row-bias"
    }

    fn cpu_fwd(&self, storage: &CpuStorage, layout: &Layout) -> Result<(CpuStorage, Shape)> {
        let src = contiguous_f32(storage, layout)?;
        let cols = self.bias.len();
        if cols == 0 || src.len() % cols != 0 {
            candle_core::bail!("bias width {cols} does not divide {}", src.len());
        }
        let mut dst = vec![0f32; src.len()];
        dst.par_chunks_mut(cols)
            .zip(src.par_chunks(cols))
            .for_each(|(d, s)| {
                for ((o, &v), &b) in d.iter_mut().zip(s).zip(self.bias) {
                    let x = v + b;
                    *o = if self.gelu { gelu_erf(x) } else { x };
                }
            });
        Ok((CpuStorage::F32(dst), layout.shape().clone()))
    }
}

/// Softmax over the last dim of `(batch, heads, seq, seq)` scores with the
/// additive padding bias (`f32::MIN` on masked keys), in
/// parallel over rows.
struct MaskedSoftmax<'a> {
    /// `(batch, seq)` additive bias.
    bias: &'a [f32],
    heads: usize,
    seq: usize,
}

impl CustomOp1 for MaskedSoftmax<'_> {
    fn name(&self) -> &'static str {
        "ragmonk-masked-softmax"
    }

    fn cpu_fwd(&self, storage: &CpuStorage, layout: &Layout) -> Result<(CpuStorage, Shape)> {
        let src = contiguous_f32(storage, layout)?;
        let s = self.seq;
        let per_batch = self.heads * s * s;
        if s == 0 || src.len() != per_batch * (self.bias.len() / s) {
            candle_core::bail!("masked softmax shape mismatch");
        }
        let mut dst = vec![0f32; src.len()];
        dst.par_chunks_mut(s)
            .zip(src.par_chunks(s))
            .enumerate()
            .for_each(|(row, (d, x))| {
                let bias = &self.bias[(row * s / per_batch) * s..][..s];
                let mut max = f32::NEG_INFINITY;
                for ((o, &v), &m) in d.iter_mut().zip(x).zip(bias) {
                    *o = v + m;
                    max = max.max(*o);
                }
                let mut sum = 0f32;
                for o in d.iter_mut() {
                    *o = (*o - max).exp();
                    sum += *o;
                }
                for o in d.iter_mut() {
                    *o /= sum;
                }
            });
        Ok((CpuStorage::F32(dst), layout.shape().clone()))
    }
}

impl Bert {
    pub fn load(vb: VarBuilder, cfg: &BertConfig) -> Result<Self> {
        if cfg.hidden_act != "gelu" {
            candle_core::bail!("unsupported hidden_act {}", cfg.hidden_act);
        }
        // sentence-transformers exports have no prefix; plain HF BERT uses
        // `bert.`.
        let vb = if vb.contains_tensor("embeddings.word_embeddings.weight") {
            vb
        } else {
            vb.pp("bert")
        };
        let h = cfg.hidden_size;
        let e = vb.pp("embeddings");
        let emb_norm = Norm::load(e.pp("LayerNorm"), h, cfg.layer_norm_eps)?;
        let mut layers = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            let l = vb.pp(format!("encoder.layer.{i}"));
            let a = l.pp("attention");
            let s = a.pp("self");
            let qkv = Dense::fused(&[
                Dense::load(s.pp("query"), h, h)?,
                Dense::load(s.pp("key"), h, h)?,
                Dense::load(s.pp("value"), h, h)?,
            ])?;
            layers.push(Layer {
                qkv,
                attn_out: Dense::load(a.pp("output.dense"), h, h)?,
                attn_norm: Norm::load(a.pp("output.LayerNorm"), h, cfg.layer_norm_eps)?,
                inter: Dense::load(l.pp("intermediate.dense"), h, cfg.intermediate_size)?,
                out: Dense::load(l.pp("output.dense"), cfg.intermediate_size, h)?,
                out_norm: Norm::load(l.pp("output.LayerNorm"), h, cfg.layer_norm_eps)?,
            });
        }
        Ok(Self {
            word: e.get((cfg.vocab_size, h), "word_embeddings.weight")?,
            position: e.get(
                (cfg.max_position_embeddings, h),
                "position_embeddings.weight",
            )?,
            token_type: e.get((cfg.type_vocab_size, h), "token_type_embeddings.weight")?,
            emb_norm,
            layers,
            heads: cfg.num_attention_heads,
            head_dim: h / cfg.num_attention_heads,
            hidden: h,
        })
    }

    /// `ids`/`mask`: `(batch, seq)` u32, all token types 0. Returns
    /// `(batch, seq, hidden)`.
    pub fn forward(&self, ids: &Tensor, mask: &Tensor) -> Result<Tensor> {
        self.forward_typed(ids, None, mask)
    }

    /// Like [`Bert::forward`] with explicit `(batch, seq)` u32 token types
    /// (segment ids of a sentence pair); `None` means all zeros.
    pub fn forward_typed(
        &self,
        ids: &Tensor,
        types: Option<&Tensor>,
        mask: &Tensor,
    ) -> Result<Tensor> {
        let (b, s) = ids.dims2()?;
        let n = b * s;
        let device = ids.device();
        let flat = ids.flatten_all()?;
        let positions = Tensor::arange(0u32, s as u32, device)?;
        let pos = self.position.index_select(&positions, 0)?; // (s, h)
        let x = self
            .word
            .index_select(&flat, 0)?
            .reshape((b, s, self.hidden))?
            .broadcast_add(&pos)?;
        let x = match types {
            Some(t) => {
                let tok = self
                    .token_type
                    .index_select(&t.flatten_all()?, 0)?
                    .reshape((b, s, self.hidden))?;
                (x + tok)?
            }
            None => x.broadcast_add(&self.token_type.get(0)?)?,
        }
        .reshape((n, self.hidden))?;
        let mut x = self.emb_norm.forward(&x)?;
        // (b, s) additive bias: 0 for tokens, f32::MIN for padding.
        let bias: Vec<f32> = mask
            .flatten_all()?
            .to_vec1::<u32>()?
            .into_iter()
            .map(|m| if m == 0 { f32::MIN } else { 0.0 })
            .collect();
        let scale = 1.0 / (self.head_dim as f64).sqrt();
        for layer in &self.layers {
            let qkv = layer.qkv.forward(&x)?; // (n, 3h)
            let split = |i: usize| -> Result<Tensor> {
                qkv.narrow(1, i * self.hidden, self.hidden)?
                    .reshape((b, s, self.heads, self.head_dim))?
                    .transpose(1, 2)?
                    .contiguous()
            };
            let (q, k, v) = (split(0)?, split(1)?, split(2)?);
            let scores = (q * scale)?.matmul(&k.t()?)?;
            let probs = scores.apply_op1_no_bwd(&MaskedSoftmax {
                bias: &bias,
                heads: self.heads,
                seq: s,
            })?;
            let ctx = probs
                .matmul(&v)? // (b, heads, s, d)
                .transpose(1, 2)?
                .contiguous()?
                .reshape((n, self.hidden))?;
            let attn = layer
                .attn_norm
                .forward(&(layer.attn_out.forward(&ctx)? + &x)?)?;
            let inter = layer.inter.forward_gelu(&attn)?;
            x = layer
                .out_norm
                .forward(&(layer.out.forward(&inter)? + &attn)?)?;
        }
        x.reshape((b, s, self.hidden))
    }
}
