//! On-disk runtime layout. `RAGMONK_HOME` always
//! wins; otherwise `%LOCALAPPDATA%\RagMonk` on Windows and `~/.ragmonk`
//! elsewhere (macOS included).

use std::path::{Component, Path, PathBuf};

use crate::ids::sha256_hex;

/// Environment lookup used for path resolution, injectable for tests.
pub trait Env {
    fn var(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var_os(key).map(|v| v.to_string_lossy().into_owned())
    }
}

impl Env for std::collections::BTreeMap<String, String> {
    fn var(&self, key: &str) -> Option<String> {
        self.get(key).cloned()
    }
}

/// The user's home directory.
pub fn home_dir(env: &dyn Env, windows: bool) -> PathBuf {
    let candidates: &[&str] = if windows { &["USERPROFILE"] } else { &["HOME"] };
    candidates
        .iter()
        .find_map(|k| env.var(k).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if windows { "C:\\" } else { "/" }))
}

/// Home-directory expansion of a leading `~` / `~/...`.
pub fn expand_user(path: &str, env: &dyn Env, windows: bool) -> PathBuf {
    let is_sep = |c: char| c == '/' || (windows && c == '\\');
    if path == "~" {
        return home_dir(env, windows);
    }
    if let Some(rest) = path.strip_prefix('~') {
        if rest.starts_with(is_sep) {
            return home_dir(env, windows).join(&rest[1..]);
        }
    }
    PathBuf::from(path)
}

pub fn runtime_dir_with(env: &dyn Env, windows: bool) -> PathBuf {
    if let Some(over) = env.var("RAGMONK_HOME").filter(|v| !v.is_empty()) {
        return expand_user(&over, env, windows);
    }
    if windows {
        if let Some(base) = env.var("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(base).join("RagMonk");
        }
        return home_dir(env, windows)
            .join("AppData")
            .join("Local")
            .join("RagMonk");
    }
    home_dir(env, windows).join(".ragmonk")
}

/// The RagMonk home for this process.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_with(&ProcessEnv, cfg!(windows))
}

/// Layout of one RagMonk home. Every path is part of the on-disk contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn discover() -> Self {
        Self::new(runtime_dir())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    /// `<home>/state`: the control database and other durable state.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn control_db(&self) -> PathBuf {
        self.state_dir().join("control.db")
    }
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    pub fn user_config(&self) -> PathBuf {
        self.root.join("config.yaml")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
    pub fn daemon_pid(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }
    pub fn daemon_health(&self) -> PathBuf {
        self.root.join("daemon_health.json")
    }
    pub fn index_progress(&self) -> PathBuf {
        self.root.join("index_progress.json")
    }
    pub fn install_info(&self) -> PathBuf {
        self.root.join("install_info.json")
    }
    pub fn update_cache(&self) -> PathBuf {
        self.root.join("update.json")
    }
    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }
    pub fn project_dir(&self, project_id: &str) -> PathBuf {
        self.projects_dir().join(project_id)
    }
    pub fn project_db(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("knowledge.db")
    }
    pub fn project_cache_dir(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("cache")
    }
    pub fn project_state_dir(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("state")
    }
    pub fn project_vector_index(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("vectors.usearch")
    }
    pub fn project_vector_meta(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("vectors.meta.json")
    }

    /// `ensure_runtime_layout`: creates the home and its fixed subdirs.
    pub fn ensure_layout(&self) -> std::io::Result<&Path> {
        for dir in [
            self.root.clone(),
            self.state_dir(),
            self.projects_dir(),
            self.logs_dir(),
            self.backups_dir(),
            self.locks_dir(),
            self.tmp_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(&self.root)
    }

    /// `ensure_project_layout`.
    pub fn ensure_project_layout(&self, project_id: &str) -> std::io::Result<PathBuf> {
        for dir in [
            self.project_dir(project_id),
            self.project_cache_dir(project_id),
            self.project_state_dir(project_id),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(self.project_dir(project_id))
    }
}

/// `./.ragmonk.yaml` relative to `cwd`.
pub fn project_config_path(cwd: &Path) -> PathBuf {
    cwd.join(".ragmonk.yaml")
}

/// Non-strict path resolution (like `realpath`): components
/// are walked left to right, every existing prefix is resolved through the
/// filesystem (symlinks, `/var` -> `/private/var`), a missing component is
/// appended literally and `..` pops the path built so far. Windows results
/// avoid the `\\?\` verbatim prefix.
pub fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    let mut missing = false;
    for comp in absolute.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                // Once a component is missing, nothing below it can exist.
                if !missing {
                    match dunce::canonicalize(&out) {
                        Ok(real) => out = real,
                        Err(_) => missing = true,
                    }
                }
            }
        }
        // `..` out of a missing directory may land on an existing one again.
        if missing && out.exists() {
            if let Ok(real) = dunce::canonicalize(&out) {
                out = real;
                missing = false;
            }
        }
    }
    Ok(normalize_lexically(&out))
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `project_id_for_path`: first 12 hex chars of sha256(resolved path).
pub fn project_id_for_path(path: &Path) -> std::io::Result<String> {
    let canonical = resolve(path)?;
    Ok(project_id_for_canonical(&canonical.to_string_lossy()))
}

