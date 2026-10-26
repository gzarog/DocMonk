//! JSON-RPC and MCP tool-error semantics (audit PH05: T05.1, T05.2).
//!
//! Everything here is decided before any source is opened, so no index
//! or config is needed.

use std::io::Cursor;

use docmonk_mcp::{
    handle_line, read_bounded_line, too_large, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND,
    PARSE_ERROR,
};
use serde_json::{json, Value};

fn call(id: Value, method: &str, params: Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    handle_line(&msg.to_string()).expect("a request gets a reply")
}

fn tool(name: &str, arguments: Value) -> Value {
    call(
        json!(1),
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )["result"]
        .clone()
}

#[test]
fn malformed_messages_get_standard_json_rpc_errors() {
    let _home = crate::home_lock();
    for (line, code) in [
        ("{not json", PARSE_ERROR),
        ("[1,2]", INVALID_REQUEST),
        (r#"{"jsonrpc":"2.0","id":3}"#, INVALID_REQUEST),
        (
            r#"{"jsonrpc":"1.0","id":3,"method":"ping"}"#,
            INVALID_REQUEST,
        ),
        (
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#,
            INVALID_REQUEST,
        ),
        (
            r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#,
            METHOD_NOT_FOUND,
        ),
        (
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":"x"}"#,
            INVALID_PARAMS,
        ),
    ] {
        let r = handle_line(line).unwrap_or_else(|| panic!("no reply to {line}"));
        assert_eq!(r["error"]["code"], code, "{line}: {r}");
        assert_eq!(r["jsonrpc"], "2.0");
    }
    // Parse errors and unusable ids answer with id null; others echo it.
    assert!(handle_line("{not json").unwrap()["id"].is_null());
    assert_eq!(
        handle_line(r#"{"jsonrpc":"2.0","id":"q","method":"nope"}"#).unwrap()["id"],
        "q"
    );
}

#[test]
fn notifications_and_client_responses_get_no_reply() {
    let _home = crate::home_lock();
    assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    assert!(handle_line(r#"{"jsonrpc":"2.0","method":"anything/unknown"}"#).is_none());
    assert!(handle_line(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#).is_none());
}

#[test]
fn every_tool_failure_sets_is_error_with_a_typed_error() {
    let _home = crate::home_lock();
    // Unknown tool and validation failures are rejected before any source
    // is opened.
    let r = tool("docmonk_nope", json!({}));
    assert_eq!(r["isError"], true);
    for (name, args, needle) in [
        (
            "docmonk_evidence",
            json!({"query": "q", "source_ids": "src_a"}),
            "source_ids: expected an array",
        ),
        (
            "docmonk_evidence",
            json!({"query": "q", "source_ids": ["a", 1]}),
            "source_ids: item 1",
        ),
        (
            "docmonk_evidence",
            json!({"query": "q", "kinds": [true]}),
            "kinds: item 0",
        ),
        (
            "docmonk_evidence",
            json!({"query": "q", "document_ids": {}}),
            "document_ids: expected an array",
        ),
        (
            "docmonk_evidence",
            json!({"query": "q", "exclude_attachments": 1}),
            "exclude_attachments",
        ),
        (
            "docmonk_evidence",
            json!({"query": "q", "source_id": ["a"]}),
            "source_id: unknown argument",
        ),
        (
            "docmonk_search",
            json!({"query": "q", "limit": 0}),
            "limit: must be between 1",
        ),
        (
            "docmonk_search",
            json!({"query": "q", "limit": -1}),
            "limit: must be between 1",
        ),
        (
            "docmonk_search",
            json!({"query": "q", "limit": 1e12}),
            "limit: must be between 1",
        ),
        (
            "docmonk_explore",
            json!({"query": "q", "max_chars": 99_999_999}),
            "max_chars",
        ),
        ("docmonk_search", json!({}), "query: required"),
        ("docmonk_search", json!("q"), "expected an object"),
    ] {
        let r = tool(name, args.clone());
        assert_eq!(r["isError"], true, "{name} {args}: {r}");
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(needle), "{name} {args}: {text}");
    }
}

#[test]
fn valid_nullable_filters_pass_validation() {
    let _home = crate::home_lock();
    // Null filters mean "no restriction requested" and are accepted; the
    // call then proceeds past validation (it may fail later for lack of a
    // configured home, but never with a validation error).
    let r = tool(
        "docmonk_evidence",
        json!({"query": "q", "source_ids": null, "path_prefixes": null, "limit": null}),
    );
    let text = r["content"][0]["text"].as_str().unwrap_or_default();
    assert!(!text.contains("invalid arguments"), "{text}");
}

#[test]
fn oversized_messages_are_rejected_and_the_stream_stays_in_sync() {
    let _home = crate::home_lock();
    let big = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(10_000));
    let input = format!("{big}{{\"next\":1}}\n");
    let mut r = Cursor::new(input.into_bytes());
    assert_eq!(read_bounded_line(&mut r, 1024).unwrap(), Some(Err(())));
    assert_eq!(
        read_bounded_line(&mut r, 1024).unwrap(),
        Some(Ok("{\"next\":1}".to_owned()))
    );
    assert_eq!(read_bounded_line(&mut r, 1024).unwrap(), None);
    let reply = too_large(1024);
    assert_eq!(reply["error"]["code"], INVALID_REQUEST);
    assert!(reply["id"].is_null());
}
