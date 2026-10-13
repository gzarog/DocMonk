//! Exact, offline tokenizer for the pinned embedding model.
//!
//! The bundled `all-MiniLM-L6-v2` assets are compiled into the
//! binary and verified against the pinned SHA-256 manifest before first
//! use. They are loaded with the same Hugging Face `tokenizers` core
//! (0.23.2), with padding and truncation
//! disabled so counts are the true sequence length.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

pub const EMBEDDING_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub const TOKENIZER_REVISION: &str = "1110a243fdf4706b3f48f1d95db1a4f5529b4d41";
/// Effective input ceiling (sentence-transformers `max_seq_length`).
pub const MAX_SEQUENCE_TOKENS: i64 = 256;

pub const TOKENIZER_ASSET_MANIFEST: &[(&str, &str)] = &[
    (
        "config.json",
        "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
    ),
    (
        "sentence_bert_config.json",
        "fc1993fde0a95c24ec6c022539d41cf6e2f7c9721e5415d6fb6897472a9cd4b7",
    ),
    (
        "special_tokens_map.json",
        "303df45a03609e4ead04bc3dc1536d0ab19b5358db685b6f3da123d05ec200e3",
    ),
    (
        "tokenizer.json",
        "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
    ),
    (
        "tokenizer_config.json",
        "acb92769e8195aabd29b7b2137a9e6d6e25c476a4f15aa4355c233426c61576b",
    ),
    (
        "vocab.txt",
        "07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3",
    ),
];

macro_rules! asset {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!("../assets/all-MiniLM-L6-v2/", $name)) as &[u8],
        )
    };
}

const ASSETS: &[(&str, &[u8])] = &[
    asset!("config.json"),
    asset!("sentence_bert_config.json"),
    asset!("special_tokens_map.json"),
    asset!("tokenizer.json"),
    asset!("tokenizer_config.json"),
    asset!("vocab.txt"),
];

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct TokenizerAssetError(pub String);

/// `sha256:<hex>` over the sorted `name:sha256` manifest lines.
pub fn tokenizer_fingerprint() -> String {
    let mut lines: Vec<String> = TOKENIZER_ASSET_MANIFEST
        .iter()
        .map(|(n, d)| format!("{n}:{d}"))
        .collect();
    lines.sort();
    format!("sha256:{:x}", Sha256::digest(lines.join("\n").as_bytes()))
}

/// Revision, asset fingerprint and max length: the tokenizer's share of
/// the chunk-derivation identity.
pub fn preprocessing_fingerprint() -> String {
    format!(
        "tok:{TOKENIZER_REVISION}:{}:max{MAX_SEQUENCE_TOKENS}",
        tokenizer_fingerprint()
    )
}

fn verify_assets() -> Result<(), TokenizerAssetError> {
    for (name, expected) in TOKENIZER_ASSET_MANIFEST {
        let Some((_, data)) = ASSETS.iter().find(|(n, _)| n == name) else {
            return Err(TokenizerAssetError(format!(
                "bundled tokenizer asset {name:?} is missing -- the RagMonk build is incomplete"
            )));
        };
        let actual = format!("{:x}", Sha256::digest(data));
        if actual != *expected {
            return Err(TokenizerAssetError(format!(
                "bundled tokenizer asset {name:?} failed integrity verification \
                 (expected sha256 {expected}, got {actual})"
            )));
        }
    }
    Ok(())
}

/// Exact token counting and token-aware splitting for one model.
pub struct ModelTokenizer {
    inner: Tokenizer,
    special_tokens: i64,
}

impl std::fmt::Debug for ModelTokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelTokenizer")
            .field("model_id", &EMBEDDING_MODEL_ID)
            .finish()
    }
}

impl ModelTokenizer {
    fn load() -> Result<Self, TokenizerAssetError> {
        verify_assets()?;
        let json = ASSETS
            .iter()
            .find(|(n, _)| *n == "tokenizer.json")
            .map(|(_, d)| *d)
            .expect("verified");
        let mut inner = Tokenizer::from_bytes(json)
            .map_err(|e| TokenizerAssetError(format!("failed to load bundled tokenizer: {e}")))?;
        inner.with_padding(None);
        inner
            .with_truncation(None)
            .map_err(|e| TokenizerAssetError(format!("failed to disable truncation: {e}")))?;
        let mut t = Self {
            inner,
            special_tokens: 0,
        };
        t.special_tokens = t.count("", true);
        tracing::info!(
            component = "tokenizer",
            event = "tokenizer_loaded",
            model_id = EMBEDDING_MODEL_ID,
            revision = TOKENIZER_REVISION,
            max_sequence_tokens = MAX_SEQUENCE_TOKENS
        );
        Ok(t)
    }

    fn encode(&self, text: &str, special: bool) -> tokenizers::Encoding {
        self.inner
            .encode(text, special)
            .expect("WordPiece encoding of a str cannot fail")
    }

    /// Exact number of tokens the model sees (`add_special` adds
    /// `[CLS]`/`[SEP]`).
    pub fn count(&self, text: &str, add_special: bool) -> i64 {
        if text.is_empty() && !add_special {
            return 0;
        }
        self.encode(text, add_special).get_ids().len() as i64
    }

    /// Special tokens added around every sequence.
    pub fn special_tokens(&self) -> i64 {
        self.special_tokens
    }

    /// Splits at token offsets into slices of `text` whose re-encoded body
    /// length fits `budget` (a single oversized source token still
    /// advances).
    pub fn split(&self, text: &str, budget: i64) -> Vec<String> {
        assert!(budget > 0, "split budget must be positive, got {budget}");
        if text.is_empty() {
            return Vec::new();
        }
        let enc = self.encode(text, false);
        let n = enc.get_ids().len();
        if n as i64 <= budget {
            return vec![text.to_owned()];
        }
        let offsets = enc.get_offsets();
        let budget = budget as usize;
        let mut pieces = Vec::new();
        let mut start = 0usize;
        while start < n {
            let mut end = (start + budget).min(n);
            let char_start = offsets[start].0;
            while end > start + 1 {
                let candidate = &text[char_start..offsets[end - 1].1];
                if self.count(candidate, false) <= budget as i64 {
                    break;
                }
                end -= 1;
            }
            let piece = &text[char_start..offsets[end - 1].1];
            if !piece.is_empty() {
                pieces.push(piece.to_owned());
            }
            start = end;
        }
        pieces
    }
}

/// The process-wide tokenizer, verified and loaded on first use.
pub fn model_tokenizer() -> Result<&'static ModelTokenizer, TokenizerAssetError> {
    static T: OnceLock<Result<ModelTokenizer, TokenizerAssetError>> = OnceLock::new();
    T.get_or_init(ModelTokenizer::load)
        .as_ref()
        .map_err(Clone::clone)
}
