#![allow(dead_code)]

use std::path::{Path, PathBuf};

use docmonk_core::models::SourceType;
use docmonk_core::paths::project_id_for_canonical;
use docmonk_core::paths::Home;
use docmonk_indexing::coordinator::{run_source, NoProgress, Options};
use docmonk_ml::manifest::DEFAULT_EMBEDDING_MODEL;
use docmonk_storage::control::{ControlPlane, NewSource, SourceRecord};
use docmonk_storage::knowledge::ProjectStore;
use docmonk_storage::StorageLayout;

pub fn home(dir: &Path) -> Home {
    Home::new(dir.join("home"))
}

pub fn control(home: &Home) -> ControlPlane {
    ControlPlane::open(&StorageLayout::new(home), 8).unwrap()
}

pub fn add_source(
    cp: &mut ControlPlane,
    root: &Path,
    include: &[&str],
    exclude: &[&str],
) -> SourceRecord {
    let canonical = docmonk_core::paths::resolve(root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    cp.add_source(&NewSource {
        canonical_path: canonical,
        source_type: SourceType::Local,
        enabled: true,
        include_patterns: include.iter().map(|s| s.to_string()).collect(),
        exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
    })
    .unwrap()
    .0
}

pub fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

pub fn bump_mtime(path: &Path, secs: u64) {
    let meta = std::fs::metadata(path).unwrap();
    let t = meta.modified().unwrap() + std::time::Duration::from_secs(secs);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_tree(&p, &dest);
        } else {
            std::fs::copy(&p, dest).unwrap();
        }
    }
}

/// `DOCMONK_MODELS_DIR` when it holds the default model.
pub fn models_root() -> Option<PathBuf> {
    let root = std::env::var_os("DOCMONK_MODELS_DIR").map(PathBuf::from);
    match root {
        Some(r)
            if r.join(DEFAULT_EMBEDDING_MODEL.slug)
                .join("model.safetensors")
                .exists() =>
        {
            Some(r)
        }
        _ if std::env::var("DOCMONK_REQUIRE_MODELS").as_deref() == Ok("1") => {
            panic!("DOCMONK_REQUIRE_MODELS=1 but the embedding model is not installed")
        }
        _ => {
            eprintln!("skipping: embedding model not installed (set DOCMONK_MODELS_DIR)");
            None
        }
    }
}

pub struct Fx {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub layout: StorageLayout,
    pub cp: ControlPlane,
    pub src: SourceRecord,
}

pub fn fixture() -> Fx {
    fixture_of("fixtures/linking")
}

/// A fresh source over a copy of `<repo>/<rel>`.
pub fn fixture_of(rel: &str) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("corpus");
    copy_tree(&repo_root().join(rel), &root);
    let home = home(tmp.path());
    let layout = StorageLayout::new(&home);
    let mut cp = control(&home);
    let src = add_source(&mut cp, &root, &[], &[]);
    Fx {
        _tmp: tmp,
        root,
        layout,
        cp,
        src,
    }
}

impl Fx {
    pub fn index(&mut self, models_root: &Path) {
        let mut cfg = docmonk_config::DocMonkConfig::default();
        cfg.search.semantic = true;
        let opts = docmonk_convert::RegistryOptions {
            models_root: Some(models_root.to_path_buf()),
            ..Default::default()
        };
        let r = run_source(
            &self.layout,
            &mut self.cp,
            &self.src,
            &docmonk_convert::registry_with(&cfg, &opts),
            &Options::from_config(&cfg),
            &mut NoProgress,
        )
        .unwrap();
        assert!(r.published || r.build_id.is_some(), "{r:?}");
    }

    pub fn store(&self) -> (ProjectStore, String) {
        let active = self
            .cp
            .state(&self.src.id)
            .unwrap()
            .active_build_id
            .unwrap();
        let s = ProjectStore::open(
            &self.layout,
            &project_id_for_canonical(&self.src.path),
            &self.src.id,
            8,
        )
        .unwrap();
        (s, active)
    }
}
