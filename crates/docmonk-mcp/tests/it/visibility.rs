//! Per-source MCP visibility and the MCP query log (roadmap H5): a source
//! listed in `mcp.hidden_sources` stays searchable from the service (CLI,
//! Admin UI) but no `docmonk_*` tool returns a record from it or names it,
//! including graph traversals into it from a visible source. With
//! `mcp.query_log` every call is logged with redacted arguments.

use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use serde_json::{json, Value};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn tool(name: &str, arguments: Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}});
    docmonk_mcp::handle_line(&msg.to_string()).expect("a reply")["result"]["structuredContent"]
        .clone()
}

#[test]
fn a_hidden_source_never_appears_in_any_mcp_tool_result() {
    let _home = crate::home_lock();
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let open = d.path().join("shop");
    let hidden = d.path().join("vault");
    // The visible shop calls into the hidden payment service, whose docs
    // hold a unique word that must never reach an agent.
    write(
        &open,
        "src/Shop/Cart.cs",
        "namespace Shop { public class Cart { public int Buy(Pay.PayGate g) { return g.Charge(1); } } }\n",
    );
    write(
        &open,
        "docs/refunds.md",
        "# Refunds\n\nRefunds above 500 euros need finance approval.\n",
    );
    write(
        &hidden,
        "src/Pay/PayGate.cs",
        "namespace Pay { public class PayGate { public int Charge(int a) { return a; } } }\n",
    );
    write(
        &hidden,
        "docs/settlement.md",
        "# Settlement\n\nRefunds are settled by the zanzibarquorum ledger at night.\n",
    );
    let mut catalog = docmonk_service::sources::catalog(&home).unwrap();
    let shop = catalog
        .add(open.to_str().unwrap(), vec![], vec![])
        .unwrap()
        .id;
    let vault = catalog
        .add(hidden.to_str().unwrap(), vec![], vec![])
        .unwrap()
        .id;
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    cfg.indexing.relationship_dependencies = vec![format!("{shop} -> {vault}")];
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let selected = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &selected, "index", |_| {}).unwrap();

    std::env::set_var("DOCMONK_HOME", home.root());

    // Visible to agents before it is hidden (the test sees what it hides).
    let before = tool("docmonk_search", json!({"query": "zanzibarquorum"}));
    assert!(before.to_string().contains("zanzibarquorum"), "{before}");

    // Hidden by path; the query log on.
    cfg.mcp.hidden_sources = vec![hidden.to_str().unwrap().to_owned()];
    cfg.mcp.query_log = true;
    docmonk_config::write_user_config(&cfg, &home).unwrap();

    let calls = [
        ("docmonk_search", json!({"query": "zanzibarquorum"})),
        ("docmonk_search", json!({"query": "Refunds"})),
        ("docmonk_search", json!({"query": "PayGate Charge"})),
        (
            "docmonk_evidence",
            json!({"query": "how are refunds settled"}),
        ),
        (
            "docmonk_evidence",
            json!({"query": "Refunds", "source_ids": [vault]}),
        ),
        (
            "docmonk_explore",
            json!({"query": "refunds settlement ledger"}),
        ),
        ("docmonk_explore", json!({"query": "Charge"})),
        ("docmonk_symbol", json!({"name": "PayGate"})),
        ("docmonk_symbol", json!({"name": "Charge"})),
        ("docmonk_callers", json!({"name": "Charge"})),
        ("docmonk_callees", json!({"name": "Buy"})),
        ("docmonk_impact", json!({"name": "Charge"})),
        ("docmonk_documents", json!({})),
        ("docmonk_documents", json!({"source_id": vault})),
        ("docmonk_status", json!({})),
        ("docmonk_status", json!({"source_id": vault})),
        (
            "docmonk_ask",
            json!({"question": "how are refunds settled"}),
        ),
    ];
    let vault_root = hidden.to_string_lossy().into_owned();
    for (name, args) in &calls {
        let mut out = tool(name, args.clone()).to_string();
        // The request's own words echoed back are not a leak.
        let named = args["source_ids"].as_array().cloned().unwrap_or_default();
        let echoes = ["query", "question", "name", "source_id"]
            .iter()
            .filter_map(|k| args[k].as_str())
            .chain(named.iter().filter_map(Value::as_str));
        for echo in echoes {
            out = out.replace(echo, "<request>");
        }
        for leak in [
            vault.as_str(),
            vault_root.as_str(),
            "zanzibarquorum",
            "settlement.md",
            "PayGate.cs",
        ] {
            assert!(!out.contains(leak), "{name} {args} returned {leak}: {out}");
        }
    }
    // The visible source is still served.
    let refunds = tool("docmonk_search", json!({"query": "Refunds"})).to_string();
    assert!(
        refunds.contains("refunds.md") && refunds.contains(&shop),
        "{refunds}"
    );
    // The service itself (CLI, Admin UI) still searches the hidden source.
    let opened = docmonk_service::query::open_sources(&home, None).unwrap();
    assert!(opened.iter().any(|o| o.source.id == vault));

    // One log line per call after the switch, with redacted arguments.
    tool(
        "docmonk_search",
        json!({"query": "DB_PASSWORD=PlantedLogPw0001 failing"}),
    );
    let log = std::fs::read_to_string(home.logs_dir().join(docmonk_mcp::query_log::FILE)).unwrap();
    let lines: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), calls.len() + 2, "{log}");
    assert!(!log.contains("PlantedLogPw0001"), "{log}");
    // Only arguments the caller wrote may name the hidden source.
    for l in &lines {
        assert!(!l["sources"].to_string().contains(&vault), "{l}");
    }
    let refunds_line = lines
        .iter()
        .find(|l| l["tool"] == "docmonk_search" && l["arguments"]["query"] == "Refunds")
        .unwrap();
    assert!(
        refunds_line["result_count"].as_u64().unwrap() >= 1,
        "{refunds_line}"
    );
    assert_eq!(refunds_line["sources"], json!([shop]));
}

