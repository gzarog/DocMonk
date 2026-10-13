//! The server index schema manifest.
//!
//! Seven specialized indexes (no multiplexing of unrelated record kinds),
//! `dynamic: strict` mappings, only query/filter/sort fields indexed (bulky
//! text is stored with `index: false`), and an `_meta` block carrying the
//! schema identity so an incompatible existing index is detected instead of
//! being mutated in place.

use serde_json::{json, Map, Value};

use crate::engine::{Engine, VectorSpec};

/// Mapping identity, bumped on any mapping change. Existing indexes with
/// another identity are refused: the operator uses a fresh prefix/cluster or
/// resets the current indexes; nothing is ever mutated in place.
pub const SCHEMA_VERSION: u64 = 3;
pub const SCHEMA_TAG: &str = "ragmonk";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexKind {
    SourceState,
    Files,
    Code,
    Documents,
    Chunks,
    Relationships,
    /// Live indexing runs: one heartbeat document per source, written by
    /// whichever host holds the source's writer lease.
    Runtime,
}

impl IndexKind {
    pub const ALL: [IndexKind; 7] = [
        IndexKind::SourceState,
        IndexKind::Files,
        IndexKind::Code,
        IndexKind::Documents,
        IndexKind::Chunks,
        IndexKind::Relationships,
        IndexKind::Runtime,
    ];

    /// Indexes holding build-scoped, index-derived records.
    pub const BUILD_SCOPED: [IndexKind; 5] = [
        IndexKind::Files,
        IndexKind::Code,
        IndexKind::Documents,
        IndexKind::Chunks,
        IndexKind::Relationships,
    ];

    pub fn suffix(self) -> &'static str {
        match self {
            IndexKind::SourceState => "source-state",
            IndexKind::Files => "files",
            IndexKind::Code => "code",
            IndexKind::Documents => "documents",
            IndexKind::Chunks => "chunks",
            IndexKind::Relationships => "relationships",
            IndexKind::Runtime => "runtime",
        }
    }
}

/// `{prefix}-{kind}`, e.g. `ragmonk-chunks` for the default config.
pub fn index_name(prefix: &str, kind: IndexKind) -> String {
    format!("{prefix}-{}", kind.suffix())
}

fn kw() -> Value {
    json!({ "type": "keyword" })
}
fn stored() -> Value {
    json!({ "type": "keyword", "index": false, "doc_values": false })
}
fn stored_text() -> Value {
    json!({ "type": "text", "index": false })
}
fn text_kw() -> Value {
    json!({ "type": "text", "fields": { "raw": { "type": "keyword", "ignore_above": 1024 } } })
}
fn int() -> Value {
    json!({ "type": "integer" })
}
fn long() -> Value {
    json!({ "type": "long" })
}
fn date() -> Value {
    json!({ "type": "date" })
}
fn double() -> Value {
    json!({ "type": "double" })
}
fn boolean() -> Value {
    json!({ "type": "boolean" })
}
fn opaque() -> Value {
    json!({ "type": "object", "enabled": false })
}

