//! The DocMonk MCP server (`docmonk serve --mcp`) over stdio.
//!
//! A JSON-RPC 2.0 loop over newline-delimited stdin/stdout exposing the
//! read-only `docmonk_*` tools (catalog in `mcp_tools.json`) with
//! `{schema_version, ok, error, ...}` results. `docmonk_ask` answers
//! through the configured AI provider, like `docmonk ask`.
//!
//! Each tool call loads the config and opens the indexed sources afresh,
//! like a CLI invocation, on a worker thread bounded by
//! `mcp.request_timeout_seconds`. Errors never cross the boundary as
//! protocol failures: they become `ok: false` results typed by the error
//! kind, `timeout` or `internal_error`.

use std::io::{BufRead, Write};
use std::sync::OnceLock;
use std::time::Duration;

use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::Home;
use docmonk_retrieval::graph::{self, Direction};
use docmonk_retrieval::lexical;
use serde_json::{json, Map, Value};

use docmonk_service::query::{self as query_cmd, open_sources};
use docmonk_service::{load, prepared_home};

pub mod exec;
pub mod query_log;
pub mod visibility;

/// Protocol versions the server accepts; anything else gets the latest.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SCHEMA_VERSION: &str = "1";

pub const INSTRUCTIONS: &str =
    "DocMonk exposes code and document knowledge already indexed locally \
on this machine (see `docmonk index`). Start with docmonk_explore for a natural-language or \
identifier query -- it is the primary tool and runs DocMonk's deterministic query planner. Use \
docmonk_evidence when you need hard source/path/document filters or verified [E#] citations \
with an explicit insufficient_evidence verdict. The other read-only tools \
(docmonk_search/symbol/callers/callees/impact/documents/status) are narrower, single-purpose \
lookups; all 9 of them return a bounded, versioned JSON result \
(schema_version/ok/error) and never call an LLM or the network -- results are \
retrieved/structured data for the calling agent to reason over. docmonk_ask is the one \
exception: it calls the locally configured `ai:` provider (which may be a real cloud endpoint, \
gated by privacy.external_ai_allowed) to synthesize an answer over the same evidence \
docmonk_explore would return -- prefer docmonk_explore when the calling agent can reason over \
evidence itself; use docmonk_ask only when a synthesized natural-language answer is \
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

