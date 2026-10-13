//! `cargo xtask package` and `cargo xtask release-manifest`.
//!
//! `package` writes `docmonk-<ver>-<target>.tar.gz` (`.zip` for Windows
//! targets) holding one top-level directory with the binary, the bundled
//! models under `models/`, README.md and LICENSE, and adds or replaces the
//! archive's line in `<out>/SHA256SUMS`. Entries are sorted and carry a
//! fixed mtime, so the same inputs give the same archive.
//!
//! `release-manifest` writes `<dist>/SHA256SUMS` for every release archive
//! in `<dist>` (replacing per-target `SHA256SUMS.*` fragments), so the
//! published checksums always cover exactly the published archives.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// 2026-01-01T00:00:00Z.
const MTIME: u64 = 1_767_225_600;
const SUMS: &str = "SHA256SUMS";

pub struct PackageArgs {
    pub binary: PathBuf,
    pub version: String,
    pub target: String,
    pub models: Option<PathBuf>,
    pub out: PathBuf,
}

impl PackageArgs {
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let (mut binary, mut version, mut target, mut models, mut out) =
            (None, None, None, None, None);
        let mut args = args;
        while let Some(flag) = args.next() {
            let mut value = || args.next().with_context(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--binary" => binary = Some(PathBuf::from(value()?)),
                "--version" => version = Some(value()?),
                "--target" => target = Some(value()?),
                "--models" => models = Some(PathBuf::from(value()?)),
                "--out" => out = Some(PathBuf::from(value()?)),
                other => bail!("unknown option {other}"),
            }
        }
        let version: String = version.context("--version is required")?;
        let version = version.strip_prefix('v').unwrap_or(&version).to_owned();
        if !is_semver(&version) {
            bail!("--version must be MAJOR.MINOR.PATCH[-PRERELEASE], got {version:?}");
        }
        Ok(Self {
            binary: binary.context("--binary is required")?,
            version,
            target: target.context("--target is required")?,
            models,
            out: out.context("--out is required")?,
        })
    }
}

/// Strict `MAJOR.MINOR.PATCH` with an optional `-PRERELEASE` of
/// alphanumerics, dots and hyphens.
pub fn is_semver(v: &str) -> bool {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
        && pre.is_none_or(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        })
}

/// `(path in archive, source file, unix mode)`, sorted.
fn entries(root: &Path, a: &PackageArgs, windows: bool) -> Result<Vec<(String, PathBuf, u32)>> {
    let name = if windows { "docmonk.exe" } else { "docmonk" };
    if !a.binary.is_file() {
        bail!("binary {} does not exist", a.binary.display());
    }
    let mut out = vec![(name.to_owned(), a.binary.clone(), 0o755)];
    for doc in ["README.md", "LICENSE"] {
        if root.join(doc).is_file() {
            out.push((doc.to_owned(), root.join(doc), 0o644));
        }
    }
    if let Some(models) = &a.models {
        let mut files = Vec::new();
        collect_files(models, &mut files)?;
        files.sort();
        for path in files {
            let rel = path.strip_prefix(models)?;
            let rel: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push((format!("models/{}", rel.join("/")), path, 0o644));
        }
    }
    Ok(out)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Builds the archive and records it in `SHA256SUMS`; returns its path.
pub fn package(root: &Path, a: &PackageArgs) -> Result<PathBuf> {
    let windows = a.target.contains("windows");
    let top = format!("docmonk-{}-{}", a.version, a.target);
    let archive = a
        .out
        .join(format!("{top}.{}", if windows { "zip" } else { "tar.gz" }));
    let files = entries(root, a, windows)?;
    std::fs::create_dir_all(&a.out)?;
    if windows {
        let mut z = zip::ZipWriter::new(std::fs::File::create(&archive)?);
        let when = zip::DateTime::from_date_and_time(2026, 1, 1, 0, 0, 0)
            .map_err(|e| anyhow::anyhow!("zip timestamp: {e}"))?;
        for (rel, src, mode) in &files {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .last_modified_time(when)
                .unix_permissions(*mode);
            z.start_file(format!("{top}/{rel}"), opts)?;
            z.write_all(&std::fs::read(src)?)?;
        }
        z.finish()?;
    } else {
        let gz = flate2::GzBuilder::new().mtime(MTIME as u32).write(
            std::fs::File::create(&archive)?,
            flate2::Compression::default(),
        );
        let mut t = tar::Builder::new(gz);
        for (rel, src, mode) in &files {
            let data = std::fs::read(src)?;
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_mtime(MTIME);
            h.set_uid(0);
            h.set_gid(0);
            h.set_entry_type(tar::EntryType::Regular);
            t.append_data(&mut h, format!("{top}/{rel}"), &data[..])?;
        }
        t.into_inner()?.finish()?;
    }
    update_sums(&a.out, &archive)?;
    Ok(archive)
}

fn sha256_file(path: &Path) -> Result<String> {
    let digest = Sha256::digest(std::fs::read(path)?);
    Ok(digest.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

fn write_sums(path: &Path, mut lines: Vec<(String, String)>) -> Result<()> {
    lines.sort_by(|a, b| a.1.cmp(&b.1));
    let text: String = lines
        .iter()
        .map(|(digest, name)| format!("{digest}  {name}\n"))
        .collect();
    std::fs::write(path, text)?;
    Ok(())
}

fn read_sums(path: &Path) -> Result<Vec<(String, String)>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            Some((
                parts.next()?.to_owned(),
                parts.next()?.trim_start_matches('*').to_owned(),
            ))
        })
        .collect())
}

