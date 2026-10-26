//! An agent follows an answer to its sources with MCP alone (roadmap P8):
//! evidence → a citation → `docmonk_read` (the verified span, numbered,
//! with context) → `docmonk_references` (where the cited symbol is used)
//! → `docmonk_links` (the specification linked to it). After the cited
//! file changes and is reindexed, the old citation is refused, and result
//! budgets cut long answers with explicit truncation markers.

use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use serde_json::{json, Map, Value};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn tool(name: &str, arguments: Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}});
    let reply = docmonk_mcp::handle_line(&msg.to_string()).expect("a reply");
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{name}: {reply}");
    result["structuredContent"].clone()
}

fn index(home: &Home) {
    let s = docmonk_service::indexing::selected_sources(home, None).unwrap();
    docmonk_service::indexing::index_sources(home, &s, "index", |_| {}).unwrap();
}

const REFUNDS: &str = "namespace Billing {
    public class RefundPolicy {
        // Refunds above the limit need finance approval.
        public bool NeedsApproval(int amountEuros) {
            var limit = 500;
            return amountEuros > limit;
        }
    }
    public class Desk {
        public bool Check(RefundPolicy p) { return p.NeedsApproval(900); }
    }
}
";

#[test]
fn an_agent_follows_an_answer_to_its_span_references_and_links() {
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let root = d.path().join("billing");
    write(&root, "src/Billing/RefundPolicy.cs", REFUNDS);
    write(
        &root,
        "docs/refunds.md",
        "# Refund approval\n\nRefunds above 500 euros need finance approval; \
         RefundPolicy.NeedsApproval implements the rule.\n",
    );
    docmonk_service::sources::catalog(&home)
        .unwrap()
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap();
    index(&home);
    // The only test in this binary: the MCP server reads its home from the
    // environment, like the real `docmonk mcp` process.
    std::env::set_var("DOCMONK_HOME", home.root());

    // 1. The answer and its citations.
    let ev = tool(
        "docmonk_evidence",
        json!({"query": "NeedsApproval refund limit", "limit": 10}),
    );
    let evidence = ev["evidence"].as_array().unwrap().clone();
    let code = evidence
        .iter()
        .find(|e| e["valid"] == true && e["path"].as_str().unwrap().ends_with("RefundPolicy.cs"))
        .unwrap_or_else(|| panic!("no code citation: {ev}"))
        .clone();
    let doc = evidence
        .iter()
        .find(|e| e["valid"] == true && e["kind"] == "document")
        .unwrap_or_else(|| panic!("no document citation: {ev}"))
        .clone();

    // 2. The cited code span, verified, numbered, the cited lines marked.
    // The evidence entry is passed back unchanged.
    let mut args: Map<String, Value> = code.as_object().unwrap().clone();
    args.insert("context_lines".into(), json!(2));
    let read = tool("docmonk_read", Value::Object(args));
    assert_eq!(read["valid"], true, "{read}");
    let text = read["text"].as_str().unwrap();
    assert!(text.contains("NeedsApproval"), "{text}");
    assert!(text.lines().any(|l| l.starts_with('>')), "{text}");
    assert!(read["path"].as_str().unwrap().ends_with("RefundPolicy.cs"));

    // The cited document chunk comes with its heading context.
    let read_doc = tool("docmonk_read", doc.clone());
    assert_eq!(read_doc["valid"], true, "{read_doc}");
    assert!(
        read_doc["text"]
            .as_str()
            .unwrap()
            .contains("finance approval"),
        "{read_doc}"
    );
    assert!(read_doc["context"].is_object(), "{read_doc}");

    // 3. Where the cited symbol is used.
    let refs = tool("docmonk_references", json!({"name": "NeedsApproval"}));
    let edges = refs["edges"].as_array().unwrap();
    // The call in Desk.Check (line 10).
    assert!(
        edges.iter().any(|e| e["relationship_type"] == "calls"
            && e["source_location"]
                .as_str()
                .unwrap()
                .ends_with("RefundPolicy.cs:10")),
        "{refs}"
    );

    // 4. The specification linked to it, and back.
    let links = tool("docmonk_links", json!({"entity": "NeedsApproval"}));
    let rows = links["links"].as_array().unwrap();
    assert!(
        rows.iter().any(|l| l.to_string().contains("refunds.md")),
        "{links}"
    );
    let back = tool("docmonk_links", json!({"document": "refunds.md"}));
    assert!(!back["links"].as_array().unwrap().is_empty(), "{back}");

    // 5. The code changes: the old citation is refused, not answered from
    // stale text.
    write(
        &root,
        "src/Billing/RefundPolicy.cs",
        &REFUNDS.replace("var limit = 500;", "var limit = 750;"),
    );
    index(&home);
    let stale = tool("docmonk_read", code);
    assert_eq!(stale["valid"], false, "{stale}");
    assert!(stale["text"].is_null(), "{stale}");
    assert!(stale["reason"].is_string(), "{stale}");

    // 6. Result budgets: a long answer is cut and says so.
    cfg.mcp.max_result_items = 1;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let cut = tool("docmonk_search", json!({"query": "refund", "limit": 20}));
    assert_eq!(cut["results"].as_array().unwrap().len(), 1, "{cut}");
    assert_eq!(cut["truncated"], true, "{cut}");
    assert_eq!(cut["truncation"][0]["path"], "results", "{cut}");
}
