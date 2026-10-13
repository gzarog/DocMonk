use std::process::Command;

fn ragmonk() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ragmonk"))
}

#[test]
fn version_json_envelope() {
    let out = ragmonk().args(["version", "--json"]).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], "1");
    assert!(v["data"]["version"].is_string());
}

#[test]
fn unknown_command_exits_with_invalid_arguments_code() {
    let out = ragmonk().arg("definitely-not-a-command").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

fn with_home(home: &std::path::Path) -> Command {
    let mut cmd = ragmonk();
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RAGMONK_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("RAGMONK_HOME", home);
    cmd
}

#[test]
fn config_show_get_set_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("runtime:\n  log_level: info\n"), "{text}");
    assert!(
        home.join("logs").is_dir(),
        "config show ensures the runtime layout"
    );

    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["config", "set", "runtime.sqlite_cache_size_mb", "32"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["config", "get", "runtime.sqlite_cache_size_mb"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "32\n");
    let written = std::fs::read_to_string(home.join("config.yaml")).unwrap();
    assert!(
        written.contains("  sqlite_cache_size_mb: 32\n"),
        "{written}"
    );
}

#[test]
fn config_errors_exit_codes_and_redaction() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["config", "get", "no.such.key"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "Error: unknown config key: no.such.key\n"
    );

    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "storage:\n  server:\n    url: https://user:hunter2@es:9200\n",
    )
    .unwrap();
    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["config", "show"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.starts_with(
            "Error: invalid configuration: storage.server.url: must not contain credentials"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("hunter2"), "credential leaked: {stderr}");
}

// clean-slate-audit: allow-start
#[test]
fn no_migration_command_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let out = with_home(&tmp.path().join("home"))
        .args(["--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(out.status.success());
    assert!(!help.contains("migrate"), "{help}");
}
// clean-slate-audit: allow-end

#[test]
fn server_requires_server_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["server", "init"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "local mode is a config error");
    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["server", "schema", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["indexes"].as_array().unwrap().len(), 7);
    assert_eq!(v["data"]["indexes"][6]["name"], "ragmonk-runtime");
    assert_eq!(v["data"]["indexes"][0]["name"], "ragmonk-source-state");
}

#[test]
fn incompatible_control_database_is_refused_not_converted() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let db = home.join("state").join("control.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE sources (id TEXT); INSERT INTO sources VALUES ('x');")
            .unwrap();
    }
    let before = std::fs::read(&db).unwrap();
    let out = with_home(&home)
        .current_dir(tmp.path())
        .args(["source", "list"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ragmonk index"), "{err}");
    assert_eq!(std::fs::read(&db).unwrap(), before, "never altered");
}
