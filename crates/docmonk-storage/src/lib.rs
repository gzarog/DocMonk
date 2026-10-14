//! DocMonk local storage.
//!
//! Layout inside a DocMonk home:
//!
//! ```text
//! <home>/state/control.db                       control plane (sources, build state)
//! <home>/projects/<project_id>/knowledge.db     per-source knowledge
//! ```
//!
//! Each database is created directly at its current schema
//! ([`schema`]); an existing database with any other schema is refused,
//! never converted. Every new source starts in
//! [`control::BuildState::NeedsFullRebuild`].

pub mod control;
pub mod db;
pub mod error;
pub mod graph;
pub mod knowledge;
pub mod maintenance;
pub mod read;
pub mod schema;
pub mod search;
pub mod stats;
pub mod status;
pub mod vectors;

pub use error::StorageError;

use std::path::{Path, PathBuf};

/// Storage paths under a DocMonk home.
#[derive(Debug, Clone)]
pub struct StorageLayout {
    state: PathBuf,
    projects: PathBuf,
}

impl StorageLayout {
    pub fn new(home: &docmonk_core::paths::Home) -> Self {
        Self {
            state: home.state_dir(),
            projects: home.projects_dir(),
        }
    }

    /// A layout rooted elsewhere (server mode's disposable staging area).
    pub fn at(root: &Path) -> Self {
        Self {
            state: root.join("state"),
            projects: root.join("projects"),
        }
    }

    /// `<home>/state`.
    pub fn state_dir(&self) -> &Path {
        &self.state
    }
    pub fn control_db(&self) -> PathBuf {
        self.state.join("control.db")
    }
    pub fn project_dir(&self, project_id: &str) -> PathBuf {
        self.projects.join(project_id)
    }
    pub fn project_db(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join("knowledge.db")
    }
}
