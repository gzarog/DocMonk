//! Read-only status queries over one project store: one aggregate
//! statement per published build, the recent error log and per-file error
//! times.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::knowledge::ProjectStore;

/// One recorded indexing error.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ErrorRecord {
    pub path: Option<String>,
    pub error_code: String,
    pub error_message: String,
    pub occurred_at: String,
}

/// A file's `(last_indexed_at, updated_at)`.
pub type FileTimes = (Option<String>, Option<String>);

/// Every counter `ragmonk status` shows for one build, read by a single
/// SQL statement (one consistent read snapshot under WAL).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BuildSummary {
    pub files: i64,
    pub indexed: i64,
    pub failed: i64,
    pub retry: i64,
    pub next_retry_at: Option<String>,
    pub max_attempt_count: i64,
    pub entities: i64,
    pub documents: i64,
    pub chunks: i64,
    pub relationships: i64,
    pub links: i64,
    /// When the build was published (`builds.finished_at`).
    pub published_at: Option<String>,
    /// The relationship graph's state record (raw `metadata` JSON).
    pub graph_state: Option<String>,
}

impl ProjectStore {
    /// [`BuildSummary`] of `build_id`.
    pub fn build_summary(&self, build_id: &str) -> Result<BuildSummary> {
        Ok(self
            .query_rows(
                "build summary",
                "SELECT
                    (SELECT COUNT(*) FROM files WHERE build_id = ?1),
                    (SELECT COUNT(*) FROM files WHERE build_id = ?1 AND status = 'indexed'),
                    (SELECT COUNT(*) FROM files WHERE build_id = ?1 AND status = 'failed'),
                    (SELECT COUNT(*) FROM files WHERE build_id = ?1 AND status = 'retry'),
                    (SELECT MIN(next_attempt_at) FROM files WHERE build_id = ?1 AND status = 'retry'),
                    (SELECT COALESCE(MAX(attempt_count), 0) FROM files WHERE build_id = ?1),
                    (SELECT COUNT(*) FROM entities WHERE build_id = ?1),
                    (SELECT COUNT(*) FROM documents WHERE build_id = ?1),
                    (SELECT COUNT(*) FROM chunks WHERE build_id = ?1),
                    (SELECT COUNT(*) FROM relationships WHERE build_id = ?1),
                    (SELECT COUNT(*) FROM cross_links WHERE build_id = ?1),
                    (SELECT finished_at FROM builds WHERE id = ?1),
                    (SELECT value FROM metadata WHERE key = 'graph_state')",
                &[&build_id],
                |r| {
                    Ok(BuildSummary {
                        files: r.get(0)?,
                        indexed: r.get(1)?,
                        failed: r.get(2)?,
                        retry: r.get(3)?,
                        next_retry_at: r.get(4)?,
                        max_attempt_count: r.get(5)?,
                        entities: r.get(6)?,
                        documents: r.get(7)?,
                        chunks: r.get(8)?,
                        relationships: r.get(9)?,
                        links: r.get(10)?,
                        published_at: r.get(11)?,
                        graph_state: r.get(12)?,
                    })
                },
            )?
            .pop()
            .unwrap_or_default())
    }

    /// The effective graph status of `build_id` from its [`BuildSummary`]
    /// (no extra statement).
    pub fn graph_status_of(build_id: &str, s: &BuildSummary) -> crate::graph::GraphStatus {
        let state: crate::graph::GraphState = s
            .graph_state
            .as_deref()
            .and_then(|r| serde_json::from_str(r).ok())
            .unwrap_or_default();
        let generation = format!("{build_id}@{}", s.published_at.as_deref().unwrap_or(""));
        state.status(Some(&generation))
    }

    /// When each failing file of `build_id` last failed: `file_id ->
    /// occurred_at` of its newest recorded error.
    pub fn file_error_times(&self, build_id: &str) -> Result<BTreeMap<String, String>> {
        Ok(self
            .query_rows(
                "file error times",
                "SELECT f.id, MAX(e.occurred_at) FROM files f
                 JOIN index_errors e ON e.file_id = f.id AND e.source_id = f.source_id
                 WHERE f.build_id = ?1 AND f.last_error IS NOT NULL
                 GROUP BY f.id",
                &[&build_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?
            .into_iter()
            .collect())
    }

    /// `(last_indexed_at, updated_at)` of every file in `build_id`.
    pub fn file_times(&self, build_id: &str) -> Result<BTreeMap<String, FileTimes>> {
        Ok(self
            .query_rows(
                "file times",
                "SELECT id, last_indexed_at, updated_at FROM files WHERE build_id = ?1",
                &[&build_id],
                |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))),
            )?
            .into_iter()
            .collect())
    }

    /// Most recent errors of this source, newest first.
    pub fn recent_errors(&self, limit: i64) -> Result<Vec<ErrorRecord>> {
        let source = self.source_id().to_owned();
        self.query_rows(
            "recent errors",
            "SELECT rel_path, error_code, error_message, occurred_at FROM index_errors
             WHERE source_id = ?1 ORDER BY occurred_at DESC, id DESC LIMIT ?2",
            &[&source, &limit],
            error_row,
        )
    }
}

fn error_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ErrorRecord> {
    Ok(ErrorRecord {
        path: r.get(0)?,
        error_code: r.get(1)?,
        error_message: r.get(2)?,
        occurred_at: r.get(3)?,
    })
}
