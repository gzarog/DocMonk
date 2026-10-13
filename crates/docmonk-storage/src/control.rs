//! Control plane: source definitions plus their build state.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use docmonk_core::ids::make_source_id;
use docmonk_core::models::SourceType;

use crate::db::{now_iso, write_tx};
use crate::error::{Result, StorageError};
use crate::schema::{open_current, CONTROL_SCHEMA};
use crate::StorageLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    NeedsFullRebuild,
    Building,
    Ready,
    Failed,
}

impl BuildState {
    pub fn as_str(self) -> &'static str {
        match self {
            BuildState::NeedsFullRebuild => "needs_full_rebuild",
            BuildState::Building => "building",
            BuildState::Ready => "ready",
            BuildState::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "needs_full_rebuild" => BuildState::NeedsFullRebuild,
            "building" => BuildState::Building,
            "ready" => BuildState::Ready,
            "failed" => BuildState::Failed,
            other => {
                return Err(StorageError::Invalid(format!(
                    "unknown build_state {other:?}"
                )))
            }
        })
    }
}

/// Versions of everything that shapes index-derived data. A published
/// build records the versions it was built with; any difference in the
/// structural ones forces a full rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexVersions {
    pub parser_version: String,
    pub chunker_version: String,
    pub converter_version: String,
    pub embedding_model_id: Option<String>,
    pub embedding_text_version: Option<String>,
}

