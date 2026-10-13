//! `<home>/update.json`: written by `update check`
//! and the background check, read by `update status`, the startup notice
//! and the Admin UI. A missing or unreadable cache is "not checked yet".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCache {
    pub last_checked: String,
    pub installed_version: String,
    pub latest_version: String,
    pub release_url: String,
    #[serde(default)]
    pub last_notified_version: Option<String>,
}

pub fn path(home: &Path) -> PathBuf {
    home.join("update.json")
}

pub fn read(home: &Path) -> Option<UpdateCache> {
    serde_json::from_str(&std::fs::read_to_string(path(home)).ok()?).ok()
}

/// Write-then-rename, so a concurrent reader never sees half a file.
pub fn write(home: &Path, cache: &UpdateCache) -> std::io::Result<()> {
    let target = path(home);
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = target.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(cache).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &target)
}

/// UTC now, ISO 8601 with offset (`datetime.now(UTC).isoformat()`).
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Seconds since the epoch of an ISO 8601 timestamp (`Z` or `±HH:MM`).
pub fn parse_iso(ts: &str) -> Option<i64> {
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, dd) = (d.next()??, d.next()??, d.next()??);
    let (clock, offset) = if let Some(c) = time.strip_suffix('Z') {
        (c, 0)
    } else if let Some(i) = time.rfind(['+', '-']) {
        let (c, o) = time.split_at(i);
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let mut parts = o[1..].split(':').map(|p| p.parse::<i64>().ok());
        let (oh, om) = (parts.next()??, parts.next().flatten().unwrap_or(0));
        (c, sign * (oh * 3600 + om * 60))
    } else {
        (time, 0)
    };
    let mut c = clock.split(':');
    let (h, mi) = (
        c.next()?.parse::<i64>().ok()?,
        c.next()?.parse::<i64>().ok()?,
    );
    let s = c
        .next()
        .and_then(|s| s.split('.').next())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    Some(days_from_civil(y, m, dd) * 86_400 + h * 3600 + mi * 60 + s - offset)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Whether a cache is missing or older than `interval_hours`.
pub fn is_stale(cache: Option<&UpdateCache>, interval_hours: i64) -> bool {
    let Some(checked) = cache.and_then(|c| parse_iso(&c.last_checked)) else {
        return true;
    };
    let now = parse_iso(&now_iso()).unwrap_or(0);
    now - checked > interval_hours * 3600
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        let now = now_iso();
        let t = parse_iso(&now).unwrap();
        assert_eq!(parse_iso("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_iso("2026-01-01T00:00:00Z"), Some(1_767_225_600));
        assert_eq!(
            parse_iso("2026-01-01T01:00:00.5+01:00"),
            Some(1_767_225_600)
        );
        assert!(t > 1_767_225_600);
    }

    #[test]
    fn staleness_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_none());
        assert!(is_stale(None, 24));
        let c = UpdateCache {
            last_checked: now_iso(),
            installed_version: "1.0.0".into(),
            latest_version: "1.1.0".into(),
            release_url: "u".into(),
            last_notified_version: None,
        };
        write(dir.path(), &c).unwrap();
        assert_eq!(read(dir.path()), Some(c.clone()));
        assert!(!is_stale(Some(&c), 24));
        let old = UpdateCache {
            last_checked: "2020-01-01T00:00:00+00:00".into(),
            ..c
        };
        assert!(is_stale(Some(&old), 24));
        std::fs::write(path(dir.path()), "{").unwrap();
        assert!(read(dir.path()).is_none());
    }
}
