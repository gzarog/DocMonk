//! Version parsing and comparison.
//!
//! A release tag is untrusted input, so it must be exactly
//! `MAJOR.MINOR.PATCH` (an optional leading `v`). The installed side is
//! more lenient: a development build (`0.3.0-dev+abc`, or `0.0.0` from an
//! untagged build) compares by its numbers, with a pre-release older than
//! the release itself.

/// Strips a leading `v`/`V`.
pub fn normalize(version: &str) -> &str {
    version
        .strip_prefix('v')
        .or_else(|| version.strip_prefix('V'))
        .unwrap_or(version)
}

/// `(major, minor, patch)` of a strict `MAJOR.MINOR.PATCH`.
pub fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let v = normalize(version);
    let mut parts = v.split('.');
    let mut num = || -> Option<u64> {
        let p = parts.next()?;
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let out = (num()?, num()?, num()?);
    parts.next().is_none().then_some(out)
}

pub fn is_valid(version: &str) -> bool {
    parse(version).is_some()
}

/// `(major, minor, patch, is_release)` of an installed version.
fn parse_installed(version: &str) -> Option<(u64, u64, u64, bool)> {
    let v = normalize(version.trim());
    let (core, rest) = match v.find(['-', '+']) {
        Some(i) => (&v[..i], &v[i..]),
        None => (v, ""),
    };
    let (a, b, c) = parse(core)?;
    Some((a, b, c, !rest.starts_with('-')))
}

/// True when the untrusted `candidate` is strictly newer than
/// `installed`. An invalid candidate, or an installed version that
/// cannot be read, is never "newer".
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    let (Some((a, b, c)), Some((x, y, z, release))) =
        (parse(candidate), parse_installed(installed))
    else {
        return false;
    };
    match (a, b, c).cmp(&(x, y, z)) {
        std::cmp::Ordering::Greater => true,
        // `1.2.0` is newer than its own pre-release `1.2.0-rc1`.
        std::cmp::Ordering::Equal => !release,
        std::cmp::Ordering::Less => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_tags() {
        assert_eq!(parse("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse("1.2.3"), Some((1, 2, 3)));
        for bad in [
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "v1.2.3-rc1",
            "",
            "1..3",
            "+1.2.3",
        ] {
            assert!(!is_valid(bad), "{bad}");
        }
    }

    #[test]
    fn newer() {
        assert!(is_newer("v1.2.4", "1.2.3"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.2.2", "v1.2.3"));
        assert!(is_newer("1.2.3", "1.2.3-dev.4+g1234"));
        assert!(!is_newer("1.2.3", "1.2.3+build5"));
        assert!(is_newer("0.0.1", "0.0.0"));
        assert!(!is_newer("garbage", "0.0.0"));
        assert!(!is_newer("1.0.0", "not-a-version"));
    }
}
