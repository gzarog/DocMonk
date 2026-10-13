//! `daemon.pid` bookkeeping: lets a separate CLI
//! invocation find and signal the running daemon. The per-pass
//! `index.lock` is what keeps two writers apart. The PID file only locates
//! the process.

use std::path::Path;
use std::time::{Duration, Instant};

use docmonk_core::paths::Home;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::progress::now_iso;
use crate::runtime::is_process_alive;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidInfo {
    pub pid: i64,
    pub started_at: String,
}

/// Writes `{"pid", "started_at"}` (now) for `pid`.
pub fn write_pid_file(home: &Home, pid: i64) -> std::io::Result<PidInfo> {
    let info = PidInfo {
        pid,
        started_at: now_iso(),
    };
    let path = home.daemon_pid();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string(&info).unwrap_or_default())?;
    Ok(info)
}

/// The recorded daemon. A missing, partial or corrupt file reads as `None`.
pub fn read_pid_file(home: &Home) -> Option<PidInfo> {
    read_pid_path(&home.daemon_pid())
}

fn read_pid_path(path: &Path) -> Option<PidInfo> {
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let pid = match &data["pid"] {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))?,
        Value::String(s) => s.trim().parse().ok()?,
        Value::Bool(b) => i64::from(*b),
        _ => return None,
    };
    let started_at = match data.get("started_at")? {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Some(PidInfo { pid, started_at })
}

pub fn remove_pid_file(home: &Home) {
    let _ = std::fs::remove_file(home.daemon_pid());
}

/// The recorded daemon, only if its process is still alive. A stale PID
/// file (the process died without cleaning up) means "not running".
pub fn running_daemon(home: &Home) -> Option<PidInfo> {
    read_pid_file(home).filter(|i| is_process_alive(i.pid))
}

/// Asks the process at `pid` to shut down: SIGTERM on POSIX. Windows has
/// no deliverable graceful signal, so this is a hard terminate
/// there. Returns whether a signal was sent.
pub fn signal_stop(pid: i64) -> bool {
    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    let pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let Some(proc_) = sys.process(pid) else {
        return false;
    };
    proc_
        .kill_with(sysinfo::Signal::Term)
        .unwrap_or_else(|| proc_.kill())
}

/// Stops a running daemon and waits for it to exit. `Ok(false)` when no
/// daemon was running. `Err` carries a timeout message when it does
/// not stop in time.
pub fn stop_and_wait(home: &Home, timeout: Duration, action: &str) -> Result<bool, String> {
    let Some(info) = running_daemon(home) else {
        return Ok(false);
    };
    signal_stop(info.pid);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && is_process_alive(info.pid) {
        std::thread::sleep(Duration::from_millis(100));
    }
    if is_process_alive(info.pid) {
        return Err(format!(
            "daemon (pid {}) did not stop within {:.0}s; refusing to {action} while it is running",
            info.pid,
            timeout.as_secs_f64()
        ));
    }
    remove_pid_file(home);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tolerant_read() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        assert!(read_pid_file(&home).is_none());
        let info = write_pid_file(&home, 4242).unwrap();
        assert_eq!(read_pid_file(&home).unwrap(), info);
        std::fs::write(home.daemon_pid(), "{\"pid\": 1").unwrap();
        assert!(read_pid_file(&home).is_none());
        std::fs::write(home.daemon_pid(), "{\"pid\": \"7\", \"started_at\": 3}").unwrap();
        let i = read_pid_file(&home).unwrap();
        assert_eq!((i.pid, i.started_at.as_str()), (7, "3"));
        remove_pid_file(&home);
        assert!(read_pid_file(&home).is_none());
    }

    #[test]
    fn own_pid_runs_and_stale_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        write_pid_file(&home, i64::from(std::process::id())).unwrap();
        assert!(running_daemon(&home).is_some());
        write_pid_file(&home, i64::from(u32::MAX - 3)).unwrap();
        assert!(running_daemon(&home).is_none());
        assert_eq!(stop_and_wait(&home, Duration::from_secs(1), "x"), Ok(false));
    }
}
