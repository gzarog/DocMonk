//! Version/build metadata. Release builds of the Rust binary set
//! `RAGMONK_BUILD_VERSION` (and optionally `RAGMONK_BUILD_COMMIT`) at compile
//! time so both report the same release string.

pub fn version() -> &'static str {
    match option_env!("RAGMONK_BUILD_VERSION") {
        Some(v) if !v.is_empty() => v,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

pub fn commit() -> Option<&'static str> {
    option_env!("RAGMONK_BUILD_COMMIT").filter(|c| !c.is_empty())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub commit: Option<&'static str>,
    pub target_os: &'static str,
    pub target_arch: &'static str,
    pub profile: &'static str,
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        version: version(),
        commit: commit(),
        target_os: std::env::consts::OS,
        target_arch: std::env::consts::ARCH,
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    }
}