pub fn project_id_for_canonical(canonical: &str) -> String {
    sha256_hex(canonical.as_bytes())[..12].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn ragmonk_home_wins_and_expands_tilde() {
        let e = env(&[("RAGMONK_HOME", "~/rm"), ("HOME", "/home/u")]);
        assert_eq!(runtime_dir_with(&e, false), PathBuf::from("/home/u/rm"));
        let e = env(&[("RAGMONK_HOME", "/x"), ("LOCALAPPDATA", "C:\\L")]);
        assert_eq!(runtime_dir_with(&e, true), PathBuf::from("/x"));
    }

    #[test]
    fn platform_defaults() {
        let e = env(&[("HOME", "/home/u")]);
        assert_eq!(
            runtime_dir_with(&e, false),
            PathBuf::from("/home/u/.ragmonk")
        );
        let e = env(&[("LOCALAPPDATA", "L")]);
        assert_eq!(
            runtime_dir_with(&e, true),
            PathBuf::from("L").join("RagMonk")
        );
        let e = env(&[("USERPROFILE", "U")]);
        assert_eq!(
            runtime_dir_with(&e, true),
            PathBuf::from("U")
                .join("AppData")
                .join("Local")
                .join("RagMonk")
        );
        // An empty override is ignored, like an unset one.
        let e = env(&[("RAGMONK_HOME", ""), ("HOME", "/h")]);
        assert_eq!(runtime_dir_with(&e, false), PathBuf::from("/h/.ragmonk"));
    }

    #[test]
    fn resolve_handles_missing_tail_and_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let real = dunce::canonicalize(dir.path()).unwrap();
        let p = dir.path().join("a").join("..").join("b").join("c");
        assert_eq!(resolve(&p).unwrap(), real.join("b").join("c"));
        assert_eq!(resolve(dir.path()).unwrap(), real);
        // `..` back out of a missing directory into an existing one.
        std::fs::create_dir(dir.path().join("x")).unwrap();
        let p = dir.path().join("missing").join("..").join("x").join("y");
        assert_eq!(resolve(&p).unwrap(), real.join("x").join("y"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_follows_symlinks_in_existing_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let real = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("link")).unwrap();
        let p = dir.path().join("link").join("new");
        assert_eq!(resolve(&p).unwrap(), real.join("target").join("new"));
    }

    #[test]
    fn ensure_layout_creates_home_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("h"));
        home.ensure_layout().unwrap();
        for sub in ["projects", "logs", "backups", "locks", "tmp"] {
            assert!(home.root().join(sub).is_dir(), "{sub}");
        }
        home.ensure_project_layout("abc").unwrap();
        assert!(home.project_cache_dir("abc").is_dir());
        assert!(home.project_state_dir("abc").is_dir());
    }
}