fn rpc(method: &str, params: Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    docmonk_mcp::handle_line(&msg.to_string()).expect("a reply")
}

/// Resources follow the same rule (upgrade plan A3, ADR 0045): a hidden
/// source is not listed, and none of its URIs can be read, including ones
/// an agent learned while the source was still visible.
#[test]
fn a_hidden_source_cannot_be_listed_or_read_as_a_resource() {
    let _home = crate::home_lock();
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let open = d.path().join("shop");
    let hidden = d.path().join("vault");
    write(
        &open,
        "docs/refunds.md",
        "# Refunds\n\nRefunds above 500 euros need finance approval.\n",
    );
    write(
        &hidden,
        "src/Pay/PayGate.cs",
        "namespace Pay { public class PayGate { public int Charge(int a) { return a; } } }\n",
    );
    write(
        &hidden,
        "docs/settlement.md",
        "# Settlement\n\nRefunds are settled by the zanzibarquorum ledger at night.\n",
    );
    let mut catalog = docmonk_service::sources::catalog(&home).unwrap();
    let shop = catalog
        .add(open.to_str().unwrap(), vec![], vec![])
        .unwrap()
        .id;
    let vault = catalog
        .add(hidden.to_str().unwrap(), vec![], vec![])
        .unwrap()
        .id;
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let selected = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &selected, "index", |_| {}).unwrap();
    std::env::set_var("DOCMONK_HOME", home.root());

    // While visible: both sources are listed and the vault is readable.
    let listed = rpc("resources/list", json!({})).to_string();
    assert!(
        listed.contains(&shop) && listed.contains(&vault),
        "{listed}"
    );
    let record = |query: &str, kind: &str| -> String {
        let out = tool("docmonk_evidence", json!({"query": query}));
        out["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["kind"] == kind && e["source_id"] == vault.as_str())
            .unwrap_or_else(|| panic!("no {kind} evidence for {query}: {out}"))["record_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let chunk_uri = format!(
        "docmonk://chunk/{vault}/{}",
        record("zanzibarquorum ledger", "document")
    );
    let symbol_uri = format!(
        "docmonk://symbol/{vault}/{}",
        record("PayGate Charge", "entity")
    );
    let chunk = rpc("resources/read", json!({"uri": chunk_uri}));
    let chunk_text = chunk["result"]["contents"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{chunk}"));
    assert!(chunk_text.contains("zanzibarquorum"), "{chunk}");
    let document_uri = chunk_text
        .split_whitespace()
        .find(|w| w.starts_with("docmonk://document/"))
        .unwrap()
        .to_owned();
    let source_uri = format!("docmonk://source/{vault}");
    let uris = [source_uri, chunk_uri, symbol_uri, document_uri];
    for uri in &uris {
        let reply = rpc("resources/read", json!({"uri": uri}));
        assert!(
            reply["result"]["contents"][0]["text"].is_string(),
            "{uri}: {reply}"
        );
    }

    // Hidden (by id here; the other test hides by path).
    cfg.mcp.hidden_sources = vec![vault.clone()];
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let listed = rpc("resources/list", json!({}));
    let text = listed.to_string();
    assert!(text.contains(&shop), "{text}");
    assert!(
        !text.contains(&vault) && !text.contains(hidden.to_str().unwrap()),
        "{text}"
    );
    assert_eq!(listed["result"]["resources"].as_array().unwrap().len(), 1);
    for uri in &uris {
        let reply = rpc("resources/read", json!({"uri": uri}));
        // Exactly the answer an unknown source gets.
        assert_eq!(
            reply["error"],
            json!({"code": -32002, "message": "Resource not found", "data": {"uri": uri}}),
            "{uri}: {reply}"
        );
        assert!(!reply.to_string().contains("zanzibarquorum"), "{reply}");
    }
    let unknown = rpc(
        "resources/read",
        json!({"uri": "docmonk://source/src_nope"}),
    );
    assert_eq!(unknown["error"]["code"], -32002);
    // The visible source still reads.
    let shop_uri = format!("docmonk://source/{shop}");
    let reply = rpc("resources/read", json!({"uri": shop_uri}));
    assert!(
        reply["result"]["contents"][0]["text"].is_string(),
        "{reply}"
    );
}