/// `docmonk serve [--mcp]`.
pub fn serve(mcp: bool) -> Result<(), DocMonkError> {
    if !mcp {
        return Err(DocMonkError::usage(
            "docmonk serve currently requires --mcp (the REST API lands in a later phase).",
        ));
    }
    let home = prepared_home()?;
    // A long-lived process: reuse read connections between tool calls.
    docmonk_service::query::enable_store_pool();
    if !load(&home)?.mcp.enabled {
        return Err(DocMonkError::new(
            ErrorKind::Config,
            "the MCP server is disabled (mcp.enabled=false in config); run `docmonk config set \
             mcp.enabled true` to enable it.",
        ));
    }
    // stdout is the transport: diagnostics go to stderr only.
    eprintln!("DocMonk MCP server starting on stdio...");
    let limit = usize::try_from(load(&home)?.mcp.max_request_bytes).unwrap_or(1 << 20);
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut stdout = std::io::stdout();
    loop {
        let reply = match read_bounded_line(&mut input, limit) {
            Ok(None) | Err(_) => break,
            Ok(Some(Err(()))) => Some(too_large(limit)),
            Ok(Some(Ok(line))) if line.trim().is_empty() => continue,
            Ok(Some(Ok(line))) => handle_line(&line),
        };
        if let Some(reply) = reply {
            let text = serde_json::to_string(&reply).unwrap_or_default();
            if writeln!(stdout, "{text}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
    // Graceful shutdown: give cancelled workers a moment to stop.
    let drained = executor().drain(Duration::from_secs(5));
    eprintln!(
        "DocMonk MCP server stopped (drained: {drained}); metrics: {}",
        metrics().to_json()
    );
    Ok(())
}

/// JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The reply to a message longer than `mcp.max_request_bytes` (its id is
/// unknown, so `null`).
pub fn too_large(limit: usize) -> Value {
    rpc_error(
        Value::Null,
        INVALID_REQUEST,
        &format!("request exceeds mcp.max_request_bytes={limit}"),
    )
}

/// One JSON-RPC message in, at most one message out. Malformed JSON is a
/// parse error (-32700, id null); a message that is not a valid request
/// is -32600; an unknown method -32601; bad `tools/call` params -32602.
/// Notifications (no `id`) and responses never get a reply.
pub fn handle_line(line: &str) -> Option<Value> {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        eprintln!("docmonk mcp: unparseable message ({} bytes)", line.len());
        return Some(rpc_error(Value::Null, PARSE_ERROR, "Parse error"));
    };
    let Value::Object(obj) = &msg else {
        return Some(rpc_error(Value::Null, INVALID_REQUEST, "Invalid Request"));
    };
    // A valid id is a string, a number or null; anything else is unusable.
    let id = match obj.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_) | Value::Null)) => Some(v.clone()),
        Some(_) => {
            return Some(rpc_error(
                Value::Null,
                INVALID_REQUEST,
                "Invalid Request: bad id",
            ))
        }
    };
    let method = obj.get("method").and_then(Value::as_str);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() {
        // A response from the client (result/error, no method) needs no reply.
        if method.is_none() && (obj.contains_key("result") || obj.contains_key("error")) {
            return None;
        }
        return Some(rpc_error(
            id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            "Invalid Request",
        ));
    }
    let method = method.unwrap_or_default();
    // Notifications get no reply.
    let id = id?;
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    let outcome = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "prompts/list" => Ok(json!({"prompts": []})),
        "resources/list" => Ok(json!({"resources": []})),
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "tools/call" if !params.is_object() => Err((INVALID_PARAMS, "Invalid params")),
        "tools/call" => Ok(call_tool(&params)),
        _ => Err((METHOD_NOT_FOUND, "Method not found")),
    };
    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => rpc_error(id, code, message),
    })
}

/// Reads one `\n`-terminated line of at most `limit` bytes. `Ok(None)` at
/// end of input; `Err(())` for an over-long line, which is consumed up to
/// its newline so the stream stays in sync.
pub fn read_bounded_line(
    r: &mut impl BufRead,
    limit: usize,
) -> std::io::Result<Option<Result<String, ()>>> {
    let mut buf = Vec::new();
    let mut over = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            if buf.is_empty() && !over {
                return Ok(None);
            }
            break;
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(p) => (p + 1, true),
            None => (chunk.len(), false),
        };
        if !over {
            if buf.len() + take > limit + 1 {
                over = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        if done {
            break;
        }
    }
    if over {
        return Ok(Some(Err(())));
    }
    while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        buf.pop();
    }
    Ok(Some(Ok(String::from_utf8_lossy(&buf).into_owned())))
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
            "name": format!("docmonk v{}", docmonk_core::version::version()),
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
    tool_result(output)
}

/// The `tools/call` result for a tool's (projected) output.
fn tool_result(output: Value) -> Value {
    let text = serde_json::to_string_pretty(&output).unwrap_or_default();
    // A failed call (`ok: false`) is a tool error to the client, not a
    // successful result that happens to describe one.
    let is_error = output["ok"] != true;
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": output,
        "isError": is_error,
    })
}

// ---------------------------------------------------------- validation ---

