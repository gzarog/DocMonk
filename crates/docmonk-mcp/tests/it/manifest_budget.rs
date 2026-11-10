//! The size of the `tools/list` result (upgrade plan A1). An agent that
//! loads the manifest whole pays for it in context on every session, so
//! the default listing has a byte budget and this test fails when the
//! manifest outgrows it.
//!
//! `cargo test -p docmonk-mcp --test it -- manifest_budget:: --nocapture`
//! prints the size of every tool in both listings.

use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use serde_json::{json, Value};

/// The largest default `tools/list` result, in bytes of compact JSON.
/// Raise it only with a reason in the PR: every byte is agent context.
const MANIFEST_BUDGET_BYTES: usize = 20_000;

/// A rough token count: about four bytes of JSON per token.
fn approx_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

fn bytes(v: &Value) -> usize {
    serde_json::to_string(v).unwrap().len()
}

/// The `tools/list` result under a fresh home with `configure` applied.
fn tools_list(configure: impl FnOnce(&mut DocMonkConfig)) -> Value {
    let d = tempfile::tempdir().unwrap();
    let home = Home::new(d.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    configure(&mut cfg);
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    std::env::set_var("DOCMONK_HOME", home.root());
    let reply =
        docmonk_mcp::handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#)
            .expect("a reply");
    reply["result"].clone()
}

fn report(label: &str, result: &Value) -> usize {
    let total = bytes(result);
    eprintln!(
        "{label}: {total} bytes, about {} tokens",
        approx_tokens(total)
    );
    for tool in result["tools"].as_array().unwrap() {
        let size = bytes(tool);
        eprintln!(
            "  {:<20} {:>6} bytes  ~{:>5} tokens  (input {:>5}, output {:>5}, description {:>4})",
            tool["name"].as_str().unwrap(),
            size,
            approx_tokens(size),
            bytes(&tool["inputSchema"]),
            bytes(&tool["outputSchema"]),
            tool["description"].as_str().unwrap().len(),
        );
    }
    total
}

#[test]
fn the_default_manifest_fits_its_budget() {
    let _home = crate::home_lock();
    let minimal = tools_list(|_| {});
    let full = tools_list(|cfg| cfg.mcp.full_output_schemas = true);
    let size = report("default tools/list", &minimal);
    report("tools/list with mcp.full_output_schemas", &full);
    assert!(
        size <= MANIFEST_BUDGET_BYTES,
        "the default tools/list result is {size} bytes, over the budget of \
         {MANIFEST_BUDGET_BYTES}: shorten descriptions or schemas"
    );
}

#[test]
fn the_full_schemas_are_served_behind_the_config_key() {
    let _home = crate::home_lock();
    let minimal = tools_list(|_| {});
    let full = tools_list(|cfg| cfg.mcp.full_output_schemas = true);
    assert_eq!(
        full["tools"],
        Value::Array(docmonk_mcp::tools().to_vec()),
        "the config key serves the catalog unchanged"
    );
    let minimal = minimal["tools"].as_array().unwrap();
    let full = full["tools"].as_array().unwrap();
    assert_eq!(minimal.len(), full.len());
    for (small, big) in minimal.iter().zip(full) {
        let name = big["name"].as_str().unwrap();
        // Only the output schema differs between the two listings.
        for field in ["name", "description", "inputSchema", "annotations"] {
            assert_eq!(small.get(field), big.get(field), "{name}.{field}");
        }
        let keys = |schema: &Value| -> Vec<String> {
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(
            keys(&small["outputSchema"]),
            keys(&big["outputSchema"]),
            "{name}: the minimal schema names every top-level key"
        );
        // A failed result carries the envelope only, so the minimal
        // schema requires nothing else.
        for key in small["outputSchema"]["required"]
            .as_array()
            .into_iter()
            .flatten()
        {
            assert!(
                ["schema_version", "ok", "error"].contains(&key.as_str().unwrap()),
                "{name}: {key}"
            );
            assert!(
                big["outputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(key),
                "{name}: {key}"
            );
        }
        // Nothing nested survives: no definitions, no property detail.
        assert!(small["outputSchema"].get("$defs").is_none(), "{name}");
        for (key, prop) in small["outputSchema"]["properties"].as_object().unwrap() {
            assert_eq!(
                prop.as_object().unwrap().keys().collect::<Vec<_>>(),
                ["type"],
                "{name}.{key}"
            );
        }
    }
}

fn type_matches(value: &Value, ty: &str) -> bool {
    match ty {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// Every recorded tool result (shaped by the full schema), failed ones
/// included, is valid against the minimal schema a client is given by
/// default.
#[test]
fn recorded_results_are_valid_against_the_minimal_schemas() {
    let golden: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/expected/mcp.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let mut checked = 0;
    for record in golden.as_array().unwrap() {
        let structured = &record["response"]["result"]["structuredContent"];
        let Some(result) = structured.as_object() else {
            continue;
        };
        let name = record["request"]["params"]["name"].as_str().unwrap();
        let listed = docmonk_mcp::listed_tools(false)
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"));
        let schema = &listed["outputSchema"];
        for key in schema["required"].as_array().into_iter().flatten() {
            assert!(result.contains_key(key.as_str().unwrap()), "{name}: {key}");
        }
        for (key, value) in result {
            let ty = &schema["properties"][key]["type"];
            let allowed: Vec<&str> = match ty {
                Value::String(one) => vec![one.as_str()],
                Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => panic!("{name}: result key {key} is not in the minimal schema"),
            };
            assert!(
                allowed.iter().any(|t| type_matches(value, t)),
                "{name}.{key}: {value} is not {}",
                json!(allowed)
            );
        }
        checked += 1;
    }
    assert!(
        checked >= 20,
        "only {checked} recorded tool results checked"
    );
}