impl IndexVersions {
    /// Field names whose values differ from `other`.
    pub fn differences(&self, other: &IndexVersions) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.parser_version != other.parser_version {
            out.push("parser_version");
        }
        if self.chunker_version != other.chunker_version {
            out.push("chunker_version");
        }
        if self.converter_version != other.converter_version {
            out.push("converter_version");
        }
        if self.embedding_model_id != other.embedding_model_id {
            out.push("embedding_model_id");
        }
        if self.embedding_text_version != other.embedding_text_version {
            out.push("embedding_text_version");
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRecord {
    pub id: String,
    pub path: String,
    pub source_type: SourceType,
    pub enabled: bool,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceState {
    pub source_id: String,
    pub build_state: BuildState,
    pub rebuild_reason: Option<String>,
    pub online_status: String,
    pub active_build_id: Option<String>,
    pub pending_build_id: Option<String>,
    pub versions: Option<IndexVersions>,
    pub last_full_build_at: Option<String>,
    pub last_scan_at: Option<String>,
    pub last_error: Option<String>,
}

/// How the next indexing pass for a source must proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RebuildPlan {
    /// Process every file from the original source; reuse nothing.
    Full { reason: String },
    /// Only changed/new/deleted files; unchanged files of the active build
    /// may be reused.
    Incremental { active_build_id: String },
}

/// Pure decision: the plan for a source given its state and the versions
/// the running code would produce.
pub fn plan_for(state: &SourceState, current: &IndexVersions) -> RebuildPlan {
    let full = |reason: String| RebuildPlan::Full { reason };
    match state.build_state {
        BuildState::NeedsFullRebuild => full(
            state
                .rebuild_reason
                .clone()
                .unwrap_or_else(|| "needs_full_rebuild".into()),
        ),
        BuildState::Building | BuildState::Failed if state.active_build_id.is_none() => {
            full("no published build".into())
        }
        _ => match (&state.active_build_id, &state.versions) {
            (Some(active), Some(built)) => {
                let diff = built.differences(current);
                if diff.is_empty() {
                    RebuildPlan::Incremental {
                        active_build_id: active.clone(),
                    }
                } else {
                    full(format!("versions changed: {}", diff.join(", ")))
                }
            }
            _ => full("no published build".into()),
        },
    }
}

pub struct ControlPlane {
    conn: Connection,
}

fn json_list(s: &str) -> Result<Vec<String>> {
    serde_json::from_str(s).map_err(|e| StorageError::Invalid(format!("bad pattern list: {e}")))
}

fn source_from_row(row: &Row<'_>) -> rusqlite::Result<(SourceRecord, String, String)> {
    Ok((
        SourceRecord {
            id: row.get("id")?,
            path: row.get("path")?,
            source_type: if row.get::<_, String>("source_type")? == "network" {
                SourceType::Network
            } else {
                SourceType::Local
            },
            enabled: row.get::<_, i64>("enabled")? != 0,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        },
        row.get("include_patterns")?,
        row.get("exclude_patterns")?,
    ))
}

type RawState = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn state_row(row: &Row<'_>) -> rusqlite::Result<RawState> {
    Ok((
        row.get("source_id")?,
        row.get("build_state")?,
        row.get("rebuild_reason")?,
        row.get("online_status")?,
        row.get("active_build_id")?,
        row.get("pending_build_id")?,
        row.get("versions")?,
        row.get("last_full_build_at")?,
        row.get("last_scan_at")?,
        row.get("last_error")?,
    ))
}

fn state_from_raw(raw: RawState) -> Result<SourceState> {
    let (source_id, state, reason, online, active, pending, versions, full_at, scan_at, err) = raw;
    Ok(SourceState {
        source_id,
        build_state: BuildState::parse(&state)?,
        rebuild_reason: reason,
        online_status: online,
        active_build_id: active,
        pending_build_id: pending,
        versions: versions
            .map(|v| serde_json::from_str(&v))
            .transpose()
            .map_err(|e| StorageError::Invalid(format!("bad versions json: {e}")))?,
        last_full_build_at: full_at,
        last_scan_at: scan_at,
        last_error: err,
    })
}

/// A new source definition.
#[derive(Debug, Clone)]
pub struct NewSource {
    pub canonical_path: String,
    pub source_type: SourceType,
    pub enabled: bool,
    pub include_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
}

impl ControlPlane {
    /// Opens `<home>/state/control.db`, creating it at the current schema
    /// when missing and refusing any other schema.
    pub fn open(layout: &StorageLayout, cache_size_mb: i64) -> Result<Self> {
        let path = layout.control_db();
        let conn = open_current(&path, cache_size_mb, CONTROL_SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Adds a source (or returns the existing one with the same path). New
    /// sources always start in `needs_full_rebuild`.
    pub fn add_source(&mut self, new: &NewSource) -> Result<(SourceRecord, bool)> {
        if let Some(existing) = self.get_source_by_path(&new.canonical_path)? {
            return Ok((existing, false));
        }
        let id = make_source_id(&new.canonical_path);
        let now = now_iso();
        let reason = "new source";
        write_tx(&mut self.conn, |tx| {
            tx.execute(
                "INSERT INTO sources (id, path, source_type, enabled, include_patterns,
                    exclude_patterns, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    new.canonical_path,
                    new.source_type.as_str(),
                    i64::from(new.enabled),
                    serde_json::to_string(&new.include_patterns).unwrap_or_else(|_| "[]".into()),
                    serde_json::to_string(&new.exclude_patterns).unwrap_or_else(|_| "[]".into()),
                    now,
                    now,
                ],
            )
            .map_err(StorageError::sqlite("insert source"))?;
            tx.execute(
                "INSERT INTO source_state (source_id, build_state, rebuild_reason, updated_at)
                 VALUES (?1, 'needs_full_rebuild', ?2, ?3)",
                params![id, reason, now],
            )
            .map_err(StorageError::sqlite("insert source_state"))?;
            Ok(())
        })?;
        let rec = self
            .get_source(&id)?
            .ok_or_else(|| StorageError::NotFound(id.clone()))?;
        Ok((rec, true))
    }

    fn finish_source(&self, parts: (SourceRecord, String, String)) -> Result<SourceRecord> {
        let (mut rec, inc, exc) = parts;
        rec.include_patterns = json_list(&inc)?;
        rec.exclude_patterns = json_list(&exc)?;
        Ok(rec)
    }

    pub fn get_source(&self, id: &str) -> Result<Option<SourceRecord>> {
        let row = self
            .conn
            .query_row("SELECT * FROM sources WHERE id = ?1", [id], source_from_row)
            .optional()
            .map_err(StorageError::sqlite("get source"))?;
        row.map(|r| self.finish_source(r)).transpose()
    }

    pub fn get_source_by_path(&self, path: &str) -> Result<Option<SourceRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT * FROM sources WHERE path = ?1",
                [path],
                source_from_row,
            )
            .optional()
            .map_err(StorageError::sqlite("get source by path"))?;
        row.map(|r| self.finish_source(r)).transpose()
    }

    pub fn list_sources(&self, enabled_only: bool) -> Result<Vec<SourceRecord>> {
        let sql = if enabled_only {
            "SELECT * FROM sources WHERE enabled = 1 ORDER BY created_at, id"
        } else {
            "SELECT * FROM sources ORDER BY created_at, id"
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(StorageError::sqlite("list sources"))?;
        let rows = stmt
            .query_map([], source_from_row)
            .map_err(StorageError::sqlite("list sources"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StorageError::sqlite("list sources"))?;
        rows.into_iter().map(|r| self.finish_source(r)).collect()
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE sources SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
                params![i64::from(enabled), now_iso(), id],
            )
            .map_err(StorageError::sqlite("set enabled"))?;
        if n == 0 {
            return Err(StorageError::NotFound(format!("no such source: {id}")));
        }
        Ok(())
    }

    /// Removes a source definition and its index state (not its files).
    pub fn remove_source(&mut self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM sources WHERE id = ?1", [id])
            .map_err(StorageError::sqlite("remove source"))?;
        Ok(n > 0)
    }

    /// Every source's state in one statement (status reads the catalog
    /// and builds once per snapshot).
    pub fn all_states(&self) -> Result<Vec<SourceState>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM source_state ORDER BY source_id")
            .map_err(StorageError::sqlite("list source_state"))?;
        let rows = stmt
            .query_map([], state_row)
            .map_err(StorageError::sqlite("list source_state"))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StorageError::sqlite("list source_state"))?;
        rows.into_iter().map(state_from_raw).collect()
    }

