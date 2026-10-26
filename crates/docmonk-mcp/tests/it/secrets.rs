//! Planted secrets never leave the index (ADR 0042, roadmap H4): the
//! `fixtures/secrets` corpus (code, appsettings JSON, Markdown, a PDF, an
//! email with an attachment) plus generated token-format secrets is indexed,
//! and every MCP tool, and the service calls behind the CLI and the Admin
//! UI, is asked about them. No planted value may appear in any answer, the
//! redacted lines must still be found by their key names, and `.netrc` and
//! `*.tfstate` files must not be indexed at all.

use std::path::Path;

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use serde_json::{json, Value};

/// Values committed in `fixtures/secrets`.
const COMMITTED: &[&str] = &[
    "PlantedPyPw1035",
    "PlantedClientSecret2246",
    "PlantedSqlPw8813",
    "PlantedJsonKey9924",
    "PlantedUrlPw5512",
    "PlantedEnvPw6620",
    "PlantedKeyBody",
    "PlantedNetrcPw3301",
    "PlantedTfstatePw4410",
    "PlantedMailKey4402",
    "PlantedMailBodyPw5530",
    "PlantedPdfPw7731",
    "PlantedFnToken6655",
];

/// Token-format secrets, assembled here so the repository holds no
/// token-shaped string.
fn generated() -> Vec<String> {
    vec![
        ["AKIA", "PLANTED000000042"].concat(),
        ["ghp_", "Planted", &"7".repeat(29)].concat(),
        ["xoxb-", "4242424242-PlantedSlackToken"].concat(),
        [
            "eyJhbGciOiJIUzI1NiJ9",
            ".eyJzdWIiOiJQbGFudGVkSnd0In0",
            ".PlantedJwtSignature0042",
        ]
        .concat(),
    ]
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// Every string in an answer, without echoes of the request: strings that
/// are (case-insensitively) part of the query, such as the query field or
/// its parsed terms, and the query quoted in a summary sentence. Queries
/// that name a planted value only prove nothing matches it; the key-name
/// queries, which never contain one, catch values leaked with context.
fn answer_text(v: &Value, echo: &str) -> String {
    let mut out = String::new();
    let mut walk = vec![v];
    while let Some(v) = walk.pop() {
        match v {
            Value::String(s) if echo.is_empty() || !echo.contains(&s.to_lowercase()) => {
                let lower = s.to_lowercase();
                if echo.is_empty() || !lower.contains(echo) {
                    out.push_str(s);
                } else {
                    // Drop the quoted query, keep everything around it.
                    out.push_str(&lower.replace(echo, ""));
                }
                out.push('\n');
            }
            Value::Array(a) => walk.extend(a),
            Value::Object(o) => walk.extend(o.values()),
            _ => {}
        }
    }
    out
}

fn tool(name: &str, arguments: Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": arguments}});
    docmonk_mcp::handle_line(&msg.to_string()).expect("a reply")["result"].clone()
}

#[test]
fn no_planted_secret_is_retrievable_through_any_read_surface() {
    let _home = crate::home_lock();
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    // The text layer is enough for the planted PDF; no layout models.
    cfg.documents.pdf_mode = "fast".into();
    docmonk_config::write_user_config(&cfg, &home).unwrap();

    let root = d.path().join("corpus");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/secrets");
    copy_dir(&fixtures, &root);
    let tokens = generated();
    std::fs::write(
        root.join("src/tokens.py"),
        format!(
            "AWS_ACCESS_KEY_ID = \"{}\"\nGITHUB = \"{}\"\nSLACK_BOT = \"{}\"\nSESSION = \"{}\"\n",
            tokens[0], tokens[1], tokens[2], tokens[3]
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("docs/tokens.md"),
        format!(
            "# Tokens\n\nThe CI bot uses {} and posts with {}.\nA sample session: {}\n",
            tokens[1], tokens[2], tokens[3]
        ),
    )
    .unwrap();

    let mut catalog = docmonk_service::sources::catalog(&home).unwrap();
    let source = catalog
        .add(root.to_str().unwrap(), vec![], vec![])
        .unwrap()
        .id;
    let selected = docmonk_service::indexing::selected_sources(&home, None).unwrap();
    docmonk_service::indexing::index_sources(&home, &selected, "index", |_| {}).unwrap();

    std::env::set_var("DOCMONK_HOME", home.root());

    let mut answers = Vec::new();
    let mut ask = |name: &str, args: Value| {
        let out = tool(name, args.clone());
        let echo = args
            .get("query")
            .or_else(|| args.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        answers.push((
            format!("{name} {args}"),
            answer_text(&out["structuredContent"], &echo),
        ));
        out
    };
    let mut queries: Vec<String> = COMMITTED.iter().map(|s| s.to_string()).collect();
    queries.extend(tokens.iter().cloned());
    queries.extend(
        [
            "DB_PASSWORD",
            "password",
            "client_secret",
            "ApiKey",
            "connection string Password",
            "postgres ops db01",
            "SMTP_PASSWORD",
            "deploy key private key",
            "RSA PRIVATE KEY",
            "api_key handover notes",
            "AWS_ACCESS_KEY_ID",
            "GITHUB token",
            "SLACK_BOT",
            "session token",
            "netrc machine login",
            "tfstate db_password",
            "billing handover",
            "operator billing console login",
        ]
        .map(String::from),
    );
    for q in &queries {
        ask("docmonk_search", json!({"query": q, "limit": 20}));
        ask("docmonk_evidence", json!({"query": q, "limit": 20}));
        ask("docmonk_explore", json!({"query": q}));
    }
    for name in [
        "DB_PASSWORD",
        "client_secret",
        "connect",
        "AWS_ACCESS_KEY_ID",
    ] {
        ask("docmonk_symbol", json!({"name": name}));
    }
    let docs = ask("docmonk_documents", json!({"source_id": source}));
    ask("docmonk_status", json!({"source_id": source}));

    let mut planted: Vec<String> = COMMITTED.iter().map(|s| s.to_string()).collect();
    planted.extend(tokens);
    for (call, out) in &answers {
        let out_lower = out.to_lowercase();
        for p in &planted {
            assert!(
                !out_lower.contains(&p.to_lowercase()),
                "{call} returned {p}: {out}"
            );
        }
    }

    // The redacted lines are still indexed and found by their key names.
    let found = |q: &str| {
        answers
            .iter()
            .filter(|(c, _)| c.starts_with("docmonk_search") && c.contains(q))
            .any(|(_, out)| out.contains("[REDACTED:"))
    };
    for q in [
        "postgres ops db01",
        "RSA PRIVATE KEY",
        "session token",
        "api_key handover notes",
        "billing handover",
        "operator billing console login",
    ] {
        assert!(found(q), "no redacted hit for {q}");
    }

    // Secret files are skipped by name; every planted document is indexed.
    let listing = docs.to_string();
    assert!(
        !listing.contains(".netrc") && !listing.contains("deploy.tfstate"),
        "{listing}"
    );
    for doc in ["runbook.md", "handover.pdf", "handover.eml", "tokens.md"] {
        assert!(listing.contains(doc), "{doc} not indexed: {listing}");
    }
}