fn props(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// Fields every build-scoped record carries.
fn scoped() -> Vec<(&'static str, Value)> {
    vec![
        ("source_id", kw()),
        ("build_id", kw()),
        ("file_id", kw()),
        ("rel_path", kw()),
    ]
}

fn properties(kind: IndexKind, engine: Engine, vector: Option<&VectorSpec>) -> Map<String, Value> {
    let mut fields: Vec<(&str, Value)> = match kind {
        // The authoritative source catalog and build lifecycle in server
        // mode: registration, filters, online state, the active/pending
        // build, the writer lease (fencing token) and retired builds kept
        // readable for a grace period after publication.
        IndexKind::SourceState => vec![
            ("source_id", kw()),
            ("path", kw()),
            ("source_type", kw()),
            ("enabled", boolean()),
            ("include_patterns", kw()),
            ("exclude_patterns", kw()),
            ("created_at", date()),
            ("state", kw()),
            ("rebuild_reason", stored_text()),
            ("online_status", kw()),
            ("active_build_id", kw()),
            ("pending_build_id", kw()),
            ("versions", opaque()),
            ("last_full_build_at", date()),
            ("last_scan_at", date()),
            ("last_error", stored_text()),
            ("lease_owner", kw()),
            ("lease_token", long()),
            ("lease_expires_at", date()),
            ("retired_builds", opaque()),
            ("content_digest", kw()),
            ("published_at", date()),
            ("updated_at", date()),
        ],
        IndexKind::Files => {
            let mut f = scoped();
            f.extend([
                ("path_text", json!({ "type": "text" })),
                ("kind", kw()),
                ("size", long()),
                ("mtime", json!({ "type": "double" })),
                ("content_hash", kw()),
                ("parser_version", kw()),
                ("chunker_version", kw()),
                ("converter_version", kw()),
                ("embedding_model_id", kw()),
                ("embedding_text_version", kw()),
                ("status", kw()),
                ("attempt_count", int()),
                ("last_error", stored_text()),
                ("last_error_at", date()),
                ("next_attempt_at", kw()),
                ("knowledge_digest", kw()),
                ("indexed_at", date()),
            ]);
            f
        }
        IndexKind::Code => {
            let mut f = scoped();
            f.extend([
                ("entity_id", kw()),
                ("kind", kw()),
                ("name", text_kw()),
                ("qualified_name", text_kw()),
                ("language", kw()),
                ("parent_id", kw()),
                ("signature", json!({ "type": "text" })),
                ("start_line", int()),
                ("end_line", int()),
                ("start_col", int()),
                ("end_col", int()),
                ("mtime", double()),
                ("embedding_fingerprint", kw()),
            ]);
            if let Some(spec) = vector {
                f.push(("embedding", engine.vector_field(spec)));
            }
            f
        }
        IndexKind::Documents => {
            let mut f = scoped();
            f.extend([
                ("document_id", kw()),
                ("format", kw()),
                ("title", text_kw()),
                ("author", kw()),
                ("page_count", int()),
                ("is_scanned", json!({ "type": "boolean" })),
                ("content_hash", kw()),
                ("parent_document_id", kw()),
                ("attachment_name", text_kw()),
                ("attachment_content_type", kw()),
                ("attachment_index", int()),
                ("attachment_content_id", stored()),
                ("parent_title", stored_text()),
                ("mtime", double()),
            ]);
            f
        }
        IndexKind::Chunks => {
            let mut f = scoped();
            f.extend([
                ("document_id", kw()),
                ("parent_document_id", kw()),
                ("chunk_id", kw()),
                ("kind", kw()),
                ("ordinal", int()),
                ("heading_path", text_kw()),
                ("heading_level", int()),
                ("title", json!({ "type": "text" })),
                ("search_text", json!({ "type": "text" })),
                ("text", stored_text()),
                ("embedding_text", stored_text()),
                ("token_count", int()),
                ("page_start", int()),
                ("page_end", int()),
                ("table_rows", opaque()),
                ("caption", json!({ "type": "text" })),
                ("parent_ordinal", int()),
                ("fts_heading", text_kw()),
                ("document_format", kw()),
                ("attachment_name", stored()),
                ("attachment_content_type", stored()),
                ("attachment_index", int()),
                ("parent_title", stored_text()),
                ("mtime", double()),
                ("embedding_fingerprint", kw()),
            ]);
            if let Some(spec) = vector {
                f.push(("embedding", engine.vector_field(spec)));
            }
            f
        }
        IndexKind::Relationships => {
            let mut f = scoped();
            f.extend([
                ("record_kind", kw()),
                ("relationship_id", kw()),
                ("relationship_type", kw()),
                ("source_entity_id", kw()),
                ("target_entity_id", kw()),
                ("target_symbol", kw()),
                ("resolver", kw()),
                ("confidence", kw()),
                ("source_location", stored()),
                ("evidence", stored_text()),
                ("entity_id", kw()),
                ("document_id", kw()),
                ("chunk_id", kw()),
                ("reference_text", stored_text()),
                ("link_type", kw()),
                ("manual_link_id", kw()),
                ("chunk_ordinal", int()),
                ("created_at", date()),
            ]);
            f
        }
        // Fast-changing run progress lives here, never in the source-state
        // compare-and-swap document. `lease_token` fences writes: a run that
        // lost the source's lease cannot overwrite a newer run's document.
        IndexKind::Runtime => vec![
            ("source_id", kw()),
            ("run_id", kw()),
            ("host", kw()),
            ("owner", kw()),
            ("lease_token", long()),
            ("operation", kw()),
            ("stage", kw()),
            ("active", boolean()),
            ("outcome", kw()),
            ("scanned", long()),
            ("planned", long()),
            ("processed", long()),
            ("indexed", long()),
            ("failed", long()),
            ("retry", long()),
            ("error", stored_text()),
            ("started_at", date()),
            ("last_progress_at", date()),
            ("heartbeat_at", date()),
            ("expires_at", date()),
            ("finished_at", date()),
            ("updated_at", date()),
        ],
    };
    fields.sort_by(|a, b| a.0.cmp(b.0));
    props(&fields)
}

/// Settings applied at creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSettings {
    pub shards: u32,
    pub replicas: u32,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            shards: 1,
            replicas: 1,
        }
    }
}

