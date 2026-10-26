//! `docmonk eval` (roadmap P10): record queries with where their answers
//! are, list and remove them, and run them against the index, reporting
//! recall@1/10, MRR and abstention. With the pinned models present,
//! `--model` scores a throwaway index built with that model.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn docmonk(home: &Path, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_docmonk"));
    for (k, _) in std::env::vars() {
        // Only the pinned-model locations are inherited.
        if k.starts_with("DOCMONK_") && k != "DOCMONK_MODELS_DIR" && k != "DOCMONK_OCR_MODELS_DIR" {
            c.env_remove(k);
        }
    }
    c.env("DOCMONK_HOME", home)
        .env("DOCMONK_UPDATES__ENABLED", "false")
        .env("DOCMONK_NO_UPDATE_CHECK", "1")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ok(home: &Path, args: &[&str]) -> String {
    let o = docmonk(home, args);
    assert!(o.status.success(), "{args:?}: {o:?}");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn json(home: &Path, args: &[&str]) -> Value {
    let out = ok(home, args);
    let v: Value = serde_json::from_str(&out).expect(&out);
    v.get("data").cloned().unwrap_or(v)
}

#[test]
fn record_list_run_and_remove_round_trip() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let root = d.path().join("billing");
    write(
        &root,
        "docs/refunds.md",
        "# Refund approval\n\nRefunds above 500 euros need finance approval.\n",
    );
    write(
        &root,
        "docs/shipping.md",
        "# Shipping\n\nParcels leave the warehouse every weekday at noon.\n",
    );
    write(
        &root,
        "src/Billing/Invoice.cs",
        "namespace Billing {\n    public class Invoice {\n        public int Total(int a) { return a; }\n    }\n}\n",
    );
    ok(&home, &["init"]);
    ok(&home, &["source", "add", root.to_str().unwrap()]);
    ok(&home, &["index"]);

    let q1 = json(
        &home,
        &[
            "eval",
            "record",
            "which refunds need finance approval",
            "--expect",
            "docs/refunds.md",
            "--json",
        ],
    );
    assert_eq!(q1["id"], "q1");
    ok(
        &home,
        &[
            "eval",
            "record",
            "Invoice Total",
            "--expect",
            "src/Billing/Invoice.cs:3",
        ],
    );
    ok(
        &home,
        &[
            "eval",
            "record",
            "quarterly zanzibarquorum ledger",
            "--unanswerable",
        ],
    );
    // Recording needs an expectation or --unanswerable.
    assert!(!docmonk(&home, &["eval", "record", "nothing expected"])
        .status
        .success());

    let set = json(&home, &["eval", "list", "--json"]);
    assert_eq!(set["queries"].as_array().unwrap().len(), 3);
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("eval/queries.json")).unwrap())
            .unwrap();
    assert_eq!(stored["format_version"], 1);
    assert_eq!(stored["queries"][1]["expect"][0]["line"], 3);

    let r = json(&home, &["eval", "run", "--json"]);
    let run = &r["runs"][0];
    assert_eq!(run["answerable"], 2);
    assert_eq!(run["unanswerable"], 1);
    let m = &run["metrics"];
    for k in [
        "recall@1",
        "recall@10",
        "mrr",
        "false_abstention",
        "false_support",
    ] {
        assert!(m[k].is_number(), "{k}: {r}");
    }
    assert_eq!(m["recall@10"], 1.0, "{r}");
    assert!(m["mrr"].as_f64().unwrap() > 0.0, "{r}");
    let ranks: Vec<&Value> = run["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| &q["rank"])
        .collect();
    assert!(
        ranks[0].is_u64() && ranks[1].is_u64() && ranks[2].is_null(),
        "{r}"
    );
    // The table form prints one row per run.
    let table = ok(&home, &["eval", "run"]);
    assert!(table.contains("recall@10"), "{table}");

    // With the pinned embedding model: a throwaway index per model.
    let models = std::env::var_os("DOCMONK_MODELS_DIR")
        .map(std::path::PathBuf::from)
        .filter(|m| m.join("all-minilm-l6-v2").is_dir());
    if models.is_some() {
        let r = json(
            &home,
            &["eval", "run", "--model", "all-minilm-l6-v2", "--json"],
        );
        let run = &r["runs"][0];
        assert_eq!(run["embedding_model"], "all-minilm-l6-v2");
        assert_eq!(run["semantic"], true);
        assert!(run["metrics"]["recall@10"].is_number(), "{r}");
        assert!(home.join("eval/runs/all-minilm-l6-v2").is_dir());
    }
    let bad = docmonk(&home, &["eval", "run", "--model", "no-such-model"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("unknown embedding model"),
        "{bad:?}"
    );

    ok(&home, &["eval", "remove", "q2"]);
    let set = json(&home, &["eval", "list", "--json"]);
    assert_eq!(set["queries"].as_array().unwrap().len(), 2);
    assert!(!docmonk(&home, &["eval", "remove", "q9"]).status.success());
}
