//! Query commands end to end: text modes, errors and `link …` (the
//! retrieval crate's tests cover the `--json` data).

use std::path::Path;
use std::process::{Command, Output};

fn ragmonk(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ragmonk"))
        .args(args)
        .env("RAGMONK_HOME", home)
        .env("RAGMONK_SEARCH__SEMANTIC", "false")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    assert!(o.status.success(), "{o:?}");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn json(home: &Path, args: &[&str]) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&out(&ragmonk(home, args))).unwrap()["data"].clone()
}

#[test]
fn queries_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("billing.py"),
        "def charge(amount):\n    return audit(amount)\n\n\ndef audit(amount):\n    return amount\n",
    )
    .unwrap();
    std::fs::write(
        src.join("guide.md"),
        "# Billing\n\nIntro text.\n\n## Charging\n\nThe charge function bills.\n\nMore about charge.\n",
    )
    .unwrap();
    out(&ragmonk(&home, &["init"]));
    out(&ragmonk(&home, &["source", "add", src.to_str().unwrap()]));
    out(&ragmonk(&home, &["index"]));

    // symbol / callers / callees / references
    let s = json(&home, &["symbol", "charge", "--json"]);
    assert_eq!(s["matches"][0]["qualified_name"], "billing.charge");
    let src_abs = s["matches"][0]["source_path"].as_str().unwrap().to_owned();
    let t = out(&ragmonk(&home, &["symbol", "charge"]));
    assert!(t.starts_with("Kind") && t.contains("billing.charge"), "{t}");
    let c = json(&home, &["callers", "audit", "--json"]);
    let loc = c["edges"][0]["source_location"].as_str().unwrap();
    assert!(loc.starts_with(&src_abs) && loc.ends_with(":2"), "{loc}");
    assert!(out(&ragmonk(&home, &["callees", "charge"])).contains("calls"));
    assert!(out(&ragmonk(&home, &["references", "nothing_here"]))
        .contains("No entities found matching 'nothing_here'."));
    let o = ragmonk(&home, &["callers", "x", "--max-depth", "11"]);
    assert_eq!(o.status.code(), Some(2), "range checked");

    // impact / explore text views
    let t = out(&ragmonk(&home, &["impact", "audit"]));
    assert!(
        t.contains("Callers: billing.charge") && t.contains("Blast radius:"),
        "{t}"
    );
    let t = out(&ragmonk(&home, &["explore", "charge"]));
    assert!(t.contains("Intent:") && t.contains("Evidence:"), "{t}");

    // search: json with context, table, snippets, files, explain
    let r = json(&home, &["search", "charge function", "--json", "--explain"]);
    let doc = r["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "document")
        .expect("document hit");
    assert!(doc["path"].as_str().unwrap().starts_with(&src_abs));
    assert_eq!(doc["context"]["parent_heading"]["text"], "Charging");
    assert!(r["explain"]["stages"]
        .as_array()
        .is_some_and(|s| !s.is_empty()));
    let t = out(&ragmonk(&home, &["search", "charge function"]));
    assert!(
        t.contains("MD: ") && t.contains("Match:") && t.contains("[heading] Charging"),
        "{t}"
    );
    let t = out(&ragmonk(&home, &["search", "charge", "--table"]));
    assert!(t.starts_with("Kind") && t.contains("exact_symbol"), "{t}");
    let t = out(&ragmonk(&home, &["search", "zzzz_nothing"]));
    assert!(t.contains("No results for 'zzzz_nothing'."), "{t}");
    let t = out(&ragmonk(&home, &["search", "charge", "--explain"]));
    assert!(
        t.contains("Query kind: keyword") && t.contains("Tokenizer:"),
        "{t}"
    );

    // link add / list / remove
    let t = out(&ragmonk(&home, &["link", "add", "audit", "guide.md"]));
    assert!(t.starts_with("Linked billing.audit -> "), "{t}");
    let id = t
        .trim()
        .rsplit('(')
        .next()
        .unwrap()
        .trim_end_matches(')')
        .to_owned();
    assert_eq!(
        out(&ragmonk(&home, &["link", "add", "audit", "guide.md"])).trim(),
        "That link already exists."
    );
    let links = json(&home, &["link", "list", "--json", "--entity", "audit"]);
    let user: Vec<_> = links["links"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["resolver"] == "user")
        .collect();
    assert_eq!(user.len(), 1);
    assert_eq!(user[0]["link_id"], id.as_str());
    assert!(user[0]["document_path"]
        .as_str()
        .unwrap()
        .ends_with("guide.md"));
    let o = ragmonk(&home, &["link", "add", "nope", "guide.md"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(
        out(&ragmonk(&home, &["link", "remove", &id])).trim(),
        format!("Removed {id}")
    );
    let o = ragmonk(&home, &["link", "remove", &id]);
    assert_eq!(o.status.code(), Some(2));
    // Manual links survive a rebuild of the source.
    out(&ragmonk(&home, &["link", "add", "audit", "guide.md"]));
    std::fs::write(src.join("guide.md"), "# Billing\n\nChanged.\n").unwrap();
    out(&ragmonk(&home, &["index"]));
    let links = json(&home, &["link", "list", "--json"]);
    assert!(links["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["resolver"] == "user"));
}
