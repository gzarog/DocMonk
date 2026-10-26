//! Git remote sources (ADR 0043): a repository registered by URL and
//! branch is a shallow, DocMonk-owned checkout under
//! `<home>/sources/git/<key>/checkout`, indexed like any folder source.
//!
//! Every git operation runs the system `git`, so credentials come only
//! from git's own helpers (credential managers, SSH agents); DocMonk never
//! stores them and refuses URLs that carry a password. Before each full
//! pass the checkout is fetched and reset to the branch head; the fetched
//! and the last indexed commit are kept in `remote.json` next to it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The metadata file next to a checkout.
pub const META: &str = "remote.json";
const CHECKOUT: &str = "checkout";

/// A git source's remote and checkout state (`remote.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRemote {
    pub url: String,
    pub branch: String,
    /// The indexed directory inside the repository (`None`: its root).
    #[serde(default)]
    pub subpath: Option<String>,
    /// The commit checked out by the last successful fetch.
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub fetched_at: Option<String>,
    /// The commit of the last pass that indexed this checkout.
    #[serde(default)]
    pub indexed_commit: Option<String>,
    /// Why the last fetch failed (`None` after a successful one).
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Whether `s` names a git remote rather than a local directory:
/// `https://`, `http://`, `ssh://`, `git://` and `file://` URLs, and the
/// scp-like `user@host:path` form.
pub fn is_remote_url(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if ["https://", "http://", "ssh://", "git://", "file://"]
        .iter()
        .any(|p| lower.starts_with(p))
    {
        return true;
    }
    match (s.find('@'), s.find(':')) {
        (Some(at), Some(colon)) => {
            at < colon && !s[..colon].contains('/') && !s.contains('\\') && colon + 1 < s.len()
        }
        _ => false,
    }
}

/// Refuses a URL that embeds a password or token (`scheme://user:secret@`):
/// DocMonk would store it in `remote.json` and show it in status.
pub fn check_url(url: &str) -> Result<(), String> {
    if let Some((_, rest)) = url.split_once("://") {
        let authority = rest.split('/').next().unwrap_or_default();
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            if userinfo.contains(':') {
                return Err(
                    "the git URL contains a password or token; remove it and let git's \
                     credential helper or SSH agent provide credentials"
                        .into(),
                );
            }
        }
    }
    Ok(())
}

/// `<home>/sources/git`: every git checkout DocMonk owns.
pub fn sources_root(home_root: &Path) -> PathBuf {
    home_root.join("sources").join("git")
}

/// The checkout directory of `(url, branch, subpath)`: one per
/// combination, so adding the same remote again is idempotent.
pub fn remote_dir(home_root: &Path, url: &str, branch: &str, subpath: Option<&str>) -> PathBuf {
    let mut h = Sha256::new();
    for part in [url, branch, subpath.unwrap_or_default()] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    let digest = h.finalize();
    let key: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    sources_root(home_root).join(key)
}

/// The remote directory (holding [`META`] and the checkout) of a source
/// whose root is a DocMonk git checkout, or `None` for a folder source.
pub fn remote_dir_of(source_path: &Path) -> Option<PathBuf> {
    source_path.ancestors().find_map(|a| {
        let parent = a.parent()?;
        (a.file_name()? == CHECKOUT && parent.join(META).is_file()).then(|| parent.to_path_buf())
    })
}

/// The remote metadata of a git source (`None` for a folder source).
pub fn remote_of(source_path: &Path) -> Option<GitRemote> {
    read_meta(&remote_dir_of(source_path)?).ok()
}

pub fn read_meta(dir: &Path) -> Result<GitRemote, String> {
    let raw = std::fs::read(dir.join(META)).map_err(|e| format!("git: {e}"))?;
    serde_json::from_slice(&raw).map_err(|e| format!("git: unreadable {META}: {e}"))
}

fn write_meta(dir: &Path, meta: &GitRemote) -> Result<(), String> {
    let tmp = dir.join(format!("{META}.tmp"));
    let body = serde_json::to_vec_pretty(meta).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body)
        .and_then(|_| std::fs::rename(&tmp, dir.join(META)))
        .map_err(|e| format!("git: cannot write {META}: {e}"))
}

/// Runs `git` non-interactively (a missing credential fails instead of
/// prompting, so a daemon never hangs). Returns trimmed stdout.
fn git(cwd: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut c = Command::new("git");
    if let Some(d) = cwd {
        c.arg("-C").arg(d);
    }
    let out = c
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: cannot run git ({e}); install git to use git sources"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
            .or_else(|| err.lines().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("failed");
        Err(format!(
            "git {}: {line}",
            args.first().copied().unwrap_or("")
        ))
    }
}

/// A subpath inside the repository: relative, without `..`.
fn checked_subpath(subpath: Option<&str>) -> Result<Option<String>, String> {
    let Some(s) = subpath
        .map(|s| s.trim().trim_matches('/'))
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let p = Path::new(s);
    if p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "git subpath must be a relative path inside the repository: {s}"
        ));
    }
    Ok(Some(s.replace('\\', "/")))
}

