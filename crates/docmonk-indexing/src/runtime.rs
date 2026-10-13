//! Process and host probes shared by indexing runs, the daemon and
//! `ragmonk status` (which never treats them as cluster-wide proof).

/// Whether process `pid` exists on this host (safe, cross-platform). An
/// exited-but-unreaped child (zombie) is not running.
pub fn is_process_alive(pid: i64) -> bool {
    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    if pid == std::process::id() {
        return true;
    }
    let pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .is_some_and(|p| p.status() != sysinfo::ProcessStatus::Zombie)
}

/// This host's name as a safe token (`COMPUTERNAME`, `HOSTNAME`,
/// `/etc/hostname`; `host` when none is known).
pub fn host_name() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "host".into());
    crate::lock::sanitize_token(&host, 64).unwrap_or_else(|| "host".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_and_nonsense_is_not() {
        assert!(is_process_alive(i64::from(std::process::id())));
        assert!(!is_process_alive(-1));
        assert!(!is_process_alive(i64::from(u32::MAX) + 1));
    }

    #[test]
    fn exited_child_is_not_alive() {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = i64::from(child.id());
        child.wait().unwrap();
        assert!(!is_process_alive(pid));
    }

    #[test]
    fn host_name_is_a_token() {
        let h = host_name();
        assert!(!h.is_empty());
        assert!(h.len() <= 64);
    }
}
