//! The opt-in MCP query log (`mcp.query_log`): one JSON line per tool call
//! in `<home>/logs/mcp.jsonl` with the time, the tool, its arguments with
//! credentials redacted, how many results it returned and which sources
//! they came from. The file is rotated to `mcp.jsonl.1` past
//! `mcp.query_log_max_bytes`. Logging never fails a call.

use std::collections::BTreeSet;
use std::io::Write;

use docmonk_core::paths::Home;
use serde_json::{json, Value};

pub const FILE: &str = "mcp.jsonl";

fn redact(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(docmonk_core::secrets::redact(s).into_owned()),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), redact(v))).collect()),
        other => other.clone(),
    }
}

/// Records in the answer's top-level lists (warnings aside).
fn result_count(out: &Value) -> usize {
    out.as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.as_str() != "warnings")
        .filter_map(|(_, v)| v.as_array())
        .map(Vec::len)
        .sum()
}

fn sources(v: &Value, into: &mut BTreeSet<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|v| sources(v, into)),
        Value::Object(o) => {
            if let Some(id) = o.get("source_id").and_then(Value::as_str) {
                into.insert(id.to_owned());
            }
            o.values().for_each(|v| sources(v, into));
        }
        _ => {}
    }
}

/// The log line for one call.
pub fn entry(tool: &str, args: &Value, out: &Value) -> Value {
    let mut touched = BTreeSet::new();
    sources(out, &mut touched);
    json!({
        "time": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "tool": tool,
        "arguments": redact(args),
        "ok": out["ok"] == true,
        "error": out["error"]["type"],
        "result_count": result_count(out),
        "sources": touched,
    })
}

pub fn append(home: &Home, tool: &str, args: &Value, out: &Value, max_bytes: i64) {
    let dir = home.logs_dir();
    let path = dir.join(FILE);
    let line = entry(tool, args, out).to_string();
    let r = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        if size > 0 && size + line.len() as u64 + 1 > max_bytes.max(0) as u64 {
            std::fs::rename(&path, dir.join(format!("{FILE}.1")))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")
    })();
    if let Err(e) = r {
        eprintln!("docmonk mcp: query log not written: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_redacted_and_results_counted() {
        let args = json!({"query": "why does DB_PASSWORD=hunter2hunter fail", "limit": 5});
        let out = json!({"ok": true, "error": null,
            "results": [{"source_id": "s1"}, {"source_id": "s2"}, {"source_id": "s1"}],
            "warnings": ["w"]});
        let e = entry("docmonk_search", &args, &out);
        assert_eq!(e["tool"], "docmonk_search");
        assert!(!e.to_string().contains("hunter2hunter"), "{e}");
        assert_eq!(e["result_count"], 3);
        assert_eq!(e["sources"], json!(["s1", "s2"]));
        assert_eq!(e["arguments"]["limit"], 5);
    }

    #[test]
    fn the_log_rotates_past_its_size() {
        let d = tempfile::tempdir().unwrap();
        let home = Home::new(d.path());
        let out = json!({"ok": true, "results": []});
        for _ in 0..40 {
            append(&home, "docmonk_search", &json!({"query": "q"}), &out, 4096);
        }
        let log = home.logs_dir().join(FILE);
        let rotated = home.logs_dir().join(format!("{FILE}.1"));
        assert!(rotated.is_file());
        assert!(std::fs::metadata(&log).unwrap().len() <= 4096);
        for l in std::fs::read_to_string(&log).unwrap().lines() {
            let v: Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["tool"], "docmonk_search");
        }
    }
}