fn update_sums(out: &Path, archive: &Path) -> Result<()> {
    let name = archive
        .file_name()
        .context("archive name")?
        .to_string_lossy()
        .into_owned();
    let mut lines: Vec<_> = read_sums(&out.join(SUMS))?
        .into_iter()
        .filter(|(_, n)| *n != name)
        .collect();
    lines.push((sha256_file(archive)?, name));
    write_sums(&out.join(SUMS), lines)
}

fn is_release_archive(name: &str) -> bool {
    name.starts_with("docmonk-") && (name.ends_with(".tar.gz") || name.ends_with(".zip"))
}

/// Writes `<dist>/SHA256SUMS` over every release archive in `dist` and
/// removes `SHA256SUMS.*` fragments. Returns the archive names.
pub fn release_manifest(dist: &Path) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(dist).with_context(|| format!("reading {}", dist.display()))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with(&format!("{SUMS}.")) && !name.ends_with(".minisig") {
            std::fs::remove_file(&path)?;
        } else if path.is_file() && is_release_archive(&name) {
            lines.push((sha256_file(&path)?, name));
        }
    }
    if lines.is_empty() {
        bail!(
            "no docmonk-*.tar.gz / docmonk-*.zip archives in {}",
            dist.display()
        );
    }
    let names = lines.iter().map(|(_, n)| n.clone()).collect();
    write_sums(&dist.join(SUMS), lines)?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_is_strict() {
        for ok in ["1.0.0", "0.9.1", "1.0.0-rc.1", "10.20.30"] {
            assert!(is_semver(ok), "{ok}");
        }
        for bad in ["1.0", "v1.0.0", "1.0.0.0", "01.0.0", "1.0.0-", "1.x.0", ""] {
            assert!(!is_semver(bad), "{bad}");
        }
    }

    #[test]
    fn archives_are_reproducible_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("models/m")).unwrap();
        std::fs::write(root.join("README.md"), "readme").unwrap();
        std::fs::write(root.join("models/m/w.bin"), "weights").unwrap();
        let bin = root.join("docmonk");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let args = |out: &Path, target: &str| PackageArgs {
            binary: bin.clone(),
            version: "1.0.0".into(),
            target: target.into(),
            models: Some(root.join("models")),
            out: out.to_path_buf(),
        };
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let first = package(&root, &args(&a, "x86_64-unknown-linux-gnu")).unwrap();
        let second = package(&root, &args(&b, "x86_64-unknown-linux-gnu")).unwrap();
        assert_eq!(
            std::fs::read(&first).unwrap(),
            std::fs::read(&second).unwrap()
        );
        package(&root, &args(&a, "x86_64-pc-windows-msvc")).unwrap();
        let sums = std::fs::read_to_string(a.join(SUMS)).unwrap();
        assert_eq!(sums.lines().count(), 2, "{sums}");
        assert!(sums.contains("  docmonk-1.0.0-x86_64-pc-windows-msvc.zip\n"));

        // The tar.gz holds one top directory with binary, docs and models.
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(&first).unwrap());
        let mut names: Vec<String> = tar::Archive::new(gz)
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "docmonk-1.0.0-x86_64-unknown-linux-gnu/README.md",
                "docmonk-1.0.0-x86_64-unknown-linux-gnu/docmonk",
                "docmonk-1.0.0-x86_64-unknown-linux-gnu/models/m/w.bin",
            ]
        );

        // The manifest replaces fragments and covers exactly the archives.
        std::fs::rename(a.join(SUMS), a.join("SHA256SUMS.x86_64")).unwrap();
        let listed = release_manifest(&a).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(!a.join("SHA256SUMS.x86_64").exists());
        assert_eq!(std::fs::read_to_string(a.join(SUMS)).unwrap(), sums);
    }
}