/// The `_meta` identity stored on every RagMonk index.
pub fn meta(kind: IndexKind, vector: Option<&VectorSpec>) -> Value {
    json!({
        "schema": SCHEMA_TAG,
        "schema_version": SCHEMA_VERSION,
        "index_kind": kind,
        "vector": if matches!(kind, IndexKind::Chunks | IndexKind::Code) { json!(vector) } else { Value::Null },
    })
}

/// Full create-index body.
pub fn create_body(
    kind: IndexKind,
    engine: Engine,
    vector: Option<&VectorSpec>,
    settings: &IndexSettings,
) -> Value {
    let mut index_settings = json!({
        "number_of_shards": settings.shards,
        "number_of_replicas": settings.replicas,
    });
    if matches!(kind, IndexKind::Chunks | IndexKind::Code) && vector.is_some() {
        if let (Some(dst), Some(extra)) = (
            index_settings.as_object_mut(),
            engine.vector_index_settings().as_object().cloned(),
        ) {
            dst.extend(extra);
        }
    }
    json!({
        "settings": { "index": index_settings },
        "mappings": {
            "dynamic": "strict",
            "_meta": meta(kind, vector),
            "properties": properties(kind, engine, vector),
        }
    })
}

/// Checks an existing index's `_meta` against the expected identity.
pub fn check_meta(
    index: &str,
    actual: Option<&Value>,
    kind: IndexKind,
    vector: Option<&VectorSpec>,
) -> Result<(), String> {
    let expected = meta(kind, vector);
    match actual {
        Some(m) if m == &expected => Ok(()),
        Some(m) => Err(format!(
            "index {index} has schema metadata {m} but this RagMonk expects {expected}; \
             use a fresh index_prefix (or cluster) or reset the current RagMonk indexes and \
             reindex (indexes are never converted in place)"
        )),
        None => Err(format!(
            "index {index} exists but is not a RagMonk index; refusing to use or modify it. \
             Choose another index_prefix"
        )),
    }
}

