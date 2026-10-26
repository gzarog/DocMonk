//! Daemon bookkeeping against fixtures/expected/daemon.json: uptime text,
//! PID and health file parsing, and trigger coalescing replayed step by
//! step.

use std::path::PathBuf;

use docmonk_core::paths::Home;
use docmonk_indexing::daemon::scheduler::Scheduler;
use docmonk_indexing::daemon::{format_uptime, health, pid};
use serde_json::{json, Value};

fn golden() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/expected/daemon.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn uptime_text() {
    for c in golden()["uptime"].as_array().unwrap() {
        assert_eq!(
            format_uptime(c["seconds"].as_f64().unwrap()),
            c["text"],
            "{c}"
        );
    }
}

#[test]
fn pid_and_health_files() {
    let g = golden();
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path());
    for c in g["pid_files"].as_array().unwrap() {
        std::fs::write(home.daemon_pid(), c["text"].as_str().unwrap()).unwrap();
        let got =
            pid::read_pid_file(&home).map(|i| json!({"pid": i.pid, "started_at": i.started_at}));
        assert_eq!(got.unwrap_or(Value::Null), c["parsed"], "{}", c["text"]);
    }
    for c in g["health_files"].as_array().unwrap() {
        std::fs::write(home.daemon_health(), c["text"].as_str().unwrap()).unwrap();
        let got = health::read_health(&home).map(|h| serde_json::to_value(h).unwrap());
        assert_eq!(got.unwrap_or(Value::Null), c["parsed"], "{}", c["text"]);
    }
}

#[test]
fn coalescing_scripts() {
    for script in golden()["scheduler"].as_array().unwrap() {
        let name = &script["name"];
        let mut s = Scheduler::default();
        for (i, step) in script["steps"].as_array().unwrap().iter().enumerate() {
            let op = step["op"].as_array().unwrap();
            let mut got = serde_json::Map::new();
            match op[0].as_str().unwrap() {
                "trigger" => {
                    let paths: Vec<PathBuf> = step["paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| PathBuf::from(p.as_str().unwrap()))
                        .collect();
                    s.enqueue(op[1].as_str().unwrap(), op[2].as_str().unwrap(), &paths);
                }
                kind @ ("start" | "start_no_request") => {
                    let id = s.start_next().unwrap();
                    got.insert("started".into(), json!(id));
                    if kind == "start" {
                        let r = s.take_request(&id);
                        got.insert(
                            "request".into(),
                            json!({
                                "source_id": r.source_id,
                                "reason": r.reason,
                                "full": r.full,
                                "changed_paths": r.changed_paths.iter()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .collect::<Vec<_>>(),
                            }),
                        );
                    }
                }
                _ => {
                    s.settle(op[1].as_str().unwrap());
                }
            }
            for key in ["started", "request"] {
                assert_eq!(
                    got.get(key).unwrap_or(&Value::Null),
                    &step[key],
                    "{name} step {i} {key}"
                );
            }
            assert_eq!(json!(s.queue()), step["queue"], "{name} step {i} queue");
            for (id, state) in step["states"].as_object().unwrap() {
                assert_eq!(s.state_of(id), state.as_str(), "{name} step {i} {id}");
            }
            let n = step["states"].as_object().unwrap().len();
            let known = ["a", "b"]
                .iter()
                .filter(|id| s.state_of(id).is_some())
                .count();
            assert_eq!(known, n, "{name} step {i} states");
        }
    }
}
