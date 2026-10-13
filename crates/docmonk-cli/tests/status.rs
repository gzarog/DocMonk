//! `ragmonk status` end to end: the watch loop stops cleanly on Ctrl+C,
//! an unreachable server is a nonzero exit (never an empty report), and
//! server mode never shows the home's local SQLite data.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn cmd(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ragmonk"));
    c.args(args)
        .env("RAGMONK_HOME", home)
        .env("RAGMONK_SEARCH__SEMANTIC", "false")
        .stdin(Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

fn run(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    cmd(home, args, env).output().unwrap()
}

fn data(o: &Output) -> serde_json::Value {
    assert!(o.status.success(), "{o:?}");
    serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()["data"].clone()
}

/// A home with one indexed local source.
fn local_home(dir: &Path) -> std::path::PathBuf {
    let home = dir.join("home");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("app.py"), "def main():\n    return 1\n").unwrap();
    assert!(run(&home, &["init"], &[]).status.success());
    assert!(run(&home, &["source", "add", src.to_str().unwrap()], &[])
        .status
        .success());
    assert!(run(&home, &["index"], &[]).status.success());
    home
}

#[cfg(unix)]
#[test]
fn watch_stops_cleanly_on_ctrl_c() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let home = local_home(dir.path());
    let child = cmd(&home, &["status", "--watch", "--interval", "0.2"], &[])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let started = Instant::now();
    let o = child.wait_with_output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("RagMonk status"), "{text}");
    assert!(text.contains("Ctrl+C to exit"), "{text}");
}

#[test]
fn json_is_the_canonical_report_and_watch_json_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let home = local_home(dir.path());
    let r = data(&run(&home, &["status", "--json"], &[]));
    for k in [
        "schema_version",
        "snapshot_id",
        "observed_at",
        "mode",
        "backend",
        "health",
        "indexer",
        "summary",
        "sources",
        "problems",
        "recent_errors",
        "diagnostics",
    ] {
        assert!(r.get(k).is_some(), "{k}");
    }
    assert_eq!(r["backend"]["kind"], "sqlite");
    assert_eq!(r["sources"][0]["published"]["indexed"], 1);
    let o = run(&home, &["status", "--watch", "--json"], &[]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn unreachable_server_fails_with_exit_code_four() {
    let dir = tempfile::tempdir().unwrap();
    let home = local_home(dir.path());
    let env = [
        ("RAGMONK_STORAGE__MODE", "server"),
        ("RAGMONK_STORAGE__SERVER__URL", "http://127.0.0.1:9"),
        ("RAGMONK_STORAGE__SERVER__REQUEST_TIMEOUT_SECONDS", "2"),
    ];
    for args in [
        &["status"][..],
        &["status", "--json"],
        &["status", "--errors"],
    ] {
        let o = run(&home, args, &env);
        assert_eq!(o.status.code(), Some(4), "{args:?}: {o:?}");
        assert!(!String::from_utf8_lossy(&o.stdout).contains("No problems found"));
    }
}

/// Server mode with a populated local home and an empty server prefix
/// shows the (empty) server catalog, never the local counts.
#[test]
fn server_mode_never_reads_local_sqlite() {
    let Some(url) = std::env::var("RAGMONK_TEST_OPENSEARCH_URL")
        .ok()
        .or_else(|| std::env::var("RAGMONK_TEST_ELASTICSEARCH_URL").ok())
        .filter(|u| !u.is_empty())
    else {
        assert!(
            std::env::var("RAGMONK_REQUIRE_LIVE_SERVER").as_deref() != Ok("1"),
            "a test cluster URL is required when RAGMONK_REQUIRE_LIVE_SERVER=1"
        );
        eprintln!("skipping: no test cluster");
        return;
    };
    let engine = if std::env::var("RAGMONK_TEST_OPENSEARCH_URL").is_ok_and(|u| u == url) {
        "opensearch"
    } else {
        "elasticsearch"
    };
    let dir = tempfile::tempdir().unwrap();
    let home = local_home(dir.path());
    let local = data(&run(&home, &["status", "--json"], &[]));
    assert_eq!(local["summary"]["sources"]["registered"], 1);
    let prefix = format!(
        "rmcli{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000_000
    );
    let env = [
        ("RAGMONK_STORAGE__MODE", "server"),
        ("RAGMONK_STORAGE__SERVER__URL", url.as_str()),
        ("RAGMONK_STORAGE__SERVER__ENGINE", engine),
        ("RAGMONK_STORAGE__SERVER__INDEX_PREFIX", prefix.as_str()),
    ];
    // No schema yet: a typed failure that names the fix.
    let o = run(&home, &["status", "--json"], &env);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("server init"),
        "{o:?}"
    );
    assert!(run(&home, &["server", "init"], &env).status.success());
    let r = data(&run(&home, &["status", "--json"], &env));
    assert_eq!(r["mode"], "server");
    assert_eq!(r["backend"]["kind"], engine);
    assert_eq!(r["summary"]["sources"]["registered"], 0);
    assert_eq!(r["summary"]["files"]["discovered"], 0);
    assert!(r["sources"].as_array().unwrap().is_empty());
    // Clean up the disposable prefix.
    let client = reqwest::blocking::Client::new();
    for kind in [
        "source-state",
        "files",
        "code",
        "documents",
        "chunks",
        "relationships",
        "runtime",
    ] {
        let _ = client.delete(format!("{url}/{prefix}-{kind}")).send();
    }
}
