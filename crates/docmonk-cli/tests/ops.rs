//! Ops commands end to end: doctor/health, backup → restore round trip,
//! foreign-archive refusal, rebuild, vectors and uninstall.

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

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("app.py"), "def main():\n    return 1\n").unwrap();
    std::fs::write(src.join("notes.md"), "# Notes\n\nSome text.\n").unwrap();
    out(&ragmonk(&home, &["init"]));
    out(&ragmonk(&home, &["source", "add", src.to_str().unwrap()]));
    out(&ragmonk(&home, &["index"]));
    (dir, home, src)
}

#[test]
fn doctor_and_health() {
    let (_dir, home, _src) = setup();
    let d = json(&home, &["doctor", "--json"]);
    let names: Vec<&str> = d["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "Core",
            "Database",
            "Sources",
            "Index Lock",
            "Index",
            "Disk",
            "Tokenizer",
            "AI"
        ]
    );
    assert_ne!(d["result"], "UNHEALTHY", "{d}");
    let tok = &d["sections"][6]["checks"];
    assert!(
        tok[4]["detail"].as_str().unwrap().contains("truncated 0"),
        "{tok}"
    );
    let t = out(&ragmonk(&home, &["doctor"]));
    assert!(t.contains("RagMonk Doctor") && t.contains("Result:"), "{t}");
    let h = json(&home, &["health", "--json"]);
    assert_eq!(h["result"], d["result"]);
}

#[test]
fn backup_restore_round_trip_and_foreign_refusal() {
    let (dir, home, src) = setup();
    let archive = dir.path().join("b.tar.gz");
    let b = json(&home, &["backup", archive.to_str().unwrap(), "--json"]);
    assert_eq!(b["projects"].as_object().unwrap().len(), 1);
    assert!(archive.is_file());

    // Change state, then restore the snapshot.
    let id = json(&home, &["status", "--json"])["sources"][0]["source_id"]
        .as_str()
        .unwrap()
        .to_owned();
    out(&ragmonk(&home, &["source", "remove", &id, "--yes"]));
    assert!(json(&home, &["status", "--json"])["sources"]
        .as_array()
        .unwrap()
        .is_empty());
    let r = json(&home, &["restore", archive.to_str().unwrap(), "--json"]);
    assert_eq!(r["projects_restored"].as_array().unwrap().len(), 1);
    assert_eq!(r["daemon_was_running"], false);
    let st = json(&home, &["status", "--json"]);
    assert_eq!(st["sources"][0]["source_id"], id.as_str());
    assert_eq!(st["sources"][0]["published"]["indexed"], 2);
    // The restored index answers queries.
    let s = json(&home, &["symbol", "main", "--json"]);
    assert_eq!(s["matches"].as_array().unwrap().len(), 1);
    assert!(src.join("app.py").exists());

    // An archive in any other format is refused and nothing changes.
    let foreign = dir.path().join("foreign.tar.gz");
    let stage = dir.path().join("foreignstage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join("manifest.json"),
        r#"{"format_version": 0, "ragmonk_version": "0.9"}"#,
    )
    .unwrap();
    std::fs::write(stage.join("sources.db"), b"").unwrap();
    let f = std::fs::File::create(&foreign).unwrap();
    let mut t = tar::Builder::new(flate2::write::GzEncoder::new(
        f,
        flate2::Compression::default(),
    ));
    t.append_dir_all(".", &stage).unwrap();
    t.into_inner().unwrap().finish().unwrap();
    let o = ragmonk(&home, &["restore", foreign.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("unsupported backup archive format"),
        "{o:?}"
    );
    assert_eq!(
        json(&home, &["status", "--json"])["sources"][0]["source_id"],
        id.as_str()
    );

    // Garbage is refused before anything is touched.
    std::fs::write(dir.path().join("junk.tar.gz"), b"junk").unwrap();
    let o = ragmonk(
        &home,
        &["restore", dir.path().join("junk.tar.gz").to_str().unwrap()],
    );
    assert!(!o.status.success());
    let o = ragmonk(&home, &["restore", "/no/such/archive.tar.gz"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn rebuild_vectors_uninstall() {
    let (_dir, home, _src) = setup();
    let r = json(&home, &["rebuild", "--json"]);
    assert_eq!(r["sources"][0]["scanned"], 2);
    assert_eq!(r["sources"][0]["indexed"], 2);
    let t = out(&ragmonk(&home, &["rebuild", "--fresh", "--yes"]));
    assert!(t.contains("rebuilt scanned=2 indexed=2"), "{t}");

    let v = json(&home, &["vectors", "rebuild", "--json"]);
    assert!(
        v["rebuilt"][0]["backend"].is_null(),
        "no embeddings with semantic off"
    );

    let u = json(&home, &["uninstall", "--yes", "--keep-data", "--json"]);
    assert_eq!(u["data_purged"], false);
    assert!(home.exists());
    let u = json(&home, &["uninstall", "--yes", "--json"]);
    assert_eq!(u["data_purged"], true);
    assert!(!home.exists());
}

#[test]
fn rebuild_continues_past_a_failing_source() {
    let (dir, home, _src) = setup();
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("lib.py"), "def helper():\n    return 2\n").unwrap();
    out(&ragmonk(&home, &["source", "add", other.to_str().unwrap()]));
    out(&ragmonk(&home, &["index"]));
    // Break the first source's knowledge database: its rebuild must fail
    // on its own while the other source still rebuilds.
    let mut dbs: Vec<_> = std::fs::read_dir(home.join("projects"))
        .unwrap()
        .map(|e| e.unwrap().path().join("knowledge.db"))
        .filter(|p| p.is_file())
        .collect();
    dbs.sort();
    assert_eq!(dbs.len(), 2);
    for sidecar in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{sidecar}", dbs[0].display()));
    }
    std::fs::write(&dbs[0], b"not a database").unwrap();
    let o = ragmonk(&home, &["rebuild", "--json"]);
    assert!(!o.status.success(), "a failed source makes the run partial");
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let sources = v["data"]["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 2, "both sources were attempted: {v}");
    assert_eq!(
        sources.iter().filter(|s| s.get("error").is_some()).count(),
        1,
        "{v}"
    );
    assert!(sources
        .iter()
        .any(|s| s.get("error").is_none() && s["indexed"].as_u64() > Some(0)));
}
