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

/// Resources and prompts are advertised and offered (upgrade plan A3):
/// the template and prompt lists need no index.
#[test]
fn resources_and_prompts_are_advertised_and_listed() {
    let _home = crate::home_lock();
    let init = call(json!(1), "initialize", json!({}));
    let capabilities = &init["result"]["capabilities"];
    assert!(capabilities["resources"].is_object(), "{init}");
    assert!(capabilities["prompts"].is_object(), "{init}");

    let templates = call(json!(2), "resources/templates/list", json!({}));
    let templates = templates["result"]["resourceTemplates"].as_array().unwrap();
    let uris: Vec<&str> = templates
        .iter()
        .map(|t| t["uriTemplate"].as_str().unwrap())
        .collect();
    assert_eq!(
        uris,
        [
            "docmonk://document/{source_id}/{document_id}{?from}",
            "docmonk://chunk/{source_id}/{chunk_id}",
            "docmonk://symbol/{source_id}/{entity_id}",
        ]
    );
    for t in templates {
        assert!(t["name"].is_string() && t["description"].is_string(), "{t}");
    }

    let prompts = call(json!(3), "prompts/list", json!({}));
    let prompts = prompts["result"]["prompts"].as_array().unwrap();
    assert_eq!(prompts.len(), 2);
    for p in prompts {
        let name = p["name"].as_str().unwrap();
        let argument = p["arguments"][0]["name"].as_str().unwrap();
        let got = call(
            json!(4),
            "prompts/get",
            json!({"name": name, "arguments": {argument: "Dog.bark"}}),
        );
        let message = &got["result"]["messages"][0];
        assert_eq!(message["role"], "user", "{got}");
        assert_eq!(message["content"]["type"], "text", "{got}");
        assert!(
            message["content"]["text"]
                .as_str()
                .unwrap()
                .contains("Dog.bark"),
            "{got}"
        );
        // A missing argument and an unknown prompt are invalid params.
        let missing = call(json!(5), "prompts/get", json!({"name": name}));
        assert_eq!(missing["error"]["code"], INVALID_PARAMS, "{missing}");
    }
    let unknown = call(json!(6), "prompts/get", json!({"name": "nope"}));
    assert_eq!(unknown["error"]["code"], INVALID_PARAMS, "{unknown}");
    assert_eq!(unknown["error"]["message"], "Unknown prompt: nope");

    // A read without a URI is invalid params; a URI that is not ours is
    // "resource not found", decided before anything is opened.
    let no_uri = call(json!(7), "resources/read", json!({}));
    assert_eq!(no_uri["error"]["code"], INVALID_PARAMS, "{no_uri}");
    let foreign = call(
        json!(8),
        "resources/read",
        json!({"uri": "file:///etc/hostname"}),
    );
    assert_eq!(foreign["error"]["code"], docmonk_mcp::RESOURCE_NOT_FOUND);
    assert_eq!(foreign["error"]["data"]["uri"], "file:///etc/hostname");
}
