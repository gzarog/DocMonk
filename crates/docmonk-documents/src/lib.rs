//! Document core: the canonical normalized-document model, the
//! exact pinned tokenizer, token-budget splitting, row-aware tables and the
//! payload-aware chunker. Conversion lives in `docmonk-convert`; the
//! chunker consumes the canonical intermediate JSON only.

pub mod chunker;
pub mod model;
pub mod table;
pub mod tokenization;
pub mod tokenizer;
pub mod version;
