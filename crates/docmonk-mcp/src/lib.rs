//! The RagMonk MCP server (`ragmonk serve --mcp`) over stdio.
//!
//! A JSON-RPC 2.0 loop over newline-delimited stdin/stdout exposing the
//! read-only `ragmonk_*` tools (catalog in `mcp_tools.json`) with
//! `{schema_version, ok, error, ...}` results. `ragmonk_ask` answers
//! through the configured AI provider, like `ragmonk ask`.
//!
//! Each tool call loads the config and opens the indexed sources afresh,
//! like a CLI invocation, on a worker thread bounded by
//! `mcp.request_timeout_seconds`. Errors never cross the boundary as
//! protocol failures: they become `ok: false` results typed by the error
//! kind, `timeout` or `internal_error`.

use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

use docmonk_core::errors::{ErrorKind, RagMonkError};
use docmonk_core::paths::Home;
use docmonk_retrieval::graph::{self, Direction};
use docmonk_retrieval::lexical;
use serde_json::{json, Map, Value};

use docmonk_service::query::{self as query_cmd, open_sources};
use docmonk_service::{load, prepared_home};

/// Protocol versions the server accepts; anything else gets the latest.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SCHEMA_VERSION: &str = "1";

pub const INSTRUCTIONS: &str =
    "RagMonk exposes code and document knowledge already indexed locally \
on this machine (see `ragmonk index`). Start with ragmonk_explore for a natural-language or \
identifier query -- it is the primary tool and runs RagMonk's deterministic query planner. Use \
ragmonk_evidence when you need hard source/path/document filters or verified [E#] citations \
with an explicit insufficient_evidence verdict. The other read-only tools \
(ragmonk_search/symbol/callers/callees/impact/documents/status) are narrower, single-purpose \
lookups; all 9 of them return a bounded, versioned JSON result \
(schema_version/ok/error) and never call an LLM or the network -- results are \
retrieved/structured data for the calling agent to reason over. ragmonk_ask is the one \
exception: it calls the locally configured `ai:` provider (which may be a real cloud endpoint, \
gated by privacy.external_ai_allowed) to synthesize an answer over the same evidence \
ragmonk_explore would return -- prefer ragmonk_explore when the calling agent can reason over \
evidence itself; use ragmonk_ask only when a synthesized natural-language answer is \
specifically wanted.";

/// The tool catalog `tools/list` returns.
pub fn tools() -> &'static [Value] {
    static TOOLS: OnceLock<Vec<Value>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        serde_json::from_str(include_str!("mcp_tools.json")).expect("valid mcp_tools.json")
    })
}

fn tool(name: &str) -> Option<&'static Value> {
    tools().iter().find(|t| t["name"] == name)
}

/// `ragmonk serve [--mcp]`.
pub fn serve(mcp: bool) -> Result<(), RagMonkError> {
    if !mcp {
        return Err(RagMonkError::usage(
            "ragmonk serve currently requires --mcp (the REST API lands in a later phase).",
        ));
    }
    let home = prepared_home()?;
    if !load(&home)?.mcp.enabled {
        return Err(RagMonkError::new(
            ErrorKind::Config,
            "the MCP server is disabled (mcp.enabled=false in config); run `ragmonk config set \
             mcp.enabled true` to enable it.",
        ));
    }
    // stdout is the transport: diagnostics go to stderr only.
    eprintln!("RagMonk MCP server starting on stdio...");
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&line) {
            let text = serde_json::to_string(&reply).unwrap_or_default();
            if writeln!(stdout, "{text}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
    Ok(())
}

/// One JSON-RPC message in, at most one message out.
pub fn handle_line(line: &str) -> Option<Value> {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        // The parse failure is logged to the client.
        return Some(json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "level": "error",
                "logger": "mcp.server.exception_handler",
                "data": "Internal Server Error",
            },
        }));
    };
    let method = msg.get("method")?.as_str()?;
    // Notifications (no id) and responses get no reply.
    let id = msg.get("id").filter(|id| !id.is_null())?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let outcome = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "prompts/list" => Ok(json!({"prompts": []})),
        "resources/list" => Ok(json!({"resources": []})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "tools/call" => Ok(call_tool(&params)),
        _ => Err(json!({"code": -32602, "message": "Invalid request parameters", "data": ""})),
    };
    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    })
}

