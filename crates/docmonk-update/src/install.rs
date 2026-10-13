//! `ragmonk update install` / `rollback` for a native install
//! of a binary release:
//!
//! 1. the latest release (a validated tag), stop if not newer;
//! 2. download `ragmonk-<ver>-<target>.<ext>` and `SHA256SUMS` (plus
//!    `SHA256SUMS.minisig` when a public key is compiled in) from that
//!    tag's assets, and verify them;
//! 3. unpack into `versions/<ver>.partial`, then rename it into place;
//! 4. self-check: the new binary's `version --json` must report `<ver>`;
//! 5. copy its bundled models into `<home>/models`, switch `current`;
//! 6. run the new binary's `doctor` (health), rolling back to the
//!    previous version if it cannot run at all.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::layout::{Layout, BINARY};
use crate::release::{self, Release};
use crate::{asset_name, verify, versioning, SIG_ASSET, SUMS_ASSET, TARGET};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    pub installed_version: String,
    pub upgraded: bool,
    pub healthy: bool,
}

/// Installs the latest release over the running `installed` version.
pub fn install_latest(
    home: &Path,
    installed: &str,
    channel: release::Channel,
) -> Result<InstallOutcome, String> {
    let layout = Layout::discover(home)?;
    let latest =
        release::fetch_latest(channel).map_err(|e| format!("could not check for updates: {e}"))?;
    if !versioning::is_newer(&latest.version, installed) {
        return Ok(InstallOutcome {
            installed_version: installed.to_owned(),
            upgraded: false,
            healthy: true,
        });
    }
    install_release(home, &layout, &latest, crate::minisign_public_key())
}

pub fn install_release(
    home: &Path,
    layout: &Layout,
    rel: &Release,
    public_key: Option<&str>,
) -> Result<InstallOutcome, String> {
    let version = rel.version.as_str();
    let name = asset_name(version, TARGET);
    let archive = release::download(&release::asset_url(&rel.tag_name, &name))?;
    let sums_bytes = release::download(&release::asset_url(&rel.tag_name, SUMS_ASSET))?;
    if let Some(key) = public_key {
        let sig = release::download(&release::asset_url(&rel.tag_name, SIG_ASSET))?;
        verify::check_signature(key, &sums_bytes, &String::from_utf8_lossy(&sig))?;
    }
    verify::check_digest(&archive, &String::from_utf8_lossy(&sums_bytes), &name)?;

    let target = layout.version_dir(version);
    if !target.join(BINARY).is_file() {
        let partial = layout.versions_dir().join(format!("{version}.partial"));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial)
            .map_err(|e| format!("cannot create {}: {e}", partial.display()))?;
        unpack(&archive, name.ends_with(".zip"), &partial)?;
        if !partial.join(BINARY).is_file() {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(format!(
                "{name} does not contain {BINARY}; refusing to install"
            ));
        }
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&partial, &target).map_err(|e| format!("cannot install {version}: {e}"))?;
    }

    if let Err(e) = self_check(&layout.binary(version), version) {
        if layout.read_state().current.as_deref() != Some(version) {
            let _ = std::fs::remove_dir_all(&target);
        }
        return Err(e);
    }
    copy_models(&target.join("models"), &home.join("models"))?;
    let state = layout.switch_to(version)?;

    let bin = layout.binary(version);
    let healthy = run_quiet(&bin, &["doctor", "--json"], home);
    if self_check(&bin, version).is_err() {
        if let Some(prev) = state.previous {
            layout.switch_to(&prev)?;
            return Err(format!(
                "{version} failed to start after install; rolled back to {prev}"
            ));
        }
    }
    layout.prune();
    Ok(InstallOutcome {
        installed_version: version.to_owned(),
        upgraded: true,
        healthy,
    })
}

/// Switches back to the previously installed version.
pub fn rollback(home: &Path) -> Result<(String, String), String> {
    let layout = Layout::discover(home)?;
    let state = layout.read_state();
    let (Some(current), Some(previous)) = (state.current, state.previous) else {
        return Err("there is no previous version to roll back to".into());
    };
    if !layout.binary(&previous).is_file() {
        return Err(format!(
            "the previous version {previous} is no longer installed"
        ));
    }
    let models = layout.version_dir(&previous).join("models");
    copy_models(&models, &home.join("models"))?;
    layout.switch_to(&previous)?;
    Ok((current, previous))
}

/// The new binary must run and report the version it was published as.
fn self_check(bin: &Path, version: &str) -> Result<(), String> {
    let out = Command::new(bin)
        .args(["version", "--json"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("the new binary does not run: {e}; refusing to install"))?;
    // `version --json` prints the CLI's JSON envelope:
    // `{"schema_version": "1", "data": {"version": ..}}`.
    let reported = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v["data"]["version"].as_str().map(str::to_owned));
    match reported {
        Some(v) if out.status.success() && versioning::normalize(&v) == version => Ok(()),
        other => Err(format!(
            "the new binary reports version {} instead of {version}; refusing to install",
            other.unwrap_or_else(|| "nothing".into())
        )),
    }
}

