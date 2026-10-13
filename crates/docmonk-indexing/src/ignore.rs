//! Ignore rules: default excluded
//! directories, secret filenames, `.gitignore`/`.ragmonkignore`, source
//! exclude patterns, and include patterns that re-include an ignored path.

use std::path::{Component, Path, PathBuf};

use docmonk_core::security::{fnmatch, is_secret_filename};

pub const DEFAULT_EXCLUDE_DIRS: &[&str] = &[
    ".git",
    ".ragmonk",
    "node_modules",
    "bin",
    "obj",
    "dist",
    "build",
    "target",
    ".venv",
    "venv",
    "vendor",
    "coverage",
    ".cache",
    ".next",
    ".idea",
    ".vscode",
];

fn read_pattern_file(path: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Shell-style glob matching (case-insensitive on Windows).
fn fn_match(name: &str, pattern: &str) -> bool {
    let (n, p) = if cfg!(windows) {
        (
            name.to_lowercase().replace('/', "\\"),
            pattern.to_lowercase().replace('/', "\\"),
        )
    } else {
        (name.to_owned(), pattern.to_owned())
    };
    fnmatch(
        &n.chars().collect::<Vec<_>>(),
        &p.chars().collect::<Vec<_>>(),
    )
}

#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    root: PathBuf,
    patterns: Vec<String>,
    include_patterns: Vec<String>,
}

impl IgnoreMatcher {
    pub fn new(root: &Path, extra_patterns: &[String], include_patterns: &[String]) -> Self {
        let mut patterns = extra_patterns.to_vec();
        patterns.extend(read_pattern_file(&root.join(".gitignore")));
        patterns.extend(read_pattern_file(&root.join(".ragmonkignore")));
        Self {
            root: root.to_path_buf(),
            patterns,
            include_patterns: include_patterns.to_vec(),
        }
    }

    fn matches_any(patterns: &[String], rel_posix: &str, name: &str, is_dir: bool) -> bool {
        for pattern in patterns {
            let mut pat = pattern.as_str();
            let dir_only = pat.ends_with('/');
            if dir_only {
                pat = pat.trim_end_matches('/');
                if !is_dir {
                    continue;
                }
            }
            if pat.contains('/') {
                if fn_match(rel_posix, pat.trim_start_matches('/')) {
                    return true;
                }
            } else if fn_match(name, pat) {
                return true;
            }
        }
        false
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let parts: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        let rel_posix = parts.join("/");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if is_dir && DEFAULT_EXCLUDE_DIRS.contains(&name.as_str()) {
            return true;
        }
        if parts.len() > 1
            && parts[..parts.len() - 1]
                .iter()
                .any(|p| DEFAULT_EXCLUDE_DIRS.contains(&p.as_str()))
        {
            return true;
        }
        if !is_dir && is_secret_filename(&name) {
            return true;
        }
        if !Self::matches_any(&self.patterns, &rel_posix, &name, is_dir) {
            return false;
        }
        self.include_patterns.is_empty()
            || !Self::matches_any(&self.include_patterns, &rel_posix, &name, is_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_rules_apply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "# c\n*.log\nlogs/\n/docs/private/*\n",
        )
        .unwrap();
        let m = IgnoreMatcher::new(dir.path(), &["*.tmp".into()], &["keep.log".into()]);
        let p = |s: &str| dir.path().join(s);
        assert!(m.is_ignored(&p("a.log"), false));
        assert!(!m.is_ignored(&p("keep.log"), false), "include re-includes");
        assert!(m.is_ignored(&p("x.tmp"), false));
        assert!(m.is_ignored(&p("logs"), true));
        assert!(!m.is_ignored(&p("logs"), false), "dir-only pattern");
        assert!(m.is_ignored(&p("docs/private/a.md"), false));
        assert!(m.is_ignored(&p("node_modules"), true));
        assert!(m.is_ignored(&p("src/node_modules/x.js"), false));
        assert!(m.is_ignored(&p(".env"), false));
        assert!(!m.is_ignored(&p("src/main.rs"), false));
    }
}
