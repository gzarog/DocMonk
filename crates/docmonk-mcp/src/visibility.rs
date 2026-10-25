//! Per-source MCP visibility (`mcp.hidden_sources`). A hidden source stays
//! searchable from the CLI and the Admin UI but does not exist for AI
//! agents: every tool call runs in a
//! [`with_hidden_sources`](docmonk_service::query::with_hidden_sources)
//! scope, so its records are never read (search, evidence, explore, graph
//! traversals, ask), naming it as a filter is an unknown source, and as a
//! last line every answer is scrubbed of records that carry its id or lie
//! under its root.

use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::Home;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct Hidden {
    /// Hidden source ids.
    pub ids: Vec<String>,
    /// Their root paths, as stored.
    roots: Vec<String>,
}

impl Hidden {
    /// The registered sources `entries` (ids or paths) name. Unknown entries
    /// hide nothing.
    pub fn resolve(home: &Home, entries: &[String]) -> Result<Self, DocMonkError> {
        if entries.iter().all(|e| e.trim().is_empty()) {
            return Ok(Self::default());
        }
        let sources = docmonk_service::sources::catalog(home)?.list(false)?;
        let ids = docmonk_service::query::resolve_source_names(&sources, entries);
        let roots = sources
            .iter()
            .filter(|s| ids.contains(&s.id))
            .map(|s| s.path.clone())
            .collect();
        Ok(Self { ids, roots })
    }

    fn is_hidden_id(&self, id: &str) -> bool {
        self.ids.iter().any(|h| h == id)
    }

    fn under_hidden_root(&self, path: &str) -> bool {
        self.roots.iter().any(|r| {
            path == r
                || path
                    .strip_prefix(r.as_str())
                    .is_some_and(|rest| rest.starts_with(['/', '\\']))
        })
    }

    /// A call that names a hidden source (`source_id`, `source_ids`) is
    /// answered as if it did not exist.
    pub fn check_args(&self, args: &Map<String, Value>) -> Result<(), DocMonkError> {
        let named = args
            .get("source_id")
            .into_iter()
            .chain(
                args.get("source_ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .filter_map(Value::as_str)
            .map(str::trim);
        for id in named {
            if self.is_hidden_id(id)
                || self.under_hidden_root(id)
                || self.roots.iter().any(|r| r == id)
            {
                return Err(DocMonkError::usage(format!("no such source: {id}")));
            }
        }
        Ok(())
    }

    /// Drops every array element that is a record of a hidden source: an
    /// object whose `source_id` is hidden or whose `path` lies under a
    /// hidden root.
    pub fn scrub(&self, mut v: Value) -> Value {
        if !self.ids.is_empty() {
            self.scrub_in_place(&mut v);
        }
        v
    }

    fn hides_str(&self, s: &str) -> bool {
        self.is_hidden_id(s) || self.under_hidden_root(s)
    }

    fn hides(&self, v: &Value) -> bool {
        if let Value::String(s) = v {
            return self.hides_str(s);
        }
        let Value::Object(o) = v else { return false };
        let str_of = |k: &str| o.get(k).and_then(Value::as_str);
        str_of("source_id").is_some_and(|id| self.is_hidden_id(id))
            || ["path", "file_path", "source_path"]
                .iter()
                .filter_map(|k| str_of(k))
                .any(|p| self.under_hidden_root(p))
    }

    fn scrub_in_place(&self, v: &mut Value) {
        match v {
            Value::Array(items) => {
                items.retain(|i| !self.hides(i));
                items.iter_mut().for_each(|i| self.scrub_in_place(i));
            }
            Value::Object(o) => o.values_mut().for_each(|i| self.scrub_in_place(i)),
            // A field naming a hidden source (a dependency's producer, a
            // lock holder, ...) is blanked.
            Value::String(s) if self.hides_str(s) => *v = Value::Null,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrub_drops_hidden_records_at_any_depth() {
        let h = Hidden {
            ids: vec!["src_b".into()],
            roots: vec!["/data/b".into()],
        };
        let out = h.scrub(json!({
            "results": [
                {"source_id": "src_a", "path": "/data/a/x.md"},
                {"source_id": "src_b", "path": "/data/b/y.md"},
                {"path": "/data/b/z.md"},
                {"path": "/data/bb/kept.md"},
            ],
            "graph": {"nodes": [{"source_id": "src_b"}, {"source_id": "src_a"}]},
            "dependency": {"producers": ["src_b", "src_a"], "holder": "src_b"},
        }));
        assert_eq!(
            out,
            json!({
                "results": [
                    {"source_id": "src_a", "path": "/data/a/x.md"},
                    {"path": "/data/bb/kept.md"},
                ],
                "graph": {"nodes": [{"source_id": "src_a"}]},
                "dependency": {"producers": ["src_a"], "holder": null},
            })
        );
        let args = json!({"source_ids": ["src_b"]});
        assert!(h.check_args(args.as_object().unwrap()).is_err());
        let args = json!({"source_id": "src_a"});
        assert!(h.check_args(args.as_object().unwrap()).is_ok());
    }
}
