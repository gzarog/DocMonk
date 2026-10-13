//! Secret-file filtering and source-root confinement (`ragmonk.security`).

use std::path::{Path, PathBuf};

use crate::errors::RagMonkError;

pub const SECRET_FILENAME_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "id_rsa",
    "id_ed25519",
    "credentials*",
    "secrets*",
];

/// Shell-style glob matching (`*`, `?`, `[...]`): case-insensitive on Windows
/// (`os.path.normcase`), case-sensitive elsewhere.
pub fn is_secret_filename(name: &str) -> bool {
    is_secret_filename_for(name, cfg!(windows))
}

pub fn is_secret_filename_for(name: &str, windows: bool) -> bool {
    let fold = |s: &str| {
        if windows {
            s.to_lowercase().replace('/', "\\")
        } else {
            s.to_owned()
        }
    };
    let name = fold(name);
    SECRET_FILENAME_PATTERNS.iter().any(|p| {
        fnmatch(
            &name.chars().collect::<Vec<_>>(),
            &fold(p).chars().collect::<Vec<_>>(),
        )
    })
}

/// Minimal `fnmatch.translate` matcher: `*`, `?`, `[seq]`, `[!seq]`.
pub fn fnmatch(name: &[char], pat: &[char]) -> bool {
    match pat.first() {
        None => name.is_empty(),
        Some('*') => (0..=name.len()).any(|i| fnmatch(&name[i..], &pat[1..])),
        Some('?') => !name.is_empty() && fnmatch(&name[1..], &pat[1..]),
        Some('[') => match parse_class(pat) {
            Some((negate, set, consumed)) => {
                let Some(c) = name.first() else {
                    return false;
                };
                let hit = set.iter().any(|&(lo, hi)| lo <= *c && *c <= hi);
                hit != negate && fnmatch(&name[1..], &pat[consumed..])
            }
            None => name.first() == Some(&'[') && fnmatch(&name[1..], &pat[1..]),
        },
        Some(p) => name.first() == Some(p) && fnmatch(&name[1..], &pat[1..]),
    }
}

type Class = (bool, Vec<(char, char)>, usize);

fn parse_class(pat: &[char]) -> Option<Class> {
    let mut i = 1;
    let negate = pat.get(i) == Some(&'!');
    if negate {
        i += 1;
    }
    let start = i;
    // A `]` right after `[` / `[!` is literal.
    if pat.get(i) == Some(&']') {
        i += 1;
    }
    while i < pat.len() && pat[i] != ']' {
        i += 1;
    }
    if i >= pat.len() {
        return None;
    }
    let body = &pat[start..i];
    let mut set = Vec::new();
    let mut j = 0;
    while j < body.len() {
        if j + 2 < body.len() && body[j + 1] == '-' {
            set.push((body[j], body[j + 2]));
            j += 3;
        } else {
            set.push((body[j], body[j]));
            j += 1;
        }
    }
    Some((negate, set, i + 1))
}

/// Confines filesystem access to configured roots; symlinks are resolved
/// before the containment check.
#[derive(Debug, Clone)]
pub struct PathGuard {
    roots: Vec<PathBuf>,
}

impl PathGuard {
    pub fn new(roots: &[PathBuf]) -> std::io::Result<Self> {
        let roots = roots
            .iter()
            .map(|r| crate::paths::resolve(r))
            .collect::<Result<_, _>>()?;
        Ok(Self { roots })
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn resolve(&self, path: &Path) -> Result<PathBuf, RagMonkError> {
        let resolved = crate::paths::resolve(path).map_err(|e| {
            RagMonkError::security(format!("cannot resolve '{}': {e}", path.display()))
        })?;
        if self.roots.iter().any(|root| resolved.starts_with(root)) {
            return Ok(resolved);
        }
        Err(RagMonkError::security(format!(
            "path '{}' resolves to '{}', which escapes all allowed source roots",
            path.display(),
            resolved.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_classes() {
        let m = |n: &str, p: &str| {
            fnmatch(
                &n.chars().collect::<Vec<_>>(),
                &p.chars().collect::<Vec<_>>(),
            )
        };
        assert!(m("a.pem", "*.pem"));
        assert!(m("b", "[a-c]"));
        assert!(!m("b", "[!a-c]"));
        assert!(m("[", "["));
        assert!(m("]", "[]]"));
    }

    #[test]
    fn windows_is_case_insensitive() {
        assert!(is_secret_filename_for("A.PEM", true));
        assert!(!is_secret_filename_for("A.PEM", false));
    }

    #[cfg(unix)]
    #[test]
    fn guard_rejects_traversal_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let guard = PathGuard::new(&[root.path().to_path_buf()]).unwrap();
        assert!(guard.resolve(&root.path().join("ok.txt")).is_ok());
        assert!(guard.resolve(&root.path().join("..").join("x")).is_err());
        let err = guard
            .resolve(&root.path().join("link").join("f"))
            .unwrap_err();
        assert_eq!(err.exit_code(), 8);
    }
}
