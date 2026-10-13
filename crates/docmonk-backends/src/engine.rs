//! Engine-specific mapping/query fragments.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    OpenSearch,
    Elasticsearch,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::OpenSearch => "opensearch",
            Engine::Elasticsearch => "elasticsearch",
        }
    }

    /// Detects the engine from `GET /`.
    pub fn detect(root: &Value) -> Option<Engine> {
        let version = root.get("version")?;
        match version.get("distribution").and_then(Value::as_str) {
            Some("opensearch") => Some(Engine::OpenSearch),
            None if version.get("number").is_some() => Some(Engine::Elasticsearch),
            _ => None,
        }
    }

    /// Index settings needed for vector search, merged into `index`.
    pub fn vector_index_settings(self) -> Value {
        match self {
            Engine::OpenSearch => json!({ "knn": true }),
            Engine::Elasticsearch => json!({}),
        }
    }

    /// The vector field mapping with HNSW parameters fixed at creation.
    pub fn vector_field(self, spec: &VectorSpec) -> Value {
        match self {
            Engine::OpenSearch => json!({
                "type": "knn_vector",
                "dimension": spec.dims,
                "method": {
                    "name": "hnsw",
                    "engine": "lucene",
                    "space_type": "cosinesimil",
                    "parameters": { "m": spec.m, "ef_construction": spec.ef_construction }
                }
            }),
            Engine::Elasticsearch => json!({
                "type": "dense_vector",
                "dims": spec.dims,
                "index": true,
                "similarity": "cosine",
                "index_options": { "type": "hnsw", "m": spec.m, "ef_construction": spec.ef_construction }
            }),
        }
    }
}

/// Vector field definition; part of the schema identity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VectorSpec {
    pub dims: u32,
    pub m: u32,
    pub ef_construction: u32,
    /// The embedding model the vectors come from.
    pub model_id: String,
}

/// Vector field for the chunk index, matching the pinned embedding model: the dimensions of the current reference
/// model (all-MiniLM-L6-v2, 384). Changing it is a schema change: new
/// indexes are created and every source is rebuilt; nothing is mutated.
pub fn default_vector_spec() -> VectorSpec {
    VectorSpec {
        dims: 384,
        m: 16,
        ef_construction: 128,
        model_id: "sentence-transformers/all-MiniLM-L6-v2".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_engines() {
        assert_eq!(
            Engine::detect(&json!({"version": {"distribution": "opensearch", "number": "2.15.0"}})),
            Some(Engine::OpenSearch)
        );
        assert_eq!(
            Engine::detect(&json!({"version": {"number": "8.15.0"}})),
            Some(Engine::Elasticsearch)
        );
        assert_eq!(Engine::detect(&json!({})), None);
    }
}