/// Clones `url` (shallow, one branch: `branch`, or the remote's default)
/// unless this remote is already checked out. Returns the directory to
/// register as the source root (the checkout, or its `subpath`).
pub fn clone(
    home_root: &Path,
    url: &str,
    branch: Option<&str>,
    subpath: Option<&str>,
) -> Result<PathBuf, String> {
    check_url(url)?;
    let subpath = checked_subpath(subpath)?;
    let branch = branch.map(str::trim).filter(|b| !b.is_empty());
    let dir = remote_dir(
        home_root,
        url,
        branch.unwrap_or_default(),
        subpath.as_deref(),
    );
    let checkout = dir.join(CHECKOUT);
    if !(dir.join(META).is_file() && checkout.join(".git").exists()) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("git: {e}"))?;
        let tmp = dir.join("checkout.tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        let tmp_s = tmp.to_string_lossy().into_owned();
        let mut args = vec!["clone", "--depth", "1", "--single-branch", "--no-tags"];
        if let Some(b) = branch {
            args.extend(["--branch", b]);
        }
        args.extend(["--", url, &tmp_s]);
        if let Err(e) = git(None, &args) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        let resolved = match branch {
            Some(b) => b.to_owned(),
            None => git(Some(&tmp), &["rev-parse", "--abbrev-ref", "HEAD"])?,
        };
        let commit = git(Some(&tmp), &["rev-parse", "HEAD"])?;
        let _ = std::fs::remove_dir_all(&checkout);
        std::fs::rename(&tmp, &checkout).map_err(|e| format!("git: {e}"))?;
        write_meta(
            &dir,
            &GitRemote {
                url: url.to_owned(),
                branch: resolved,
                subpath: subpath.clone(),
                commit: Some(commit),
                fetched_at: Some(crate::progress::now_iso()),
                ..GitRemote::default()
            },
        )?;
    }
    let root = match &subpath {
        Some(s) => checkout.join(s),
        None => checkout,
    };
    if !root.is_dir() {
        return Err(format!(
            "git subpath is not a directory in the repository: {}",
            subpath.unwrap_or_default()
        ));
    }
    Ok(root)
}

/// Fetches the branch head and, when it moved, resets the checkout to it
/// (DocMonk owns the checkout, so local changes are discarded). A failed
/// fetch (unreachable remote, deleted branch, refused credentials) leaves
/// the checkout as it was and is recorded in `last_error`.
pub fn sync(dir: &Path) -> Result<GitRemote, String> {
    let mut meta = read_meta(dir)?;
    let checkout = dir.join(CHECKOUT);
    let tracking = format!("refs/remotes/origin/{}", meta.branch);
    let refspec = format!("+refs/heads/{}:{tracking}", meta.branch);
    let fetched = git(
        Some(&checkout),
        &["fetch", "--depth", "1", "--no-tags", "origin", &refspec],
    )
    .map_err(|e| {
        if e.contains("couldn't find remote ref") {
            format!(
                "git: branch '{}' no longer exists on {}",
                meta.branch, meta.url
            )
        } else {
            e
        }
    })
    .and_then(|_| git(Some(&checkout), &["rev-parse", &tracking]));
    let head = match fetched {
        Ok(h) => h,
        Err(e) => {
            meta.last_error = Some(e.clone());
            let _ = write_meta(dir, &meta);
            return Err(e);
        }
    };
    if meta.commit.as_deref() != Some(head.as_str())
        || git(Some(&checkout), &["rev-parse", "HEAD"]).ok().as_deref() != Some(head.as_str())
    {
        git(Some(&checkout), &["reset", "--hard", "--quiet", &head])?;
        git(Some(&checkout), &["clean", "-ffdxq"])?;
    }
    meta.commit = Some(head);
    meta.fetched_at = Some(crate::progress::now_iso());
    meta.last_error = None;
    write_meta(dir, &meta)?;
    Ok(meta)
}

/// The remote's current head of the branch, without fetching objects
/// (`git ls-remote`); `None` when the branch is gone.
pub fn remote_head(dir: &Path) -> Result<Option<String>, String> {
    let meta = read_meta(dir)?;
    let out = git(
        Some(&dir.join(CHECKOUT)),
        &[
            "ls-remote",
            "origin",
            &format!("refs/heads/{}", meta.branch),
        ],
    )?;
    Ok(out.split_whitespace().next().map(str::to_owned))
}

/// Records that a pass indexed `commit`.
pub fn mark_indexed(dir: &Path, commit: &str) -> Result<(), String> {
    let mut meta = read_meta(dir)?;
    meta.indexed_commit = Some(commit.to_owned());
    write_meta(dir, &meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_are_told_from_paths() {
        for u in [
            "https://github.com/o/r.git",
            "ssh://git@host/o/r",
            "git@github.com:o/r.git",
            "file:///srv/r.git",
        ] {
            assert!(is_remote_url(u), "{u}");
        }
        for p in [
            "/home/me/repo",
            "./repo",
            "C:\\work\\repo",
            "~/repo",
            "dir/with@at:colon",
        ] {
            assert!(!is_remote_url(p), "{p}");
        }
    }

    #[test]
    fn passwords_in_urls_are_refused() {
        assert!(check_url("https://user:tok@github.com/o/r").is_err());
        assert!(check_url("https://user@github.com/o/r").is_ok());
        assert!(check_url("git@github.com:o/r.git").is_ok());
    }

    #[test]
    fn subpaths_stay_inside_the_repository() {
        assert_eq!(
            checked_subpath(Some("/docs/")).unwrap(),
            Some("docs".into())
        );
        assert_eq!(checked_subpath(Some("  ")).unwrap(), None);
        assert!(checked_subpath(Some("../x")).is_err());
        assert!(checked_subpath(Some("a/../../x")).is_err());
    }

    #[test]
    fn one_directory_per_remote_branch_and_subpath() {
        let h = Path::new("/h");
        let a = remote_dir(h, "u", "main", None);
        assert_eq!(a, remote_dir(h, "u", "main", None));
        assert_ne!(a, remote_dir(h, "u", "dev", None));
        assert_ne!(a, remote_dir(h, "u", "main", Some("docs")));
        assert!(a.starts_with("/h/sources/git"));
    }
}