struct Issue {
    field: String,
    message: String,
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

/// The non-null branch of a property (`anyOf: [T, null]`), or the
/// property itself.
fn branch(prop: &Value) -> &Value {
    prop["anyOf"]
        .as_array()
        .and_then(|a| a.iter().find(|t| t["type"] != "null"))
        .unwrap_or(prop)
}

/// Checks one present, non-null argument against its schema branch.
fn check(value: &Value, schema: &Value) -> Result<Value, String> {
    match schema["type"].as_str() {
        Some("string") => match value {
            Value::String(_) => Ok(value.clone()),
            _ => Err("expected a string".into()),
        },
        Some("boolean") => match value {
            Value::Bool(_) => Ok(value.clone()),
            _ => Err("expected a boolean".into()),
        },
        Some("integer") => {
            let v = as_int(value)?;
            let n = v.as_i64().unwrap_or(i64::MAX);
            let min = schema["minimum"].as_i64();
            let max = schema["maximum"].as_i64();
            if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                return Err(format!(
                    "must be between {} and {}",
                    min.map_or("-inf".into(), |m| m.to_string()),
                    max.map_or("inf".into(), |m| m.to_string())
                ));
            }
            Ok(v)
        }
        Some("array") => {
            let Value::Array(items) = value else {
                return Err("expected an array".into());
            };
            for (i, item) in items.iter().enumerate() {
                check(item, &schema["items"]).map_err(|m| format!("item {i}: {m}"))?;
            }
            Ok(value.clone())
        }
        _ => Ok(value.clone()),
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
                    message: "required".into(),
                });
            }
            continue;
        };
        let nullable = prop["anyOf"]
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t["type"] == "null"));
        if value.is_null() && nullable {
            out.insert(field.clone(), Value::Null);
            continue;
        }
        match check(value, branch(prop)) {
            Ok(v) => {
                out.insert(field.clone(), v);
            }
            Err(message) => issues.push(Issue {
                field: field.clone(),
                message,
            }),
        }
    }
    // Unknown arguments are rejected, not silently ignored: a misspelled
    // filter must never widen a query.
    if schema["additionalProperties"] == false {
        let known = schema["properties"].as_object();
        let mut extra: Vec<&String> = given
            .keys()
            .filter(|k| known.is_none_or(|p| !p.contains_key(*k)))
            .collect();
        extra.sort();
        for field in extra {
            issues.push(Issue {
                field: field.clone(),
                message: "unknown argument".into(),
            });
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
fn error_type(e: &DocMonkError) -> &'static str {
    if let Some(class) = e.class() {
        return class;
    }
    match e.kind() {
        ErrorKind::Generic => "DocMonkError",
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

/// The process-wide tool executor (limits refreshed from config per call).
pub fn executor() -> &'static exec::Executor {
    static EXEC: OnceLock<exec::Executor> = OnceLock::new();
    EXEC.get_or_init(exec::Executor::default)
}

/// Current executor counters (running, queued, rejected, timed out...).
pub fn metrics() -> exec::Metrics {
    executor().metrics()
}

fn run_tool(name: &str, args: Map<String, Value>) -> Value {
    let home = match prepared_home() {
        Ok(h) => h,
        Err(e) => return failure(error_type(&e), e.message()),
    };
    let mcp = match load(&home) {
        Ok(cfg) => cfg.mcp,
        Err(e) => return failure(error_type(&e), e.message()),
    };
    // Hidden sources are resolved per call, like the config itself.
    let hidden = match visibility::Hidden::resolve(&home, &mcp.hidden_sources) {
        Ok(h) => h,
        Err(e) => return failure(error_type(&e), e.message()),
    };
    let logged = mcp
        .query_log
        .then(|| (Value::Object(args.clone()), home.clone()));
    let out = run_visible(&home, name, args, &mcp, hidden);
    if let Some((args, home)) = logged {
        query_log::append(&home, name, &args, &out, mcp.query_log_max_bytes);
    }
    out
}

fn run_visible(
    home: &Home,
    name: &str,
    args: Map<String, Value>,
    mcp: &docmonk_config::model::McpConfig,
    hidden: visibility::Hidden,
) -> Value {
    let home = home.clone();
    let exec = executor();
    exec.set_limits(exec::Limits {
        max_running: usize::try_from(mcp.max_concurrent_calls).unwrap_or(1),
        max_queued: usize::try_from(mcp.max_queued_calls).unwrap_or(0),
    });
    let timeout = mcp.request_timeout_seconds;
    let wait = Duration::try_from_secs_f64(timeout.max(0.0)).unwrap_or(Duration::MAX);
    let name = name.to_owned();
    match exec.run(wait, move || {
        let ids = hidden.ids.clone();
        query_cmd::with_hidden_sources(ids, || {
            hidden.check_args(&args)?;
            work(&home, &name, &args).map(|v| hidden.scrub(v))
        })
    }) {
        exec::Outcome::Done(Ok(Value::Object(mut data))) => {
            data.insert("schema_version".into(), json!(SCHEMA_VERSION));
            data.insert("ok".into(), json!(true));
            data.insert("error".into(), Value::Null);
            Value::Object(data)
        }
        exec::Outcome::Done(Ok(_)) => failure("internal_error", "tool returned no data"),
        // A worker that hit its own deadline reports the timeout, not the
        // interrupted read underneath.
        exec::Outcome::Done(Err(e)) if e.message().contains(docmonk_core::deadline::CANCELLED) => {
            failure(
                "timeout",
                format!("tool call exceeded mcp.request_timeout_seconds={timeout}s"),
            )
        }
        exec::Outcome::Done(Err(e)) => failure(error_type(&e), e.message()),
        // The panic payload is logged to stderr, never returned.
        exec::Outcome::Panicked => failure("internal_error", "internal error"),
        exec::Outcome::Overloaded => {
            let m = exec.metrics();
            failure(
                "overloaded",
                format!(
                    "{} tool calls running and {} queued (mcp.max_concurrent_calls / \
                     mcp.max_queued_calls); retry later",
                    m.running, m.queued
                ),
            )
        }
        exec::Outcome::TimedOut => failure(
            "timeout",
            format!("tool call exceeded mcp.request_timeout_seconds={timeout}s"),
        ),
    }
}

fn required(args: &Map<String, Value>, field: &str) -> Result<String, DocMonkError> {
    let value = args[field].as_str().unwrap_or_default().trim();
    if value.is_empty() {
        return Err(DocMonkError::usage(format!("{field} must not be empty")));
    }
    Ok(value.to_owned())
}

/// An optional integer argument (range-checked by [`validate`]); absent
/// or null means the default.
fn optional(args: &Map<String, Value>, field: &str, default: usize) -> usize {
    args.get(field)
        .and_then(Value::as_u64)
        .map_or(default, |v| v as usize)
}

fn work(home: &Home, name: &str, args: &Map<String, Value>) -> Result<Value, DocMonkError> {
    let cfg = load(home)?;
    match name {
        "docmonk_explore" => {
            let query = required(args, "query")?;
            let mut budget = query_cmd::config_budget(&cfg);
            let set = |field: &str, slot: &mut usize| {
                if let Some(v) = args.get(field).and_then(Value::as_u64) {
                    *slot = v as usize;
                }
            };
            set("max_chars", &mut budget.max_chars);
            set("max_files", &mut budget.max_files);
            set("max_graph_nodes", &mut budget.max_graph_nodes);
            let opened = open_sources(home, None)?;
            let explain = args
                .get("explain")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let r = query_cmd::explore_value_with(
                home,
                &cfg,
                &opened,
                &query_cmd::ExploreRequest {
                    query: &query,
                    budget,
                    explain,
                },
            )?;
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
                    "Requirements/Incidents are always empty: DocMonk has no document \
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
        "docmonk_search" => {
            let query = required(args, "query")?;
            let limit = optional(args, "limit", lexical::DEFAULT_LIMIT);
            query_cmd::lexical_value(&cfg, &open_sources(home, None)?, &query, limit)
        }
        "docmonk_evidence" => {
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
                    limit: optional(args, "limit", 10),
                },
            )
        }
        "docmonk_symbol" => {
            let name = required(args, "name")?;
            query_cmd::symbol_value(&open_sources(home, None)?, &name)
        }
        "docmonk_callers" | "docmonk_callees" => {
            let symbol = required(args, "name")?;
            let direction = if name == "docmonk_callers" {
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
        "docmonk_impact" => {
            let symbol = required(args, "name")?;
            query_cmd::impact_value(
                &open_sources(home, None)?,
                &symbol,
                optional(args, "max_depth", graph::DEFAULT_MAX_DEPTH),
                optional(args, "limit", graph::DEFAULT_LIMIT),
            )
        }
        "docmonk_documents" => {
            let source = args
                .get("source_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Ok(json!({"documents": docmonk_service::sources::docs_rows(home, source)?}))
        }
        "docmonk_status" => {
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
                    return Err(DocMonkError::usage(format!("no such source: {id}")));
                }
                report.sources.retain(|s| &s.source_id == id);
            }
            Ok(json!({ "status": report }))
        }
        "docmonk_ask" => {
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
        other => Err(DocMonkError::usage(format!("Unknown tool: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_all_ten_tools() {
        let names: Vec<&str> = tools().iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names.len(), 10);
        assert!(names.contains(&"docmonk_evidence"));
        assert!(names.contains(&"docmonk_ask"));
    }

    #[test]
    fn validation_lists_every_invalid_field() {
        let schema = &tool("docmonk_search").unwrap()["inputSchema"];
        let err = validate("docmonk_search", schema, &json!({"limit": "x"})).unwrap_err();
        assert_eq!(
            err,
            "invalid arguments for docmonk_search: query: required; limit: expected an integer"
        );
        let ok = validate(
            "docmonk_search",
            schema,
            &json!({"query": "q", "limit": "3"}),
        )
        .unwrap();
        assert_eq!(ok["limit"], 3);
    }

    /// Audit PH05: unknown arguments, out-of-range numbers, wrong booleans
    /// and wrongly typed array items are rejected (before, they were
    /// dropped, clamped or ignored, silently widening a query).
    #[test]
    fn validation_rejects_unknown_out_of_range_and_mistyped_arguments() {
        let search = &tool("docmonk_search").unwrap()["inputSchema"];
        let err = validate(
            "docmonk_search",
            search,
            &json!({"query": "q", "limit": -5, "sourceids": ["a"]}),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "invalid arguments for docmonk_search: limit: must be between 1 and 1000; \
             sourceids: unknown argument"
        );
        let calls = &tool("docmonk_callers").unwrap()["inputSchema"];
        assert!(validate(
            "docmonk_callers",
            calls,
            &json!({"name": "f", "max_depth": 1_000_000})
        )
        .unwrap_err()
        .contains("max_depth: must be between 1 and 10"));
        let evidence = &tool("docmonk_evidence").unwrap()["inputSchema"];
        let err = validate(
            "docmonk_evidence",
            evidence,
            &json!({
                "query": "q",
                "exclude_attachments": "yes",
                "path_prefixes": ["docs/", 7],
                "limit": 500,
            }),
        )
        .unwrap_err();
        assert!(
            err.contains("exclude_attachments: expected a boolean"),
            "{err}"
        );
        assert!(
            err.contains("path_prefixes: item 1: expected a string"),
            "{err}"
        );
        assert!(err.contains("limit: must be between 1 and 100"), "{err}");
        // Every bounded integer in the catalog declares both bounds.
        for t in tools() {
            for (field, prop) in t["inputSchema"]["properties"].as_object().unwrap() {
                let b = branch(prop);
                if b["type"] == "integer" {
                    assert!(
                        b["minimum"].is_i64() && b["maximum"].is_i64(),
                        "{}.{field}",
                        t["name"]
                    );
                }
            }
            assert_eq!(
                t["inputSchema"]["additionalProperties"], false,
                "{}",
                t["name"]
            );
        }
    }

    /// Audit PH05: a failed call is flagged `isError`.
    #[test]
    fn failed_results_set_is_error() {
        let failed = tool_result(failure("timeout", "too slow"));
        assert_eq!(failed["isError"], true);
        assert_eq!(failed["structuredContent"]["error"]["type"], "timeout");
        let ok = tool_result(json!({"schema_version": "1", "ok": true, "error": null}));
        assert_eq!(ok["isError"], false);
    }

    #[test]
    fn projection_keeps_schema_fields_with_defaults() {
        let schema = &tool("docmonk_documents").unwrap()["outputSchema"];
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
        let schema = &tool("docmonk_status").unwrap()["outputSchema"];
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
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(r["id"], "a");
        let r = handle_line("garbage").unwrap();
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert!(r["id"].is_null());
    }
}