fn run_quiet(bin: &Path, args: &[&str], home: &Path) -> bool {
    Command::new(bin)
        .args(args)
        .env("RAGMONK_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The archive's single top-level directory is stripped; any entry that
/// would land outside `dest` is rejected.
fn unpack(archive: &[u8], zip: bool, dest: &Path) -> Result<(), String> {
    let bad = |e: &dyn std::fmt::Display| format!("corrupt release archive: {e}");
    let place = |raw: &Path| -> Result<Option<PathBuf>, String> {
        let mut parts = raw.components();
        parts.next(); // ragmonk-<ver>-<target>/
        let mut out = dest.to_path_buf();
        let mut any = false;
        for c in parts {
            match c {
                Component::Normal(p) => {
                    out.push(p);
                    any = true;
                }
                Component::CurDir => {}
                _ => return Err(format!("unsafe path in release archive: {}", raw.display())),
            }
        }
        Ok(any.then_some(out))
    };
    if zip {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(|e| bad(&e))?;
        for i in 0..z.len() {
            let mut f = z.by_index(i).map_err(|e| bad(&e))?;
            let Some(raw) = f.enclosed_name() else {
                return Err(format!("unsafe path in release archive: {}", f.name()));
            };
            let Some(path) = place(&raw)? else { continue };
            if f.is_dir() {
                std::fs::create_dir_all(&path).map_err(|e| bad(&e))?;
                continue;
            }
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(|e| bad(&e))?;
            }
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| bad(&e))?;
            std::fs::write(&path, buf).map_err(|e| bad(&e))?;
            #[cfg(unix)]
            if let Some(mode) = f.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o755));
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for entry in tar.entries().map_err(|e| bad(&e))? {
            let mut entry = entry.map_err(|e| bad(&e))?;
            let raw = entry.path().map_err(|e| bad(&e))?.into_owned();
            let Some(path) = place(&raw)? else { continue };
            match entry.header().entry_type() {
                tar::EntryType::Directory => {
                    std::fs::create_dir_all(&path).map_err(|e| bad(&e))?;
                }
                tar::EntryType::Regular => {
                    if let Some(p) = path.parent() {
                        std::fs::create_dir_all(p).map_err(|e| bad(&e))?;
                    }
                    entry.unpack(&path).map_err(|e| bad(&e))?;
                }
                // Links and devices have no place in a release archive.
                other => return Err(format!("unexpected {other:?} entry in release archive")),
            }
        }
    }
    Ok(())
}

/// Copies a release's bundled models over `<home>/models` (a release
/// without models leaves the existing ones alone).
fn copy_models(from: &Path, to: &Path) -> Result<(), String> {
    if !from.is_dir() {
        return Ok(());
    }
    let err = |e: std::io::Error| format!("cannot install the bundled models: {e}");
    for entry in std::fs::read_dir(from).map_err(err)? {
        let entry = entry.map_err(err)?;
        let dest = to.join(entry.file_name());
        if entry.file_type().map_err(err)?.is_dir() {
            copy_models(&entry.path(), &dest)?;
        } else {
            std::fs::create_dir_all(to).map_err(err)?;
            std::fs::copy(entry.path(), &dest).map_err(err)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn unpack_strips_top_dir() {
        let dir = tempfile::tempdir().unwrap();
        let a = targz(&[("r-1/ragmonk", b"bin"), ("r-1/models/m/a.onnx", b"m")]);
        unpack(&a, false, dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("ragmonk")).unwrap(), b"bin");
        assert!(dir.path().join("models/m/a.onnx").is_file());
        copy_models(&dir.path().join("models"), &dir.path().join("home/models")).unwrap();
        assert!(dir.path().join("home/models/m/a.onnx").is_file());
    }

    #[test]
    fn unpack_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        // tar::Builder refuses `..`, so write the header name directly.
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut h = tar::Header::new_gnu();
        h.as_gnu_mut().unwrap().name[..12].copy_from_slice(b"r/../../evil");
        h.set_size(1);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append(&h, &b"x"[..]).unwrap();
        let a = b.into_inner().unwrap().finish().unwrap();
        let e = unpack(&a, false, dir.path()).unwrap_err();
        assert!(e.contains("unsafe path"), "{e}");
    }

    #[test]
    fn unpack_zip() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file("r-1/ragmonk.exe", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"exe").unwrap();
        let a = w.finish().unwrap().into_inner();
        unpack(&a, true, dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("ragmonk.exe")).unwrap(),
            b"exe"
        );
    }
}
