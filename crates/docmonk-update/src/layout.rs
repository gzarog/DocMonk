//! The native install layout, shared with `install.sh`/`install.ps1`:
//!
//! ```text
//! <install_dir>/
//!   versions/<ver>/ragmonk[.exe]   one unpacked release archive per version
//!   versions/<ver>/models/...      the models bundled with that release
//!   current -> versions/<ver>      POSIX: a relative symlink
//!   bin/ragmonk.exe                Windows: a copy of the current binary
//!   install_state.json             {"current": <ver>, "previous": <ver>|null}
//! ```
//!
//! On POSIX `<bin_dir>/ragmonk` links to `<install_dir>/current/ragmonk`,
//! so switching versions is one atomic rename of the `current` symlink.
//! Windows cannot replace a running executable, but it can rename one:
//! the old `bin/ragmonk.exe` becomes `ragmonk.exe.old` (removed on the
//! next switch) and the new one is copied in. The current and the
//! previous version are kept; older ones are pruned.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const BINARY: &str = if cfg!(windows) {
    "ragmonk.exe"
} else {
    "ragmonk"
};

/// `<RAGMONK_HOME>/install_info.json`, written by the native installers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallInfo {
    pub install_method: String,
    #[serde(default)]
    pub repository: String,
    pub install_dir: Option<String>,
    #[serde(default)]
    pub bin_dir: Option<String>,
}

pub const NATIVE: &str = "native";

pub fn read_install_info(home: &Path) -> Option<InstallInfo> {
    read_json(&home.join("install_info.json"))
}

/// Tolerates the UTF-8 BOM Windows PowerShell 5 writes.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallState {
    pub current: Option<String>,
    pub previous: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The native install this binary belongs to: `install_info.json`'s
    /// `install_dir` when it says `native`, else the directory two levels
    /// above a binary running from `versions/<ver>/`.
    pub fn discover(home: &Path) -> Result<Self, String> {
        if let Some(info) = read_install_info(home) {
            if info.install_method == NATIVE {
                if let Some(dir) = info.install_dir.filter(|d| !d.is_empty()) {
                    return Ok(Self::new(dir));
                }
            }
        }
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .map_err(|e| format!("cannot locate the running binary: {e}"))?;
        let ver_dir = exe.parent();
        let versions = ver_dir.and_then(Path::parent);
        if let (Some(versions), Some(root)) = (versions, versions.and_then(Path::parent)) {
            if versions.file_name().is_some_and(|n| n == "versions") {
                return Ok(Self::new(root));
            }
        }
        Err(
            "this RagMonk was not installed by the native installer, so it cannot \
             update itself; reinstall with install.sh / install.ps1 or update it the \
             way it was installed"
                .into(),
        )
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join("versions")
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions_dir().join(version)
    }

    pub fn binary(&self, version: &str) -> PathBuf {
        self.version_dir(version).join(BINARY)
    }

    pub fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("install_state.json")
    }

    pub fn read_state(&self) -> InstallState {
        read_json(&self.state_path()).unwrap_or_default()
    }

    pub fn write_state(&self, state: &InstallState) -> std::io::Result<()> {
        let path = self.state_path();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(state).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &path)
    }

    /// Makes `version` (already unpacked under `versions/`) the current
    /// one, and remembers the version it replaces for `rollback`.
    pub fn switch_to(&self, version: &str) -> Result<InstallState, String> {
        if !self.binary(version).is_file() {
            return Err(format!("version {version} is not installed"));
        }
        let old = self.read_state();
        self.point_current(version)
            .map_err(|e| format!("could not switch to {version}: {e}"))?;
        let state = InstallState {
            current: Some(version.to_owned()),
            previous: old.current.filter(|c| c != version).or(old.previous),
        };
        self.write_state(&state)
            .map_err(|e| format!("could not record the install state: {e}"))?;
        Ok(state)
    }

    #[cfg(unix)]
    fn point_current(&self, version: &str) -> std::io::Result<()> {
        let link = self.current_link();
        let tmp = self.root.join(".current.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(Path::new("versions").join(version), &tmp)?;
        std::fs::rename(&tmp, &link)
    }

    #[cfg(windows)]
    fn point_current(&self, version: &str) -> std::io::Result<()> {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin)?;
        let exe = bin.join(BINARY);
        // A `.old` still running (a daemon) cannot be removed; park the
        // current exe under a fresh name instead.
        let mut old = bin.join(format!("{BINARY}.old"));
        if std::fs::remove_file(&old).is_err() && old.exists() {
            old = bin.join(format!("{BINARY}.old-{}", std::process::id()));
        }
        if exe.exists() {
            std::fs::rename(&exe, &old)?;
        }
        if let Err(e) = std::fs::copy(self.binary(version), &exe) {
            let _ = std::fs::rename(&old, &exe);
            return Err(e);
        }
        std::fs::write(self.current_link(), version)
    }

    /// Removes every version but the current and previous one, plus any
    /// half-unpacked `.partial` directory.
    pub fn prune(&self) {
        let state = self.read_state();
        let keep = [state.current, state.previous];
        let Ok(entries) = std::fs::read_dir(self.versions_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !keep.iter().flatten().any(|k| *k == name) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fake_version(layout: &Layout, v: &str) {
        std::fs::create_dir_all(layout.version_dir(v)).unwrap();
        std::fs::write(layout.binary(v), v).unwrap();
    }

    #[test]
    fn switch_rollback_prune() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        for v in ["1.0.0", "1.1.0", "1.2.0"] {
            fake_version(&layout, v);
        }
        assert!(layout.switch_to("9.9.9").is_err());
        layout.switch_to("1.0.0").unwrap();
        layout.switch_to("1.1.0").unwrap();
        let s = layout.switch_to("1.2.0").unwrap();
        assert_eq!(s.current.as_deref(), Some("1.2.0"));
        assert_eq!(s.previous.as_deref(), Some("1.1.0"));
        let cur = std::fs::read_to_string(layout.current_link().join(BINARY)).unwrap();
        assert_eq!(cur, "1.2.0");
        std::fs::create_dir_all(layout.versions_dir().join("1.3.0.partial")).unwrap();
        layout.prune();
        let mut left: Vec<_> = std::fs::read_dir(layout.versions_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["1.1.0", "1.2.0"]);
        // Re-switching to the current version keeps the previous one.
        let s = layout.switch_to("1.2.0").unwrap();
        assert_eq!(s.previous.as_deref(), Some("1.1.0"));
    }

    #[test]
    fn discover_from_install_info() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("install_info.json"),
            r#"{"install_method":"native","install_dir":"/opt/rm"}"#,
        )
        .unwrap();
        assert_eq!(
            Layout::discover(home.path()).unwrap().root,
            Path::new("/opt/rm")
        );
    }
}
