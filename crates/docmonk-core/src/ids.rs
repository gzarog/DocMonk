//! Stable identifier algorithms.
//!
//! * source IDs: `src_` + sha256(canonical path)[:10] (`sources.registry`);
//! * project IDs: sha256(resolved path)[:12] (`core.paths`, see
//!   [`crate::paths::project_id_for_path`]);
//! * file/document/chunk/entity/relationship/link/build IDs: [`record`],
//!   shared by local and server storage.

use sha2::{Digest, Sha256};

use crate::models::SourceType;

const SEP: &str = "\x1f";

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

pub fn make_source_id(canonical_path: &str) -> String {
    format!("src_{}", &sha256_hex(canonical_path.as_bytes())[..10])
}

const NETWORK_PREFIXES: &[&str] = &["\\\\", "//", "smb://", "nfs://", "afp://"];

/// `detect_source_type` on the *raw* user-supplied path.
pub fn detect_source_type(raw_path: &str) -> SourceType {
    if NETWORK_PREFIXES.iter().any(|p| raw_path.starts_with(p)) && !raw_path.starts_with("///") {
        SourceType::Network
    } else {
        SourceType::Local
    }
}

/// Deterministic record identifiers: each is the first 32 hex chars of
/// sha256 over a domain tag plus `\x1f`-joined natural-key parts, so the
/// same logical record always gets the same ID (idempotent re-indexing) and
/// different record kinds can never collide.
pub mod record {
    use super::{sha256_hex, SEP};

    fn id(domain: &str, parts: &[&str]) -> String {
        let mut joined = String::from(domain);
        for part in parts {
            joined.push_str(SEP);
            joined.push_str(part);
        }
        sha256_hex(joined.as_bytes())[..32].to_owned()
    }

    /// A file is identified by its source and path relative to the source root
    /// (normalized to `/` separators by the caller).
    pub fn file_id(source_id: &str, rel_path: &str) -> String {
        id("ragmonk:file", &[source_id, rel_path])
    }

    /// Top-level document of a file, or an EML attachment child document
    /// keyed by its stable MIME ordinal.
    pub fn document_id(file_id: &str, attachment_index: Option<u64>) -> String {
        match attachment_index {
            None => id("ragmonk:document", &[file_id]),
            Some(i) => id("ragmonk:document", &[file_id, "attachment", &i.to_string()]),
        }
    }

    pub fn chunk_id(document_id: &str, ordinal: u64) -> String {
        id("ragmonk:chunk", &[document_id, &ordinal.to_string()])
    }

    /// `ordinal` disambiguates same-named overloads within one file.
    pub fn entity_id(file_id: &str, kind: &str, qualified_name: &str, ordinal: u64) -> String {
        id(
            "ragmonk:entity",
            &[file_id, kind, qualified_name, &ordinal.to_string()],
        )
    }

    pub fn relationship_id(
        source_entity_id: &str,
        relationship_type: &str,
        target: &str,
        ordinal: u64,
    ) -> String {
        id(
            "ragmonk:relationship",
            &[
                source_entity_id,
                relationship_type,
                target,
                &ordinal.to_string(),
            ],
        )
    }

    pub fn link_id(
        entity_id: &str,
        document_id: &str,
        chunk_id: Option<&str>,
        link_type: &str,
        resolver: &str,
    ) -> String {
        id(
            "ragmonk:link",
            &[
                entity_id,
                document_id,
                chunk_id.unwrap_or(""),
                link_type,
                resolver,
            ],
        )
    }

    /// A build of one source; `nonce` makes each attempt distinct.
    pub fn build_id(source_id: &str, nonce: &str) -> String {
        id("ragmonk:build", &[source_id, nonce])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn deterministic_and_domain_separated() {
            let f = file_id("src_1", "a/b.py");
            assert_eq!(f, file_id("src_1", "a/b.py"));
            assert_eq!(f.len(), 32);
            assert_ne!(f, file_id("src_1", "a/b.pyx"));
            assert_ne!(document_id(&f, None), document_id(&f, Some(0)));
            assert_ne!(document_id(&f, Some(1)), document_id(&f, Some(2)));
            // Same parts under different domains never collide.
            assert_ne!(id("ragmonk:file", &["x"]), id("ragmonk:document", &["x"]));
            assert_ne!(
                link_id("e", "d", None, "documented_by", "exact"),
                link_id("e", "d", Some(""), "documented_by", "x")
            );
        }
    }
}
