#![allow(dead_code)]

use std::path::Path;

use docmonk_core::models::SourceType;
use docmonk_core::paths::Home;
use docmonk_indexing::coordinator::{
    index_all, NoProgress, Options, PrepareInput, ProcessError, Processor, Registry,
};
use docmonk_storage::control::{ControlPlane, NewSource, SourceRecord};
use docmonk_storage::knowledge::FileKnowledge;
use docmonk_storage::StorageLayout;

pub fn home(dir: &Path) -> Home {
    Home::new(dir.join("home"))
}

pub fn control(home: &Home) -> ControlPlane {
    ControlPlane::open(&StorageLayout::new(home), 8).unwrap()
}

pub fn add_source(cp: &mut ControlPlane, root: &Path) -> SourceRecord {
    std::fs::create_dir_all(root).unwrap();
    let canonical = docmonk_core::paths::resolve(root)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    cp.add_source(&NewSource {
        canonical_path: canonical,
        source_type: SourceType::Local,
        enabled: true,
        include_patterns: vec![],
        exclude_patterns: vec![],
    })
    .unwrap()
    .0
}

pub fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// `retry*.py` fails transiently, `bad*.py` permanently, the rest index.
pub struct Flaky;

impl Processor for Flaky {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        if input.rel_path.starts_with("retry") {
            return Err(ProcessError {
                code: "io_busy".into(),
                message: "file busy".into(),
                transient: true,
            });
        }
        if input.rel_path.starts_with("bad") {
            return Err(ProcessError {
                code: "parse".into(),
                message: "syntax".into(),
                transient: false,
            });
        }
        Ok(FileKnowledge::default())
    }
}

pub fn registry() -> Registry {
    let mut reg = Registry::raw();
    reg.code = std::sync::Arc::new(Flaky);
    reg
}

pub fn index(home: &Home, cp: &mut ControlPlane) {
    let opts = Options::from_config(&docmonk_config::DocMonkConfig::default());
    index_all(home, cp, &registry(), &opts, &mut NoProgress).unwrap();
}
