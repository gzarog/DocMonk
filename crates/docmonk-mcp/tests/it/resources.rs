//! MCP resources and prompts (upgrade plan A3, ADR 0045): documents,
//! chunks and symbols are readable by `docmonk://` URI from the published
//! build, long documents page, unknown URIs are "resource not found", and
//! every read is in the MCP query log.

use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use serde_json::{json, Value};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn rpc(method: &str, params: Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    docmonk_mcp::handle_line(&msg.to_string()).expect("a reply")
}

fn tool(name: &str, arguments: Value) -> Value {
    rpc("tools/call", json!({"name": name, "arguments": arguments}))["result"]["structuredContent"]
        .clone()
}

/// The text of a resource, which must be readable.
fn read(uri: &str) -> String {
    let reply = rpc("resources/read", json!({"uri": uri}));
    let contents = reply["result"]["contents"]
        .as_array()
        .unwrap_or_else(|| panic!("{uri}: {reply}"));
    assert_eq!(contents.len(), 1, "{reply}");
    assert_eq!(contents[0]["uri"], uri);
    // Only what the protocol defines reaches the client.
    let mut keys: Vec<&String> = contents[0].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["mimeType", "text", "uri"], "{reply}");
    contents[0]["text"].as_str().unwrap().to_owned()
}

fn assert_not_found(uri: &str) {
    let reply = rpc("resources/read", json!({"uri": uri}));
    assert_eq!(
        reply["error"],
        json!({"code": -32002, "message": "Resource not found", "data": {"uri": uri}}),
        "{uri}: {reply}"
    );
}

/// The `docmonk://document/...` URI a chunk resource names.
fn document_uri_in(chunk_text: &str) -> String {
    chunk_text
        .split_whitespace()
        .find(|w| w.starts_with("docmonk://document/"))
        .unwrap_or_else(|| panic!("no document URI in {chunk_text}"))
        .to_owned()
}

fn evidence_record(query: &str, kind: &str) -> Value {
    let out = tool("docmonk_evidence", json!({"query": query}));
    out["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} evidence for {query}: {out}"))
        .clone()
}

const SECTIONS: usize = 40;