    pub fn state(&self, source_id: &str) -> Result<SourceState> {
        let raw = self
            .conn
            .query_row(
                "SELECT * FROM source_state WHERE source_id = ?1",
                [source_id],
                state_row,
            )
            .optional()
            .map_err(StorageError::sqlite("get source_state"))?
            .ok_or_else(|| StorageError::NotFound(format!("no such source: {source_id}")))?;
        state_from_raw(raw)
    }

    /// Records whether the source root was reachable on the last pass.
    pub fn set_online(&mut self, source_id: &str, online: bool, error: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "UPDATE source_state SET online_status = ?1, last_error = COALESCE(?2, last_error),
                    last_scan_at = ?3, updated_at = ?3 WHERE source_id = ?4",
                params![
                    if online { "active" } else { "offline" },
                    error,
                    now_iso(),
                    source_id
                ],
            )
            .map_err(StorageError::sqlite("set online status"))?;
        Ok(())
    }

    /// Forces a full rebuild (e.g. `ragmonk rebuild`, version change).
    pub fn require_full_rebuild(&mut self, source_id: &str, reason: &str) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE source_state SET build_state = 'needs_full_rebuild', rebuild_reason = ?1,
                    updated_at = ?2 WHERE source_id = ?3",
                params![reason, now_iso(), source_id],
            )
            .map_err(StorageError::sqlite("require full rebuild"))?;
        if n == 0 {
            return Err(StorageError::NotFound(format!(
                "no such source: {source_id}"
            )));
        }
        Ok(())
    }

    /// Forgets every build of a source (its index data was deleted) and
    /// requires a full rebuild. Registration and scan settings stay.
    pub fn reset_index_state(&mut self, source_id: &str, reason: &str) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE source_state SET build_state = 'needs_full_rebuild', rebuild_reason = ?1,
                    active_build_id = NULL, pending_build_id = NULL, versions = NULL,
                    last_full_build_at = NULL, updated_at = ?2 WHERE source_id = ?3",
                params![reason, now_iso(), source_id],
            )
            .map_err(StorageError::sqlite("reset index state"))?;
        if n == 0 {
            return Err(StorageError::NotFound(format!(
                "no such source: {source_id}"
            )));
        }
        Ok(())
    }

    /// Records that a build started. The active build stays visible.
    pub fn begin_build(&mut self, source_id: &str, build_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE source_state SET build_state = 'building', pending_build_id = ?1,
                    updated_at = ?2 WHERE source_id = ?3",
                params![build_id, now_iso(), source_id],
            )
            .map_err(StorageError::sqlite("begin build"))?;
        Ok(())
    }

    /// Atomically switches the visible build. Only now does it become searchable.
    pub fn publish_build(
        &mut self,
        source_id: &str,
        build_id: &str,
        versions: &IndexVersions,
        full: bool,
    ) -> Result<()> {
        let now = now_iso();
        let versions =
            serde_json::to_string(versions).map_err(|e| StorageError::Invalid(e.to_string()))?;
        let n = self
            .conn
            .execute(
                "UPDATE source_state SET build_state = 'ready', rebuild_reason = NULL,
                    active_build_id = ?1, pending_build_id = NULL, versions = ?2,
                    last_full_build_at = CASE WHEN ?3 THEN ?4 ELSE last_full_build_at END,
                    last_error = NULL, updated_at = ?4
                 WHERE source_id = ?5 AND pending_build_id = ?1",
                params![build_id, versions, full, now, source_id],
            )
            .map_err(StorageError::sqlite("publish build"))?;
        if n == 0 {
            return Err(StorageError::Invalid(format!(
                "build {build_id} is not the pending build of {source_id}"
            )));
        }
        Ok(())
    }

    /// Abandons the pending build; the previously active build (if any)
    /// stays visible, otherwise the source still needs a full rebuild.
    pub fn abort_build(&mut self, source_id: &str, build_id: &str, error: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE source_state SET pending_build_id = NULL, last_error = ?1, updated_at = ?2,
                    build_state = CASE WHEN active_build_id IS NULL THEN 'needs_full_rebuild'
                                       ELSE 'failed' END
                 WHERE source_id = ?3 AND pending_build_id = ?4",
                params![error, now_iso(), source_id, build_id],
            )
            .map_err(StorageError::sqlite("abort build"))?;
        Ok(())
    }

    pub fn path_exists(path: &str) -> bool {
        Path::new(path).is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions() -> IndexVersions {
        IndexVersions {
            parser_version: "p1".into(),
            chunker_version: "c1".into(),
            converter_version: "d1".into(),
            embedding_model_id: None,
            embedding_text_version: None,
        }
    }

    fn plane(dir: &Path) -> ControlPlane {
        let home = docmonk_core::paths::Home::new(dir);
        ControlPlane::open(&StorageLayout::new(&home), 8).unwrap()
    }

    fn new_source(path: &str) -> NewSource {
        NewSource {
            canonical_path: path.into(),
            source_type: SourceType::Local,
            enabled: true,
            include_patterns: vec!["*.py".into()],
            exclude_patterns: vec![],
        }
    }

    #[test]
    fn new_sources_need_full_rebuild_and_publish_switches_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut cp = plane(dir.path());
        let (src, created) = cp.add_source(&new_source("/repo")).unwrap();
        assert!(created);
        assert_eq!(src.id, make_source_id("/repo"));
        assert_eq!(src.include_patterns, vec!["*.py"]);
        assert!(
            !cp.add_source(&new_source("/repo")).unwrap().1,
            "idempotent by path"
        );

        let st = cp.state(&src.id).unwrap();
        assert_eq!(st.build_state, BuildState::NeedsFullRebuild);
        assert!(matches!(
            plan_for(&st, &versions()),
            RebuildPlan::Full { .. }
        ));

        // A started build is not visible until published.
        cp.begin_build(&src.id, "b1").unwrap();
        let st = cp.state(&src.id).unwrap();
        assert_eq!(st.active_build_id, None);
        assert!(matches!(
            plan_for(&st, &versions()),
            RebuildPlan::Full { .. }
        ));
        // Aborting the first build leaves the source needing a full rebuild.
        cp.abort_build(&src.id, "b1", "boom").unwrap();
        assert_eq!(
            cp.state(&src.id).unwrap().build_state,
            BuildState::NeedsFullRebuild
        );

        cp.begin_build(&src.id, "b2").unwrap();
        assert!(cp
            .publish_build(&src.id, "other", &versions(), true)
            .is_err());
        cp.publish_build(&src.id, "b2", &versions(), true).unwrap();
        let st = cp.state(&src.id).unwrap();
        assert_eq!(st.build_state, BuildState::Ready);
        assert_eq!(
            plan_for(&st, &versions()),
            RebuildPlan::Incremental {
                active_build_id: "b2".into()
            }
        );

        // A failed later build keeps b2 visible.
        cp.begin_build(&src.id, "b3").unwrap();
        cp.abort_build(&src.id, "b3", "boom").unwrap();
        let st = cp.state(&src.id).unwrap();
        assert_eq!(
            (st.build_state, st.active_build_id.as_deref()),
            (BuildState::Failed, Some("b2"))
        );
        assert!(matches!(
            plan_for(&st, &versions()),
            RebuildPlan::Incremental { .. }
        ));
    }

    #[test]
    fn version_changes_force_full_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let mut cp = plane(dir.path());
        let (src, _) = cp.add_source(&new_source("/r")).unwrap();
        cp.begin_build(&src.id, "b").unwrap();
        cp.publish_build(&src.id, "b", &versions(), true).unwrap();
        let mut newer = versions();
        newer.chunker_version = "c2".into();
        match plan_for(&cp.state(&src.id).unwrap(), &newer) {
            RebuildPlan::Full { reason } => assert!(reason.contains("chunker_version")),
            other => panic!("expected full rebuild, got {other:?}"),
        }
        cp.require_full_rebuild(&src.id, "user requested").unwrap();
        assert!(matches!(
            plan_for(&cp.state(&src.id).unwrap(), &versions()),
            RebuildPlan::Full { .. }
        ));
    }
}