/// Machine-readable manifest of the whole schema (for docs/diagnostics).
pub fn manifest(prefix: &str, engine: Engine, vector: Option<&VectorSpec>) -> Value {
    let indexes: Vec<Value> = IndexKind::ALL
        .iter()
        .map(|k| {
            json!({
                "name": index_name(prefix, *k),
                "kind": k,
                "body": create_body(*k, engine, vector, &IndexSettings::default()),
            })
        })
        .collect();
    json!({ "schema": SCHEMA_TAG, "schema_version": SCHEMA_VERSION, "engine": engine, "indexes": indexes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> VectorSpec {
        VectorSpec {
            dims: 384,
            m: 16,
            ef_construction: 128,
            model_id: "m".into(),
        }
    }

    #[test]
    fn names_and_strictness() {
        assert_eq!(
            index_name("ragmonk", IndexKind::SourceState),
            "ragmonk-source-state"
        );
        for kind in IndexKind::ALL {
            let body = create_body(
                kind,
                Engine::OpenSearch,
                Some(&spec()),
                &IndexSettings::default(),
            );
            assert_eq!(body["mappings"]["dynamic"], "strict");
            assert_eq!(body["mappings"]["_meta"]["schema_version"], SCHEMA_VERSION);
        }
    }

    #[test]
    fn vectors_on_chunks_and_code_and_engine_specific() {
        let os = create_body(
            IndexKind::Chunks,
            Engine::OpenSearch,
            Some(&spec()),
            &IndexSettings::default(),
        );
        assert_eq!(
            os["mappings"]["properties"]["embedding"]["type"],
            "knn_vector"
        );
        assert_eq!(os["settings"]["index"]["knn"], true);
        let es = create_body(
            IndexKind::Chunks,
            Engine::Elasticsearch,
            Some(&spec()),
            &IndexSettings::default(),
        );
        assert_eq!(
            es["mappings"]["properties"]["embedding"]["type"],
            "dense_vector"
        );
        assert_eq!(es["mappings"]["properties"]["embedding"]["dims"], 384);
        let code = create_body(
            IndexKind::Code,
            Engine::OpenSearch,
            Some(&spec()),
            &IndexSettings::default(),
        );
        assert_eq!(
            code["mappings"]["properties"]["embedding"]["type"],
            "knn_vector"
        );
        let files = create_body(
            IndexKind::Files,
            Engine::OpenSearch,
            Some(&spec()),
            &IndexSettings::default(),
        );
        assert!(files["mappings"]["properties"].get("embedding").is_none());
        assert!(files["settings"]["index"].get("knn").is_none());
    }

    /// Every field the server read and write paths rely on is mapped
    /// (strict mappings reject anything else at write time).
    #[test]
    fn mappings_carry_server_mode_fields() {
        let has = |kind: IndexKind, fields: &[&str]| {
            let body = create_body(
                kind,
                Engine::Elasticsearch,
                Some(&spec()),
                &IndexSettings::default(),
            );
            for f in fields {
                assert!(
                    body["mappings"]["properties"].get(*f).is_some(),
                    "{kind:?} lacks {f}"
                );
            }
        };
        has(
            IndexKind::SourceState,
            &[
                "path",
                "source_type",
                "enabled",
                "include_patterns",
                "exclude_patterns",
                "online_status",
                "lease_owner",
                "lease_token",
                "lease_expires_at",
                "retired_builds",
                "last_error",
                "published_at",
            ],
        );
        has(
            IndexKind::Files,
            &[
                "status",
                "attempt_count",
                "last_error",
                "last_error_at",
                "next_attempt_at",
            ],
        );
        has(
            IndexKind::Runtime,
            &[
                "source_id",
                "run_id",
                "host",
                "lease_token",
                "stage",
                "heartbeat_at",
                "expires_at",
                "outcome",
            ],
        );
        has(
            IndexKind::Code,
            &[
                "start_col",
                "end_col",
                "mtime",
                "embedding",
                "embedding_fingerprint",
            ],
        );
        has(
            IndexKind::Chunks,
            &[
                "parent_ordinal",
                "fts_heading",
                "document_format",
                "attachment_index",
                "parent_title",
                "mtime",
                "embedding_fingerprint",
            ],
        );
        has(
            IndexKind::Relationships,
            &[
                "reference_text",
                "link_type",
                "manual_link_id",
                "chunk_ordinal",
            ],
        );
    }

    #[test]
    fn meta_mismatch_is_refused() {
        let m = meta(IndexKind::Chunks, Some(&spec()));
        assert!(check_meta("x", Some(&m), IndexKind::Chunks, Some(&spec())).is_ok());
        let mut other = spec();
        other.dims = 768;
        assert!(check_meta("x", Some(&m), IndexKind::Chunks, Some(&other)).is_err());
        assert!(check_meta("x", None, IndexKind::Chunks, Some(&spec())).is_err());
    }
}