#[test]
fn documents_chunks_and_symbols_are_readable_by_uri() {
    let _home = crate::home_lock();
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let root = d.path().join("shop");
    write(
        &root,
        "docs/guide.md",
        "# Guide\n\n## Refunds\n\nRefunds above 500 euros need approval by the quillfeather \
         desk.\n\n## Shipping\n\nParcels leave the warehouse on weekdays before noon.\n",
    );
    let mut long = String::from("# Handbook\n");
    for i in 0..SECTIONS {
        long.push_str(&format!(
            "\n## Section {i}\n\nThe zebramarker{i:02} procedure is described here. {}\n",
            "It takes several careful steps and a second reviewer signs it off. ".repeat(4)
        ));
    }
    write(&root, "docs/handbook.md", &long);
    write(
        &root,
        "src/shop/cart.py",
        "def cart_total(items):\n    total = 0\n    for item in items:\n        total += \
         item.price\n    return total\n",
    );
    let mut catalog = docmonk_service::sources::catalog(&home).unwrap();
    let source = catalog
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap()
        .id;
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.mcp.query_log = true;
    // The smallest result budget, so the handbook needs several reads.
    cfg.mcp.max_result_bytes = 4096;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let selected = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &selected, "index", |_| {}).unwrap();
    std::env::set_var("DOCMONK_HOME", home.root());

    // The source is listed, and reads as its id, path and published build.
    let listed = rpc("resources/list", json!({}));
    let source_uri = format!("docmonk://source/{source}");
    assert_eq!(
        listed["result"]["resources"],
        json!([{
            "uri": source_uri,
            "name": "shop",
            "title": "Source shop",
            "description": format!("Indexed source at {}.", catalog.get(&source).unwrap().path),
            "mimeType": "application/json",
        }]),
        "{listed}"
    );
    let about: Value = serde_json::from_str(&read(&source_uri)).unwrap();
    assert_eq!(about["source_id"], source.as_str());

    // A chunk, by the record id of a document evidence entry.
    let cited = evidence_record("quillfeather desk refunds", "document");
    let build = cited["build_id"].as_str().unwrap();
    assert_eq!(
        about["build_id"], build,
        "reads are pinned to the published build"
    );
    let chunk_uri = format!(
        "docmonk://chunk/{source}/{}",
        cited["record_id"].as_str().unwrap()
    );
    let chunk = read(&chunk_uri);
    assert!(chunk.contains("quillfeather desk"), "{chunk}");
    assert!(chunk.contains("guide.md"), "{chunk}");
    assert!(chunk.contains(&format!("(build {build})")), "{chunk}");
    assert!(chunk.contains("Guide > Refunds"), "{chunk}");

    // Its whole document, in reading order.
    let guide = read(&document_uri_in(&chunk));
    let refunds = guide.find("quillfeather desk").expect(&guide);
    let shipping = guide.find("before noon").expect(&guide);
    assert!(refunds < shipping, "{guide}");
    assert!(!guide.contains("[continues at"), "{guide}");

    // A long document pages: each read ends with the URI that continues it.
    let cited = evidence_record("zebramarker07 procedure", "document");
    let mut uri = document_uri_in(&read(&format!(
        "docmonk://chunk/{source}/{}",
        cited["record_id"].as_str().unwrap()
    )));
    let mut pages = Vec::new();
    loop {
        let page = read(&uri);
        let next = page
            .lines()
            .find_map(|l| l.strip_prefix("[continues at "))
            .map(|l| l.trim_end_matches(']').to_owned());
        pages.push(page);
        match next {
            Some(next) => {
                assert_ne!(next, uri, "a continuation must advance");
                uri = next;
            }
            None => break,
        }
        assert!(pages.len() <= SECTIONS * 3, "the document never ends");
    }
    assert!(pages.len() >= 3, "{} pages", pages.len());
    for page in &pages {
        // The budget is 3/4 of mcp.max_result_bytes, plus the closing line.
        assert!(page.len() <= 3072 + 200, "{} bytes", page.len());
    }
    let whole = pages.concat();
    let mut last = 0;
    for i in 0..SECTIONS {
        let marker = format!("zebramarker{i:02} procedure");
        assert_eq!(whole.matches(&marker).count(), 1, "{marker}");
        let at = whole.find(&marker).unwrap();
        assert!(at > last, "{marker} out of order");
        last = at;
    }

    // A symbol, by the entity id an explore result carries.
    let explored = tool("docmonk_explore", json!({"query": "cart_total"}));
    let entity = explored["entities"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| {
            e["qualified_name"]
                .as_str()
                .is_some_and(|q| q.ends_with("cart_total"))
        })
        .unwrap_or_else(|| panic!("{explored}"));
    let symbol_uri = format!(
        "docmonk://symbol/{source}/{}",
        entity["entity_id"].as_str().unwrap()
    );
    let symbol = read(&symbol_uri);
    assert!(symbol.contains("cart_total (python function)"), "{symbol}");
    assert!(symbol.contains("cart.py:1-5"), "{symbol}");
    assert!(symbol.contains("total += item.price"), "{symbol}");

    // Whatever names nothing readable is the same "not found".
    for uri in [
        "docmonk://source/src_nope".to_owned(),
        format!("docmonk://chunk/{source}/nope"),
        format!("docmonk://symbol/{source}/nope"),
        format!("docmonk://document/{source}/nope"),
        format!("{}?from=100000", document_uri_in(&chunk)),
        format!("docmonk://file/{source}/x"),
        "file:///etc/hostname".to_owned(),
    ] {
        assert_not_found(&uri);
    }
    let reply = rpc("resources/read", json!({}));
    assert_eq!(reply["error"]["code"], -32602, "{reply}");

    // Every list and read is in the query log; a refused one too.
    let log = std::fs::read_to_string(home.logs_dir().join(docmonk_mcp::query_log::FILE)).unwrap();
    let lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(lines.iter().any(|l| l["tool"] == "resources/list"), "{log}");
    let line = lines
        .iter()
        .find(|l| l["tool"] == "resources/read" && l["arguments"]["uri"] == symbol_uri.as_str())
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(line["ok"], true);
    assert_eq!(line["result_count"], 1);
    assert_eq!(line["sources"], json!([source]));
    let refused = lines
        .iter()
        .find(|l| {
            l["tool"] == "resources/read"
                && l["arguments"]["uri"] == format!("docmonk://chunk/{source}/nope").as_str()
        })
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(refused["ok"], false);
}
