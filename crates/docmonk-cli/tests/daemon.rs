//! `ragmonk daemon` process lifecycle: start, status, a second start, stop.

use std::path::Path;
use std::process::{Command, Output};

/// Output goes to files, not pipes: on Windows the detached daemon
/// inherits the CLI's inheritable handles, so a pipe would never reach EOF
/// while the daemon runs.
fn ragmonk(home: &Path, args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let (out, err) = (dir.path().join("out"), dir.path().join("err"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_ragmonk"))
        .args(args)
        .env("RAGMONK_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    // Fail fast instead of hanging the CI job.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("`ragmonk {}` did not finish within 60s", args.join(" "));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Output {
        status,
        stdout: std::fs::read(&out).unwrap(),
        stderr: std::fs::read(&err).unwrap(),
    }
}

fn status(home: &Path) -> serde_json::Value {
    let out = ragmonk(home, &["daemon", "status", "--json"]);
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["data"].clone()
}

/// Kills a daemon the test left running (on failure).
struct Reaper(std::path::PathBuf);
impl Drop for Reaper {
    fn drop(&mut self) {
        if let Ok(text) = std::fs::read_to_string(self.0.join("daemon.pid")) {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if let Some(pid) = v["pid"].as_u64() {
                let pid = sysinfo::Pid::from_u32(pid as u32);
                let mut sys = sysinfo::System::new();
                sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
                if let Some(p) = sys.process(pid) {
                    p.kill();
                }
            }
        }
    }
}

#[test]
fn start_status_stop() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let _reaper = Reaper(home.clone());

    let s = status(&home);
    assert_eq!(s["running"], false);
    assert_eq!(s["pid"], serde_json::Value::Null);
    let keys: Vec<_> = s.as_object().unwrap().keys().cloned().collect();
    // Stable key order.
    assert_eq!(
        keys,
        [
            "running",
            "pid",
            "started_at",
            "uptime_seconds",
            "last_reconciliation_at",
            "sources"
        ]
    );

    let out = ragmonk(&home, &["daemon", "start"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(text.starts_with("Daemon started (pid "), "{text}");

    let s = status(&home);
    assert_eq!(s["running"], true);
    let pid = s["pid"].as_i64().unwrap();
    assert!(s["uptime_seconds"].as_f64().unwrap() >= 0.0);
    assert!(home.join("daemon_health.json").is_file());

    let out = ragmonk(&home, &["daemon", "start"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("Daemon already running (pid {pid})")
    );
    // A foreground run refuses while another daemon owns the home.
    let out = ragmonk(&home, &["daemon", "run"]);
    assert!(!out.status.success());

    let out = ragmonk(&home, &["daemon", "stop"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("Daemon stopped (pid {pid})")
    );
    assert!(!home.join("daemon.pid").exists());
    assert_eq!(status(&home)["running"], false);
    let out = ragmonk(&home, &["daemon", "stop"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Daemon is not running"
    );
}
