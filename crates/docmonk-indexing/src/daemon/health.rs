//! The daemon's heartbeat snapshot, `daemon_health.json`
//! `daemon status` reads it from another
//! process. The daemon rewrites it after every pass and reconciliation.

use std::path::PathBuf;

use docmonk_core::paths::Home;
use serde::{Deserialize, Serialize};

/// Unknown keys make the entry unreadable, as `SourceWatchStatus(**s)` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceWatchStatus {
    pub source_id: String,
    pub path: String,
    /// `"local"` or `"network"`.
    pub source_type: String,
    pub online: bool,
    #[serde(default)]
    pub last_pass_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonHealth {
    pub started_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub last_reconciliation_at: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceWatchStatus>,
}

/// Write-then-rename, with a per-process and per-thread tmp name so the
/// worker and the reconciliation thread never share one.
pub fn write_health(home: &Home, health: &DaemonHealth) -> std::io::Result<()> {
    let path = home.daemon_health();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tid = format!("{:?}", std::thread::current().id());
    let tid: String = tid.chars().filter(char::is_ascii_digit).collect();
    let tmp = PathBuf::from(format!(
        "{}.{}.{tid}.tmp",
        path.display(),
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_string(health).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)
}

/// `None` when missing or unreadable.
pub fn read_health(home: &Home) -> Option<DaemonHealth> {
    serde_json::from_str(&std::fs::read_to_string(home.daemon_health()).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tolerant_read() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        assert!(read_health(&home).is_none());
        let h = DaemonHealth {
            started_at: "a".into(),
            updated_at: "b".into(),
            last_reconciliation_at: None,
            sources: vec![SourceWatchStatus {
                source_id: "s".into(),
                path: "/p".into(),
                source_type: "local".into(),
                online: true,
                last_pass_at: Some("c".into()),
            }],
        };
        write_health(&home, &h).unwrap();
        assert_eq!(read_health(&home).unwrap(), h);
        std::fs::write(home.daemon_health(), "{\"started_at\": 1}").unwrap();
        assert!(read_health(&home).is_none());
    }
}