fn initialize(params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str().unwrap_or_default();
    let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": {
            "experimental": {},
            "prompts": {"listChanged": false},
            "resources": {"subscribe": false, "listChanged": false},
            "tools": {"listChanged": false},
        },
        "serverInfo": {
            "name": format!("ragmonk v{}", docmonk_core::version::version()),
            "version": docmonk_core::version::version(),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn text_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn call_tool(params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let Some(spec) = tool(name) else {
        return text_result(format!("Unknown tool: {name}"), true);
    };
    let empty = Value::Object(Map::new());
    let args = match &params["arguments"] {
        Value::Null => &empty,
        other => other,
    };
    let args = match validate(name, &spec["inputSchema"], args) {
        Ok(args) => args,
        Err(message) => {
            return text_result(format!("Error executing tool {name}: {message}"), true)
        }
    };
    let output = project(
        &run_tool(name, args),
        &spec["outputSchema"],
        &spec["outputSchema"],
    );
    let text = serde_json::to_string_pretty(&output).unwrap_or_default();
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": output,
        "isError": false,
    })
}

// ---------------------------------------------------------- validation ---

struct Issue {
    field: String,
    message: &'static str,
}

/// Integer arguments also accept integral floats and numeric strings.
fn as_int(v: &Value) -> Result<Value, &'static str> {
    match v {
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(v.clone()),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 => Ok(json!(f as i64)),
            _ => Err("expected an integer"),
        },
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .map(|i| json!(i))
            .map_err(|_| "expected an integer"),
        _ => Err("expected an integer"),
    }
}

/// Checks `args` against a tool's input schema, returning the coerced
/// arguments or a message listing every invalid field.
fn validate(tool: &str, schema: &Value, args: &Value) -> Result<Map<String, Value>, String> {
    let Value::Object(given) = args else {
        return Err(format!("invalid arguments for {tool}: expected an object"));
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut out = Map::new();
    let mut issues = Vec::new();
    for (field, prop) in schema["properties"].as_object().into_iter().flatten() {
        let Some(value) = given.get(field) else {
            if required.contains(&field.as_str()) {
                issues.push(Issue {
                    field: field.clone(),
                    message: "required",
                });
            }
            continue;
        };
        let nullable = prop["anyOf"]
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t["type"] == "null"));
        let ty = prop["type"].as_str().or_else(|| {
            prop["anyOf"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["type"].as_str())
                .find(|t| *t != "null")
        });
        if value.is_null() && nullable {
            out.insert(field.clone(), Value::Null);
            continue;
        }
        let checked = match ty {
            Some("string") => match value {
                Value::String(_) => Ok(value.clone()),
                _ => Err("expected a string"),
            },
            Some("integer") => as_int(value),
            _ => Ok(value.clone()),
        };
        match checked {
            Ok(v) => {
                out.insert(field.clone(), v);
            }
            Err(message) => issues.push(Issue {
                field: field.clone(),
                message,
            }),
        }
    }
    if issues.is_empty() {
        return Ok(out);
    }
    let detail: Vec<String> = issues
        .iter()
        .map(|i| format!("{}: {}", i.field, i.message))
        .collect();
    Err(format!(
        "invalid arguments for {tool}: {}",
        detail.join("; ")
    ))
}

// ---------------------------------------------------------- projection ---

fn resolve<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    match schema["$ref"]
        .as_str()
        .and_then(|r| r.strip_prefix("#/$defs/"))
    {
        Some(name) => &root["$defs"][name],
        None => schema,
    }
}

