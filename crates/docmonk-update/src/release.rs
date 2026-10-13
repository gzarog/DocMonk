//! The latest release of this project's own repository.
//! HTTPS only, no project data sent, and a tag
//! that is not a strict `MAJOR.MINOR.PATCH` is rejected.
//!
//! Debug builds read `DOCMONK_UPDATE_TEST_BASE` (a stand-in for both the
//! API and the download host) so the tests can serve fake releases.
//! Release builds ignore it: nothing in config or the environment can
//! point an update elsewhere.

use std::time::Duration;

use serde_json::Value;

use crate::{versioning, GITHUB_OWNER, GITHUB_REPO};

pub const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Normalized (no leading `v`).
    pub version: String,
    /// The git tag the release lives under.
    pub tag_name: String,
    pub html_url: String,
}

/// The test stand-in host (debug builds only).
pub fn test_base() -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var("DOCMONK_UPDATE_TEST_BASE")
            .ok()
            .filter(|b| !b.is_empty())
            .map(|b| b.trim_end_matches('/').to_owned())
    } else {
        None
    }
}

fn latest_url() -> String {
    match test_base() {
        Some(base) => format!("{base}/api/releases/latest"),
        None => {
            format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest")
        }
    }
}

/// Where a release asset is downloaded from: built from the validated
/// tag, never from a URL in the release description.
pub fn asset_url(tag: &str, name: &str) -> String {
    match test_base() {
        Some(base) => format!("{base}/download/{tag}/{name}"),
        None => format!(
            "https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/download/{tag}/{name}"
        ),
    }
}

pub fn client(timeout: Duration) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .user_agent(format!("docmonk/{}", docmonk_core::version::version()))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Which releases `docmonk update` follows (`updates.channel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// GitHub's "latest release": full releases only (the default).
    Stable,
    /// The newest `MAJOR.MINOR.PATCH` release, pre-releases included.
    Prerelease,
}

impl Channel {
    /// `updates.channel`; anything but `prerelease` is the stable channel.
    pub fn from_config(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("prerelease") {
            Self::Prerelease
        } else {
            Self::Stable
        }
    }
}

fn releases_url() -> String {
    match test_base() {
        Some(base) => format!("{base}/api/releases"),
        None => format!(
            "https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases?per_page=30"
        ),
    }
}

fn get_json(url: &str, what: &str) -> Result<Value, String> {
    let resp = client(TIMEOUT)
        .get(url)
        .header("accept", "application/vnd.github+json")
        .send()
        .map_err(|e| format!("GitHub is unreachable: {e}"))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(format!("GitHub returned HTTP {status} for {what}"));
    }
    resp.json()
        .map_err(|e| format!("GitHub returned a non-JSON response: {e}"))
}

/// A release from one entry of GitHub's release JSON.
fn parse_release(data: &Value) -> Result<Release, String> {
    let tag = data["tag_name"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or("GitHub's latest-release response had no tag_name")?;
    let version = versioning::normalize(tag).to_owned();
    if !versioning::is_valid(&version) {
        return Err(format!(
            "latest release tag '{tag}' is not a valid MAJOR.MINOR.PATCH version"
        ));
    }
    Ok(Release {
        version,
        tag_name: tag.to_owned(),
        html_url: data["html_url"].as_str().unwrap_or_default().to_owned(),
    })
}

/// The newest strict-`MAJOR.MINOR.PATCH` release in a release list that
/// ships an archive for `target`, drafts excluded and pre-releases
/// included. Entries with other tags, and releases without a native
/// archive for this platform (such as older releases), are
/// skipped, never an error.
pub fn newest_release(list: &Value, target: &str) -> Result<Release, String> {
    list.as_array()
        .ok_or("GitHub's release list was not a JSON array")?
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false))
        .filter_map(|r| parse_release(r).ok().map(|rel| (r, rel)))
        .filter(|(r, rel)| {
            let want = crate::asset_name(&rel.version, target);
            r["assets"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x["name"].as_str() == Some(want.as_str())))
        })
        .map(|(_, rel)| rel)
        .max_by_key(|r| versioning::parse(&r.version))
        .ok_or_else(|| format!("GitHub has no release with a native archive for {target}"))
}

/// The release `channel` points at, or why it could not be determined.
pub fn fetch_latest(channel: Channel) -> Result<Release, String> {
    match channel {
        Channel::Stable => parse_release(&get_json(&latest_url(), "the latest release")?),
        Channel::Prerelease => newest_release(
            &get_json(&releases_url(), "the release list")?,
            crate::TARGET,
        ),
    }
}

/// Downloads a release asset fully into memory (archives are tens of MB).
pub fn download(url: &str) -> Result<Vec<u8>, String> {
    let resp = client(Duration::from_secs(600))
        .get(url)
        .send()
        .map_err(|e| format!("download failed: {url}: {e}"))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(format!("download failed: {url}: HTTP {status}"));
    }
    resp.bytes()
        .map(|b| b.to_vec())
        .map_err(|e| format!("download failed: {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn channel_from_config() {
        assert_eq!(Channel::from_config("stable"), Channel::Stable);
        assert_eq!(Channel::from_config(" Prerelease "), Channel::Prerelease);
        assert_eq!(Channel::from_config("beta"), Channel::Stable);
    }

    #[test]
    fn newest_release_needs_a_native_archive() {
        let t = "x86_64-unknown-linux-gnu";
        let with = |v: &str| json!([{"name": crate::asset_name(v, t)}, {"name": "SHA256SUMS"}]);
        let list = json!([
            // An older release: newest version, but no native archive.
            {"tag_name": "v0.3.30", "html_url": "a", "assets": [{"name": "docmonk-0.3.30.tar.gz"}]},
            {"tag_name": "v0.10.0", "html_url": "d", "draft": true, "assets": with("0.10.0")},
            {"tag_name": "v0.0.6", "html_url": "b", "prerelease": true, "assets": with("0.0.6")},
            {"tag_name": "release-0.0.9", "html_url": "c", "assets": with("0.0.9")},
            {"tag_name": "v0.0.5", "html_url": "e", "prerelease": true, "assets": with("0.0.5")}
        ]);
        let r = newest_release(&list, t).unwrap();
        assert_eq!(
            (r.version.as_str(), r.tag_name.as_str()),
            ("0.0.6", "v0.0.6")
        );
        assert!(newest_release(&list, "aarch64-apple-darwin").is_err());
        assert!(newest_release(&json!({}), t).is_err());
    }
}
