//! `init`, `source …`, `index`, `status` and `docs` end to end on a fresh
//! home: list, info, enable/disable, remove, errors and text views.

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
fn workflow_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("app.py"), "def main():\n    return 1\n").unwrap();
    std::fs::write(src.join("notes.md"), "# Notes\n\nSome text.\n").unwrap();

    let t = out(&ragmonk(&home, &["init"]));
    assert!(t.starts_with("Wrote default configuration to "), "{t}");
    let t = out(&ragmonk(&home, &["init"]));
    assert!(t.starts_with("Using existing configuration at "), "{t}");

    // Errors: a missing path, an unknown source.
    let o = ragmonk(&home, &["source", "add", "/definitely/missing"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("source path does not exist"));
    let o = ragmonk(&home, &["index", "--source", "src_nope"]);
    assert_eq!(o.status.code(), Some(2), "usage error");

    let t = out(&ragmonk(&home, &["source", "add", src.to_str().unwrap()]));
    let id = t.split_whitespace().nth(2).unwrap().to_owned();
    assert!(id.starts_with("src_"), "{t}");
    // Adding the same path again returns the same source.
    assert!(out(&ragmonk(&home, &["source", "add", src.to_str().unwrap()])).contains(&id));

    let list = out(&ragmonk(&home, &["source", "list"]));
    assert!(list.starts_with("ID"), "{list}");
    assert!(list.contains(&id) && list.contains("yes"), "{list}");
    let info: serde_json::Value =
        serde_json::from_str(&out(&ragmonk(&home, &["source", "info", &id]))).unwrap();
    assert_eq!(info["id"], id.as_str());
    assert_eq!(info["build_state"], "needs_full_rebuild");

    assert_eq!(
        out(&ragmonk(&home, &["source", "disable", &id])).trim(),
        format!("Disabled {id}")
    );
    assert_eq!(
        out(&ragmonk(&home, &["index"])).trim(),
        "No enabled sources to index."
    );
    assert_eq!(
        out(&ragmonk(&home, &["source", "enable", &id])).trim(),
        format!("Enabled {id}")
    );

    let t = out(&ragmonk(&home, &["index"]));
    assert!(t.contains("scanned=2 new=2"), "{t}");
    assert!(
        t.contains("Index complete: 1 source(s) processed, 1 succeeded, 0 failed.\n"),
        "{t}"
    );
    // Phase 2 runs after every source indexed, reported on its own line.
    let idx = t.find("Index complete").unwrap();
    assert!(t[..idx].contains(": relationships ready"), "{t}");
    assert!(
        t.contains(
            "Relationships: 1 source(s) attempted, 1 built, 0 up to date, 0 stale, 0 failed.\n"
        ),
        "{t}"
    );
    assert!(
        t.ends_with("  Graphs: 0 skipped, 0 rebound, 0 incremental, 1 full, 0 retried; 2 file(s) derived, 2 re-resolved, 2 relationship write(s).\n"),
        "{t}"
    );

    let docs = json(&home, &["docs", "--json"]);
    let docs = docs["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["format"], "markdown");
    assert_eq!(docs[0]["title"], "Notes");
    let t = out(&ragmonk(&home, &["docs"]));
    assert!(t.starts_with("Path") && t.contains("notes.md"), "{t}");

    let st = json(&home, &["status", "--json"]);
    assert_eq!(st["schema_version"], 1);
    assert_eq!(st["mode"], "local");
    assert_eq!(st["health"]["state"], "healthy");
    assert_eq!(st["summary"]["files"]["published_indexed"], 2);
    assert_eq!(st["sources"][0]["index_state"], "completed");
    // Indexing and the relationship graph are reported independently.
    assert_eq!(st["sources"][0]["relationship_state"]["state"], "ready");

    // A graph-only retry reprocesses no content.
    let t = out(&ragmonk(&home, &["relationships", "build"]));
    assert!(t.contains(": relationships up_to_date"), "{t}");
    assert!(t.contains("1 up to date"), "{t}");
    let r = json(
        &home,
        &["relationships", "build", "--source", &id, "--json"],
    );
    assert_eq!(r["outcome"], "success", "{r}");
    assert_eq!(r["sources"][0]["relationship_state"], "up_to_date", "{r}");

    // Disabled: no graph jobs, the graph is hidden, the index stays.
    let o = ragmonk(
        &home,
        &["config", "set", "indexing.relationships_enabled", "false"],
    );
    assert!(o.status.success(), "{}", out(&o));
    let t = out(&ragmonk(&home, &["index"]));
    assert!(t.contains("scanned=2 new=0 changed=0 unchanged=2"), "{t}");
    assert!(!t.contains(": relationships"), "{t}");
    assert!(t.contains("Relationships: disabled"), "{t}");
    let st = json(&home, &["status", "--json"]);
    assert_eq!(st["sources"][0]["relationship_state"]["state"], "disabled");
    assert_eq!(st["summary"]["files"]["published_indexed"], 2);
    assert!(!ragmonk(&home, &["relationships", "build"]).status.success());
    let o = ragmonk(
        &home,
        &["config", "set", "indexing.relationships_enabled", "true"],
    );
    assert!(o.status.success(), "{}", out(&o));
    let t = out(&ragmonk(&home, &["index"]));
    assert!(t.contains(": relationships ready"), "{t}");

    let t = out(&ragmonk(&home, &["status"]));
    assert!(t.is_ascii(), "{t}");
    for part in [
        "RagMonk status  [local mode, sqlite, authoritative]",
        "Health: HEALTHY",
        "Published: 2 indexed, 0 failed, 0 retrying of 2 files",
        "== Sources ==",
        &id,
    ] {
        assert!(t.contains(part), "{part}: {t}");
    }
    assert!(out(&ragmonk(&home, &["status", "--errors"])).contains("No problems found."));
    let o = ragmonk(&home, &["status", "--watch", "--json"]);
    assert_eq!(o.status.code(), Some(2));

    let t = out(&ragmonk(&home, &["source", "remove", &id, "--yes"]));
    assert!(t.starts_with(&format!("Removed {id}")), "{t}");
    assert!(t.contains("deleted indexed data at"), "{t}");
    assert!(json(&home, &["status", "--json"])["sources"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(
        src.join("app.py").exists(),
        "source files are never touched"
    );
}