/// Shapes `value` to the tool's output schema: only the schema's fields,
/// defaults for the missing ones.
fn project(value: &Value, schema: &Value, root: &Value) -> Value {
    let schema = resolve(schema, root);
    if let Some(options) = schema["anyOf"].as_array() {
        if value.is_null() {
            return Value::Null;
        }
        return options
            .iter()
            .map(|o| resolve(o, root))
            .find(|o| o["type"] != "null")
            .map_or_else(|| value.clone(), |o| project(value, o, root));
    }
    match (schema["type"].as_str(), value) {
        (Some("object"), Value::Object(map)) => {
            let Some(props) = schema["properties"].as_object() else {
                return value.clone();
            };
            let mut out = Map::new();
            for (field, prop) in props {
                let v = match map.get(field) {
                    Some(v) => project(v, prop, root),
                    None if prop.get("default").is_some() => prop["default"].clone(),
                    None if resolve(prop, root)["type"] == "array" => json!([]),
                    None => continue,
                };
                out.insert(field.clone(), v);
            }
            Value::Object(out)
        }
        (Some("array"), Value::Array(items)) => Value::Array(
            items
                .iter()
                .map(|i| project(i, &schema["items"], root))
                .collect(),
        ),
        _ => value.clone(),
    }
}

// --------------------------------------------------------------- tools ---

/// The error type name reported in a failed tool result.
fn error_type(e: &RagMonkError) -> &'static str {
    if let Some(class) = e.class() {
        return class;
    }
    match e.kind() {
        ErrorKind::Generic => "RagMonkError",
        ErrorKind::Usage => "UsageError",
        ErrorKind::Config => "ConfigError",
        ErrorKind::SourceUnavailable => "SourceUnavailableError",
        ErrorKind::Database => "DatabaseError",
        ErrorKind::IndexingPartialFailure => "IndexingPartialFailureError",
        ErrorKind::HealthCheck => "HealthCheckError",
        ErrorKind::SecurityViolation => "SecurityViolationError",
        ErrorKind::RunLockTimeout => "RunLockTimeoutError",
        ErrorKind::LocalStorageModeRequired => "LocalStorageModeRequiredError",
        ErrorKind::ContentChangedDuringProcessing => "ContentChangedDuringProcessingError",
    }
}

fn failure(kind: &str, message: impl Into<String>) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "ok": false,
        "error": {"type": kind, "message": message.into()},
    })
}

fn run_tool(name: &str, args: Map<String, Value>) -> Value {
    let home = match prepared_home() {
        Ok(h) => h,
        Err(e) => return failure(error_type(&e), e.message()),
    };
    let timeout = match load(&home) {
        Ok(cfg) => cfg.mcp.request_timeout_seconds,
        Err(e) => return failure(error_type(&e), e.message()),
    };
    // One result per call; an abandoned worker's send never blocks.
    let (tx, rx) = mpsc::sync_channel(1);
    let name = name.to_owned();
    // An abandoned worker runs to completion in the background; the
    // timeout only stops waiting for it.
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(|| work(&home, &name, &args));
        let _ = tx.send(outcome);
    });
    let wait = Duration::try_from_secs_f64(timeout.max(0.0)).unwrap_or(Duration::MAX);
    match rx.recv_timeout(wait) {
        Ok(Ok(Ok(Value::Object(mut data)))) => {
            data.insert("schema_version".into(), json!(SCHEMA_VERSION));
            data.insert("ok".into(), json!(true));
            data.insert("error".into(), Value::Null);
            Value::Object(data)
        }
        Ok(Ok(Ok(_))) => failure("internal_error", "tool returned no data"),
        Ok(Ok(Err(e))) => failure(error_type(&e), e.message()),
        Ok(Err(panic)) => failure(
            "internal_error",
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "internal error".into()),
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => failure(
            "timeout",
            format!("tool call exceeded mcp.request_timeout_seconds={timeout}s"),
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => failure("internal_error", "worker stopped"),
    }
}

