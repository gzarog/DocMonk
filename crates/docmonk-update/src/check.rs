//! The explicit check, the detached background check
//! and the startup notice.
//! The background check and the notice never fail or slow the calling
//! command: every error is swallowed, and the notice only reads the cache.

use std::path::Path;

use crate::cache::{self, UpdateCache};
use crate::{release, versioning};

/// Queries the latest release and refreshes the cache. The "already
/// notified" marker carries over only while the latest version is
/// unchanged, so each new release is announced exactly once.
pub fn check_now(
    home: &Path,
    installed: &str,
    channel: release::Channel,
) -> Result<UpdateCache, String> {
    let latest = release::fetch_latest(channel)?;
    let carried = cache::read(home)
        .filter(|p| p.latest_version == latest.version)
        .and_then(|p| p.last_notified_version);
    let entry = UpdateCache {
        last_checked: cache::now_iso(),
        installed_version: installed.to_owned(),
        latest_version: latest.version,
        release_url: latest.html_url,
        last_notified_version: carried,
    };
    let _ = cache::write(home, &entry);
    Ok(entry)
}

/// Whether a command should spawn the background check.
pub fn background_due(home: &Path, enabled: bool, interval_hours: i64) -> bool {
    enabled && cache::is_stale(cache::read(home).as_ref(), interval_hours)
}

/// The notice for a newer cached release, at most once per version.
pub fn take_notice(home: &Path, installed: &str, enabled: bool, notify: bool) -> Option<String> {
    if !(enabled && notify) {
        return None;
    }
    let cached = cache::read(home)?;
    if !versioning::is_newer(&cached.latest_version, installed)
        || cached.last_notified_version.as_deref() == Some(cached.latest_version.as_str())
    {
        return None;
    }
    let text = format!(
        "\nA newer RagMonk version is available: {installed} → {}\nRun `ragmonk update install` to upgrade.",
        cached.latest_version
    );
    let _ = cache::write(
        home,
        &UpdateCache {
            last_notified_version: Some(cached.latest_version.clone()),
            ..cached
        },
    );
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_once_per_version() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert!(take_notice(home, "1.0.0", true, true).is_none());
        let c = UpdateCache {
            last_checked: cache::now_iso(),
            installed_version: "1.0.0".into(),
            latest_version: "1.1.0".into(),
            release_url: String::new(),
            last_notified_version: None,
        };
        cache::write(home, &c).unwrap();
        assert!(take_notice(home, "1.0.0", true, false).is_none());
        assert!(take_notice(home, "1.1.0", true, true).is_none());
        let n = take_notice(home, "1.0.0", true, true).unwrap();
        assert!(n.contains("1.0.0 → 1.1.0"), "{n}");
        assert!(take_notice(home, "1.0.0", true, true).is_none());
        assert!(!background_due(home, true, 24));
        assert!(!background_due(home, false, 24));
    }
}
