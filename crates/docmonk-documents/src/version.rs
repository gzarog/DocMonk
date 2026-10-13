//! Document derivation identity (the document version
//! stamp): what a stored file's stamp is compared
//! against to decide whether unchanged content needs reprocessing.

use crate::chunker::{chunker_version_stamp, EMBEDDING_TEXT_VERSION};

/// Folded into `parser_version` for `.eml` files while attachment
/// extraction is enabled.
pub const EMAIL_ATTACHMENTS_VERSION: &str = "eml-attachments.1";

/// Folded into the stored chunker identity (Rust only): chunks record
/// their enclosing heading (`parent_ordinal`) for search context
/// expansion. Builds without it are rebuilt.
pub const CHUNK_PARENTS_VERSION: &str = "chunk-parents.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentVersionStamp {
    pub parser_version: String,
    pub chunker_version: String,
    pub embedding_text_version: String,
}

/// `converter_parser_version` is the converter's own identity.
pub fn document_version_stamp(
    converter_parser_version: &str,
    email_attachments: bool,
) -> DocumentVersionStamp {
    let parser_version = if email_attachments {
        format!("{converter_parser_version}+{EMAIL_ATTACHMENTS_VERSION}")
    } else {
        converter_parser_version.to_owned()
    };
    DocumentVersionStamp {
        parser_version,
        chunker_version: chunker_version_stamp(),
        embedding_text_version: EMBEDDING_TEXT_VERSION.to_owned(),
    }
}
