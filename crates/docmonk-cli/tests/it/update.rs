//! `docmonk update` against a stand-in release host
//! (`DOCMONK_UPDATE_TEST_BASE`, honoured by debug builds only): check,
//! status, the background check and notice, install with checksum
//! verification, self-check and rollback. POSIX only: the fake release
//! binary is a shell script.
#![cfg(unix)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A minimal HTTP/1.1 server answering GETs from `routes` (404 else).
fn serve() -> (String, Routes) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes: Routes = Arc::default();
    let r = routes.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let r = r.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap_or(0);
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let body = r.lock().unwrap().get(&path).cloned();
                let mut s = &stream;
                let (status, body) = match body {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", b"missing".to_vec()),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(&body);
            });
        }
    });
    (base, routes)
}

fn docmonk(home: &Path, base: &str, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_docmonk"));
    for (k, _) in std::env::vars() {
        if k.starts_with("DOCMONK_") {
            c.env_remove(k);
        }
    }
    c.args(args)
        .env("DOCMONK_HOME", home)
        .env("DOCMONK_UPDATE_TEST_BASE", base)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn data(o: &Output) -> Value {
    assert!(o.status.success(), "{o:?}");
    serde_json::from_slice::<Value>(&o.stdout).unwrap()["data"].clone()
}

fn target() -> &'static str {
    docmonk_update::TARGET
}

