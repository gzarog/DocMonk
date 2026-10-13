//! Pinned model assets. Every model is identified by its Hugging Face id,
//! an exact revision and the sha256 of every file the runtime reads; the
//! fingerprint derived from all of it is stamped on each stored vector.

use sha2::{Digest, Sha256};

/// How token states are reduced to one vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Attention-masked mean (sentence-transformers MiniLM).
    Mean,
    /// The `[CLS]` token state (BGE).
    Cls,
}

/// Version of the text preprocessing contract (character cap, truncation,
/// padding, pooling, normalization). Bump on any behavior change.
pub const PREPROCESSING_VERSION: &str = "1";

/// Text-assembly version for code entities (`signature`, else
/// `qualified_name`).
pub const CODE_EMBEDDING_TEXT_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingModelSpec {
    /// Directory name under the models root.
    pub slug: &'static str,
    pub hf_id: &'static str,
    pub revision: &'static str,
    pub dims: usize,
    pub max_tokens: usize,
    /// Defensive pre-tokenizer cap in characters.
    pub max_chars: usize,
    pub pooling: Pooling,
    pub files: &'static [ModelFile],
}

impl EmbeddingModelSpec {
    /// Stable identity of model + assets + preprocessing (16 hex chars).
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for part in [
            self.hf_id,
            self.revision,
            PREPROCESSING_VERSION,
            &self.dims.to_string(),
            &self.max_tokens.to_string(),
            &self.max_chars.to_string(),
            match self.pooling {
                Pooling::Mean => "mean",
                Pooling::Cls => "cls",
            },
        ] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        for f in self.files {
            h.update(f.name.as_bytes());
            h.update([0]);
            h.update(f.sha256.as_bytes());
            h.update([0]);
        }
        let digest = h.finalize();
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    /// Download URL of one pinned file.
    pub fn url(&self, file: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.hf_id, self.revision, file.name
        )
    }
}

/// `sentence-transformers/all-MiniLM-L6-v2`, the default
/// model until the selection benchmark picks otherwise.
pub const ALL_MINILM_L6_V2: EmbeddingModelSpec = EmbeddingModelSpec {
    slug: "all-minilm-l6-v2",
    hf_id: "sentence-transformers/all-MiniLM-L6-v2",
    revision: "1110a243fdf4706b3f48f1d95db1a4f5529b4d41",
    dims: 384,
    max_tokens: 256,
    max_chars: 4000,
    pooling: Pooling::Mean,
    files: &[
        ModelFile {
            name: "config.json",
            sha256: "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
        },
        ModelFile {
            name: "model.safetensors",
            sha256: "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
        },
        ModelFile {
            name: "tokenizer.json",
            sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
        },
    ],
};

/// The model RagMonk embeds with.
pub const DEFAULT_EMBEDDING_MODEL: EmbeddingModelSpec = ALL_MINILM_L6_V2;

/// A pinned cross-encoder (`BertForSequenceClassification` with one
/// relevance logit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RerankerModelSpec {
    pub slug: &'static str,
    pub hf_id: &'static str,
    pub revision: &'static str,
    /// Pair length cap (query + passage), longest-first truncation.
    pub max_tokens: usize,
    /// Defensive pre-tokenizer cap on the passage, in characters.
    pub max_chars: usize,
    pub files: &'static [ModelFile],
}

impl RerankerModelSpec {
    pub fn url(&self, file: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.hf_id, self.revision, file.name
        )
    }
}

/// `cross-encoder/ms-marco-MiniLM-L-6-v2`, the default reranker.
pub const MS_MARCO_MINILM_L6_V2: RerankerModelSpec = RerankerModelSpec {
    slug: "ms-marco-minilm-l6-v2",
    hf_id: "cross-encoder/ms-marco-MiniLM-L-6-v2",
    revision: "233902d25c440f23af6f7d6e94d2946bac0bee0a",
    max_tokens: 256,
    max_chars: 4000,
    files: &[
        ModelFile {
            name: "config.json",
            sha256: "380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc",
        },
        ModelFile {
            name: "model.safetensors",
            sha256: "821d1aa69520101d6e0737f78a042ae25b19e5cb9160701909d10434f4aeb0ae",
        },
        ModelFile {
            name: "tokenizer.json",
            sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        },
    ],
};

/// The reranker RagMonk uses.
pub const DEFAULT_RERANKER_MODEL: RerankerModelSpec = MS_MARCO_MINILM_L6_V2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let a = ALL_MINILM_L6_V2.fingerprint();
        assert_eq!(a.len(), 16);
        assert_eq!(a, ALL_MINILM_L6_V2.fingerprint());
        let mut other = ALL_MINILM_L6_V2;
        other.max_tokens = 128;
        assert_ne!(a, other.fingerprint());
        let mut other = ALL_MINILM_L6_V2;
        other.revision = "main";
        assert_ne!(a, other.fingerprint());
    }

    #[test]
    fn urls_pin_the_revision() {
        let f = &ALL_MINILM_L6_V2.files[1];
        assert_eq!(
            ALL_MINILM_L6_V2.url(f),
            "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/\
             1110a243fdf4706b3f48f1d95db1a4f5529b4d41/model.safetensors"
        );
    }
}