fn required(args: &Map<String, Value>, field: &str) -> Result<String, RagMonkError> {
    let value = args[field].as_str().unwrap_or_default().trim();
    if value.is_empty() {
        return Err(RagMonkError::usage(format!("{field} must not be empty")));
    }
    Ok(value.to_owned())
}

/// An optional positive integer argument; `None`/0 mean the default.
fn optional(args: &Map<String, Value>, field: &str, default: usize) -> usize {
    match args.get(field).and_then(Value::as_i64) {
        Some(v) if v != 0 => v.max(0) as usize,
        _ => default,
    }
}

fn work(home: &Home, name: &str, args: &Map<String, Value>) -> Result<Value, RagMonkError> {
    let cfg = load(home)?;
    match name {
        "ragmonk_explore" => {
            let query = required(args, "query")?;
            let mut budget = query_cmd::config_budget(&cfg);
            let set = |field: &str, slot: &mut usize| {
                if let Some(v) = args.get(field).and_then(Value::as_i64) {
                    *slot = v.max(0) as usize;
                }
            };
            set("max_chars", &mut budget.max_chars);
            set("max_files", &mut budget.max_files);
            set("max_graph_nodes", &mut budget.max_graph_nodes);
            let opened = open_sources(home, None)?;
            let r = query_cmd::explore_value(home, &cfg, &opened, &query, budget)?;
            let mut warnings: Vec<Value> = Vec::new();
            if r["evidence_truncated"] == true {
                warnings.extend(
                    r["evidence_truncation_reasons"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            let empty = |k: &str| r[k].as_array().is_none_or(Vec::is_empty);
            if empty("requirements") && empty("incidents") {
                warnings.push(json!(
                    "Requirements/Incidents are always empty: RagMonk has no document \
                     classifier yet to populate these categories."
                ));
            }
            Ok(json!({
                "query": r["query"],
                "intent": r["intent"],
                "strategies": r["strategies"],
                "answer_context": r["summary"],
                "entities": r["symbols"],
                "relationships": r["call_flows"],
                "documents": r["documents"],
                "tests": r["tests"],
                "evidence": r["evidence"],
                "warnings": warnings,
            }))
        }
        "ragmonk_search" => {
            let query = required(args, "query")?;
            let limit = optional(args, "limit", lexical::DEFAULT_LIMIT);
            query_cmd::lexical_value(&cfg, &open_sources(home, None)?, &query, limit)
        }
        "ragmonk_evidence" => {
            let query = required(args, "query")?;
            let list = |k: &str| -> Vec<String> {
                args.get(k)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            };
            docmonk_service::evidence::evidence_value(
                home,
                &cfg,
                &docmonk_service::evidence::EvidenceRequest {
                    query: &query,
                    filters: docmonk_retrieval::route::SearchFilters {
                        source_ids: list("source_ids"),
                        path_prefixes: list("path_prefixes"),
                        kinds: list("kinds"),
                        document_ids: list("document_ids"),
                        exclude_attachments: args
                            .get("exclude_attachments")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    },
                    limit: optional(args, "limit", 10).min(100),
                },
            )
        }
        "ragmonk_symbol" => {
            let name = required(args, "name")?;
            query_cmd::symbol_value(&open_sources(home, None)?, &name)
        }
        "ragmonk_callers" | "ragmonk_callees" => {
            let symbol = required(args, "name")?;
            let direction = if name == "ragmonk_callers" {
                Direction::Incoming
            } else {
                Direction::Outgoing
            };
            query_cmd::calls_value(
                &open_sources(home, None)?,
                &symbol,
                direction,
                optional(args, "max_depth", graph::DEFAULT_MAX_DEPTH),
                optional(args, "limit", graph::DEFAULT_LIMIT),
            )
        }
        "ragmonk_impact" => {
            let symbol = required(args, "name")?;
            query_cmd::impact_value(
                &open_sources(home, None)?,
                &symbol,
                optional(args, "max_depth", graph::DEFAULT_MAX_DEPTH),
                optional(args, "limit", graph::DEFAULT_LIMIT),
            )
        }
        "ragmonk_documents" => {
            let source = args
                .get("source_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Ok(json!({"documents": docmonk_service::sources::docs_rows(home, source)?}))
        }
        "ragmonk_status" => {
            let source = args
                .get("source_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let limit = args.get("error_limit").and_then(Value::as_i64);
            let mut report = docmonk_service::status::report_with(home, |o| {
                if let Some(n) = limit.filter(|n| *n >= 0) {
                    o.error_limit = n as usize;
                }
                o.error_source = source.clone();
            })?;
            if let Some(id) = &source {
                if report.source(id).is_none() {
                    return Err(RagMonkError::usage(format!("no such source: {id}")));
                }
                report.sources.retain(|s| &s.source_id == id);
            }
            Ok(json!({ "status": report }))
        }
        "ragmonk_ask" => {
            let question = required(args, "question")?;
            let r = docmonk_service::ask::ask_value(&question)?;
            let mut warnings: Vec<Value> = Vec::new();
            if r["evidence_truncated"] == true {
                warnings.extend(
                    r["evidence_truncation_reasons"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            Ok(json!({
                "question": r["question"],
                "answer": r["answer"],
                "evidence": r["evidence"],
                "warnings": warnings,
            }))
        }
        other => Err(RagMonkError::usage(format!("Unknown tool: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_ten_tools() {
        let names: Vec<&str> = tools().iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names.len(), 10);
        assert!(names.contains(&"ragmonk_evidence"));
        assert!(names.contains(&"ragmonk_ask"));
    }

    #[test]
    fn validation_lists_every_invalid_field() {
        let schema = &tool("ragmonk_search").unwrap()["inputSchema"];
        let err = validate("ragmonk_search", schema, &json!({"limit": "x"})).unwrap_err();
        assert_eq!(
            err,
            "invalid arguments for ragmonk_search: query: required; limit: expected an integer"
        );
        let ok = validate(
            "ragmonk_search",
            schema,
            &json!({"query": "q", "limit": "3", "extra": 1}),
        )
        .unwrap();
        assert_eq!(ok["limit"], 3);
        assert!(!ok.contains_key("extra"));
    }

    #[test]
    fn projection_keeps_schema_fields_with_defaults() {
        let schema = &tool("ragmonk_documents").unwrap()["outputSchema"];
        let v = project(
            &json!({"ok": true, "documents": [{"source_id": "s", "file_id": "f", "path": "p", "status": "indexed", "section_count": 1, "paragraph_count": 0, "table_count": 0, "is_scanned": false, "attachments": []}]}),
            schema,
            schema,
        );
        assert_eq!(v["schema_version"], "1");
        assert!(v["error"].is_null());
        assert!(v["documents"][0].get("attachments").is_none());
        assert!(v["documents"][0]["title"].is_null());
    }

    #[test]
    fn status_projection_is_lossless() {
        let report: Value = serde_json::from_str(include_str!(
            "../../docmonk-status/tests/fixtures/server_multi_host.json"
        ))
        .unwrap();
        let schema = &tool("ragmonk_status").unwrap()["outputSchema"];
        let out = project(&json!({"ok": true, "status": report}), schema, schema);
        assert_eq!(out["status"], report, "every canonical field survives");
        assert_eq!(out["schema_version"], "1");
        let failed = project(
            &failure("BackendUnavailableError", "server unreachable"),
            schema,
            schema,
        );
        assert_eq!(failed["ok"], false);
        assert!(failed["status"].is_null());
        assert_eq!(failed["error"]["type"], "BackendUnavailableError");
    }

    #[test]
    fn protocol_basics() {
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let r = handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#).unwrap();
        assert_eq!(r["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
        let r = handle_line(r#"{"jsonrpc":"2.0","id":"a","method":"x"}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602);
        assert_eq!(r["id"], "a");
        let r = handle_line("garbage").unwrap();
        assert_eq!(r["method"], "notifications/message");
    }
}
