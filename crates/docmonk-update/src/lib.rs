//! Native self-update: `ragmonk update check|status|install|
//! rollback`, the throttled background check and the startup notice.
//!
//! * [`versioning`]: strict `MAJOR.MINOR.PATCH` for untrusted release tags.
//! * [`release`]: the latest release of this project's own repository
//!   (hardcoded; never taken from config or the environment).
//! * [`cache`]: `<home>/update.json` and its schema.
//! * [`verify`]: SHA-256 against `SHA256SUMS`, and a minisign signature
//!   over it when a public key is compiled in.
//! * [`layout`]: `<install>/versions/<ver>/` with an atomically switched
//!   current version and the previous one kept for rollback.
//! * [`install`]: download → verify → unpack → switch → self-check.

pub mod cache;
pub mod check;
pub mod install;
pub mod layout;
pub mod release;
pub mod verify;
pub mod versioning;

/// This project's own repository; updates never come from anywhere else.
pub const GITHUB_OWNER: &str = "gzarog";
pub const GITHUB_REPO: &str = "RagMonk";

/// The Rust target triple this binary was built for.
pub const TARGET: &str = env!("RAGMONK_TARGET");

/// The release archive for `version` on this target:
/// `ragmonk-<ver>-<target>.tar.gz` (`.zip` on Windows).
pub fn asset_name(version: &str, target: &str) -> String {
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("ragmonk-{version}-{target}.{ext}")
}

pub const SUMS_ASSET: &str = "SHA256SUMS";
pub const SIG_ASSET: &str = "SHA256SUMS.minisig";

/// The minisign public key releases are verified with, when one was
/// configured at build time (`RAGMONK_MINISIGN_PUBKEY`). Without one only
/// the checksums are verified.
pub fn minisign_public_key() -> Option<&'static str> {
    option_env!("RAGMONK_MINISIGN_PUBKEY").filter(|k| !k.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names() {
        assert_eq!(
            asset_name("1.2.0", "x86_64-unknown-linux-gnu"),
            "ragmonk-1.2.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("1.2.0", "x86_64-pc-windows-msvc"),
            "ragmonk-1.2.0-x86_64-pc-windows-msvc.zip"
        );
        assert!(TARGET.contains(std::env::consts::ARCH));
    }
}
