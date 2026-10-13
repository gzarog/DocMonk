//! Core identities, secret detection, lock messages and the home layout
//! against fixtures/expected/core_*.json.

use docmonk_core::errors::{DocMonkError, LockOwner};
use docmonk_core::ids;
use docmonk_core::paths::{project_id_for_canonical, Home};
use docmonk_core::security::is_secret_filename_for;
use serde_json::Value;

fn golden(name: &str) -> Value {
    let path = format!(
        "{}/../../fixtures/expected/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[test]
fn source_and_project_ids_are_as_expected() {
    let g = golden("core_ids.json");
    for case in g["source_ids"].as_array().unwrap() {
        assert_eq!(
            ids::make_source_id(s(&case["canonical_path"])),
            s(&case["id"])
        );
    }
    for case in g["project_ids"].as_array().unwrap() {
        assert_eq!(
            project_id_for_canonical(s(&case["canonical_path"])),
            s(&case["id"])
        );
    }
    for case in g["source_types"].as_array().unwrap() {
        assert_eq!(
            ids::detect_source_type(s(&case["raw"])).as_str(),
            s(&case["type"]),
            "{case}"
        );
    }
}

#[test]
fn secret_patterns_are_as_expected() {
    let g = golden("core_misc.json");
    for c in g["secrets"].as_array().unwrap() {
        assert_eq!(
            is_secret_filename_for(s(&c["name"]), false),
            c["secret"].as_bool().unwrap(),
            "{c}"
        );
    }
}

#[test]
fn lock_timeout_messages_are_as_expected() {
    let g = golden("core_misc.json");
    for c in g["lock_timeouts"].as_array().unwrap() {
        let owner = match &c["owner"] {
            Value::Object(map) => {
                let field = |k: &str| match map.get(k) {
                    None | Some(Value::Null) => None,
                    Some(Value::String(v)) => Some(v.clone()),
                    Some(v) => Some(v.to_string()),
                };
                LockOwner {
                    present: !map.is_empty(),
                    pid: field("pid"),
                    operation: field("operation"),
                    source_id: field("source_id"),
                    acquired_at: field("acquired_at"),
                    hostname: field("hostname"),
                }
            }
            _ => LockOwner::default(),
        };
        let err = DocMonkError::run_lock_timeout(
            "/x/locks/index.lock",
            c["timeout"].as_f64().unwrap(),
            &owner,
        );
        assert_eq!(err.message(), s(&c["message"]));
        assert_eq!(u64::from(err.exit_code()), c["exit_code"].as_u64().unwrap());
    }
}

#[test]
fn home_layout_is_as_expected() {
    let g = golden("core_misc.json");
    let home = Home::new("/H");
    let rel = |p: std::path::PathBuf| {
        p.strip_prefix("/H")
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    let hp = &g["home_paths"];
    assert_eq!(rel(home.user_config()), s(&hp["user_config"]));
    assert_eq!(rel(home.logs_dir()), s(&hp["logs"]));
    assert_eq!(rel(home.backups_dir()), s(&hp["backups"]));
    assert_eq!(rel(home.locks_dir()), s(&hp["locks"]));
    assert_eq!(rel(home.tmp_dir()), s(&hp["tmp"]));
    assert_eq!(rel(home.daemon_pid()), s(&hp["daemon_pid"]));
    assert_eq!(rel(home.daemon_health()), s(&hp["daemon_health"]));
    assert_eq!(rel(home.index_progress()), s(&hp["index_progress"]));
    assert_eq!(rel(home.install_info()), s(&hp["install_info"]));
    assert_eq!(rel(home.update_cache()), s(&hp["update_cache"]));
    assert_eq!(rel(home.projects_dir()), s(&hp["projects"]));
    let pp = &g["project_paths"];
    assert_eq!(rel(home.project_db("abc")), s(&pp["project_db"]));
    assert_eq!(rel(home.project_cache_dir("abc")), s(&pp["project_cache"]));
    assert_eq!(rel(home.project_state_dir("abc")), s(&pp["project_state"]));
    assert_eq!(
        rel(home.project_vector_index("abc")),
        s(&pp["vector_index"])
    );
    assert_eq!(rel(home.project_vector_meta("abc")), s(&pp["vector_meta"]));
}