/// A release archive whose binary is a script printing the real
/// `version --json` envelope (`{"schema_version", "data": {"version"}}`) and
/// succeeding at `doctor`, with one bundled model file.
fn archive(version: &str, reported: &str) -> Vec<u8> {
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in version) echo '{{\"schema_version\":\"1\",\"data\":{{\"version\":\"{reported}\",\"runtime\":\"rust\"}}}}';; esac\nexit 0\n"
    );
    let top = format!("docmonk-{version}-{}", target());
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, data, mode) in [
        (format!("{top}/docmonk"), script.into_bytes(), 0o755),
        (
            format!("{top}/models/m/model.onnx"),
            b"weights".to_vec(),
            0o644,
        ),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, name, &data[..]).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn publish(routes: &Routes, version: &str, reported: &str, tamper: bool) {
    let name = format!("docmonk-{version}-{}.tar.gz", target());
    let a = archive(version, reported);
    let digest = if tamper { "0".repeat(64) } else { sha256(&a) };
    let tag = format!("v{version}");
    let mut r = routes.lock().unwrap();
    r.insert(
        "/api/releases/latest".into(),
        format!(r#"{{"tag_name":"{tag}","html_url":"https://example.invalid/{tag}"}}"#).into(),
    );
    r.insert(format!("/download/{tag}/{name}"), a);
    r.insert(
        format!("/download/{tag}/SHA256SUMS"),
        format!("{digest}  {name}\n").into(),
    );
}

/// A home whose native install already has `0.0.1` as current.
fn native_home(dir: &Path) -> (PathBuf, PathBuf) {
    let home = dir.join("home");
    let install = dir.join("install");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(install.join("versions/0.0.1")).unwrap();
    std::fs::write(install.join("versions/0.0.1/docmonk"), "old").unwrap();
    std::fs::write(
        install.join("install_state.json"),
        r#"{"current":"0.0.1","previous":null}"#,
    )
    .unwrap();
    std::fs::write(
        home.join("install_info.json"),
        serde_json::json!({
            "install_method": "native",
            "repository": "gzarog/DocMonk",
            "install_dir": install,
            "bin_dir": dir.join("bin"),
        })
        .to_string(),
    )
    .unwrap();
    (home, install)
}

#[test]
fn check_status_and_notice() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let (base, routes) = serve();

    let s = data(&docmonk(&home, &base, &["update", "status", "--json"]));
    assert_eq!(s["status"], "unknown");

    publish(&routes, "99.0.0", "99.0.0", false);
    let c = data(&docmonk(&home, &base, &["update", "check", "--json"]));
    assert_eq!(c["latest_version"], "99.0.0");
    assert_eq!(c["update_available"], true);
    let s = data(&docmonk(&home, &base, &["update", "status", "--json"]));
    assert_eq!(s["status"], "update available");

    // Any other command announces it on stderr, exactly once.
    let o = docmonk(&home, &base, &["config", "show"]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("A newer DocMonk version is available"),
        "{err}"
    );
    assert!(!String::from_utf8_lossy(&o.stdout).contains("newer"));
    let o = docmonk(&home, &base, &["config", "show"]);
    assert!(!String::from_utf8_lossy(&o.stderr).contains("newer"));

    // Non-semver tags are rejected.
    routes.lock().unwrap().insert(
        "/api/releases/latest".into(),
        br#"{"tag_name":"main"}"#.to_vec(),
    );
    let o = docmonk(&home, &base, &["update", "check"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("not a valid MAJOR.MINOR.PATCH"));
}

#[test]
fn background_check_refreshes_a_stale_cache() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let (base, routes) = serve();
    publish(&routes, "98.0.0", "98.0.0", false);
    docmonk(&home, &base, &["init"]);
    docmonk(&home, &base, &["config", "show"]);
    let cache = home.join("update.json");
    for _ in 0..100 {
        if cache.is_file() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let c: Value = serde_json::from_str(&std::fs::read_to_string(cache).unwrap()).unwrap();
    assert_eq!(c["latest_version"], "98.0.0");
}

#[test]
fn install_and_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let (home, install) = native_home(dir.path());
    let (base, routes) = serve();

    // A bad checksum is refused and nothing changes.
    publish(&routes, "99.1.0", "99.1.0", true);
    let o = docmonk(&home, &base, &["update", "install"]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("checksum mismatch"),
        "{o:?}"
    );
    assert!(!install.join("versions/99.1.0").exists());

    // A binary that reports the wrong version is refused.
    publish(&routes, "99.1.0", "1.0.0", false);
    let o = docmonk(&home, &base, &["update", "install"]);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("refusing to install"),
        "{o:?}"
    );
    let state = std::fs::read_to_string(install.join("install_state.json")).unwrap();
    assert!(state.contains(r#""current": "0.0.1""#) || state.contains(r#""current":"0.0.1""#));
    assert!(!install.join("versions/99.1.0").exists());

    publish(&routes, "99.1.0", "99.1.0", false);
    let out = data(&docmonk(&home, &base, &["update", "install", "--json"]));
    assert_eq!(out["installed_version"], "99.1.0");
    assert_eq!(out["upgraded"], true);
    assert_eq!(out["healthy"], true);
    let current = std::fs::read_link(install.join("current")).unwrap();
    assert_eq!(current, Path::new("versions/99.1.0"));
    assert_eq!(
        std::fs::read(home.join("models/m/model.onnx")).unwrap(),
        b"weights"
    );

    let r = data(&docmonk(&home, &base, &["update", "rollback", "--json"]));
    assert_eq!(r["installed_version"], "0.0.1");
    let current = std::fs::read_link(install.join("current")).unwrap();
    assert_eq!(current, Path::new("versions/0.0.1"));
}

#[test]
fn install_refuses_a_non_native_install() {
    let dir = tempfile::tempdir().unwrap();
    let (base, routes) = serve();
    publish(&routes, "99.1.0", "99.1.0", false);
    let o = docmonk(&dir.path().join("home"), &base, &["update", "install"]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("native installer"),
        "{o:?}"
    );
}

#[test]
fn prerelease_channel_follows_prereleases() {
    let dir = tempfile::tempdir().unwrap();
    let (home, install) = native_home(dir.path());
    let (base, routes) = serve();
    // 99.2.0 exists only as a pre-release; "latest" is still 99.1.0, and
    // 99.5.0 (a release with no archive for this target) must be skipped.
    publish(&routes, "99.2.0", "99.2.0", false);
    routes.lock().unwrap().insert(
        "/api/releases/latest".into(),
        br#"{"tag_name":"v99.1.0","html_url":"https://example.invalid/v99.1.0"}"#.to_vec(),
    );
    routes.lock().unwrap().insert(
        "/api/releases".into(),
        format!(
            r#"[{{"tag_name":"v99.5.0","html_url":"https://example.invalid/py",
                  "assets":[{{"name":"docmonk-99.5.0.tar.gz"}}]}},
                {{"tag_name":"v99.2.0","prerelease":true,"html_url":"https://example.invalid/v99.2.0",
                  "assets":[{{"name":"docmonk-99.2.0-{t}.tar.gz"}}]}},
                {{"tag_name":"v99.1.0","prerelease":false,"html_url":"https://example.invalid/v99.1.0",
                  "assets":[{{"name":"docmonk-99.1.0-{t}.tar.gz"}}]}},
                {{"tag_name":"v99.9.0","draft":true}}]"#,
            t = target()
        )
        .into_bytes(),
    );

    let stable = data(&docmonk(&home, &base, &["update", "check", "--json"]));
    assert_eq!(stable["latest_version"], "99.1.0");

    // Any other value stays stable.
    let o = docmonk(&home, &base, &["config", "set", "updates.channel", "beta"]);
    assert!(o.status.success(), "{o:?}");
    let beta = data(&docmonk(&home, &base, &["update", "check", "--json"]));
    assert_eq!(beta["latest_version"], "99.1.0");
    let o = docmonk(
        &home,
        &base,
        &["config", "set", "updates.channel", "prerelease"],
    );
    assert!(o.status.success(), "{o:?}");

    let pre = data(&docmonk(&home, &base, &["update", "check", "--json"]));
    assert_eq!(pre["latest_version"], "99.2.0");
    let out = data(&docmonk(&home, &base, &["update", "install", "--json"]));
    assert_eq!(out["installed_version"], "99.2.0");
    assert_eq!(
        std::fs::read_link(install.join("current")).unwrap(),
        Path::new("versions/99.2.0")
    );
}
