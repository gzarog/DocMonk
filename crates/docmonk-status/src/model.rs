//! The canonical status contract (`schema_version` 1).
//!
//! Every surface (CLI text and JSON, MCP `docmonk_status`, Admin UI)
//! renders exactly this model. Semantics:
//!
//! * **Authority.** Local mode reads only the home's SQLite catalog and
//!   project stores; server mode reads only the configured
//!   OpenSearch/Elasticsearch prefix (`backend.authoritative`).
//! * **Published vs in progress.** `published` counters describe the
//!   source's active (published) build only. Work of a running pass is in
//!   `live` and never mixes into published counts.
//! * **Scan vs publication.** `last_scan_at` only says a filesystem scan
//!   completed; a source is `completed` once a build is published.
//! * **Unknown is null.** A metric that the authoritative input cannot
//!   observe (pre-scan totals, remote PIDs, server disk bytes, ETAs) is
//!   `null`, never a guessed `0`. A section that failed to load is listed
//!   in `diagnostics.missing_sections` and the report is `partial`.

use serde::{Deserialize, Serialize};

/// Version of this contract. Bumped on any incompatible shape change;
/// consumers never read another version.
pub const SCHEMA_VERSION: u32 = 1;

/// Recent errors kept in a report unless a caller asks for fewer/more.
pub const DEFAULT_ERROR_LIMIT: usize = 25;
/// Upper bound on `recent_errors` for any caller (response size cap).
pub const MAX_ERROR_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Local,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Sqlite,
    Opensearch,
    Elasticsearch,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Sqlite => "sqlite",
            BackendKind::Opensearch => "opensearch",
            BackendKind::Elasticsearch => "elasticsearch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Healthy,
    Degraded,
    Failed,
    Unknown,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthState::Healthy => "healthy",
            HealthState::Degraded => "degraded",
            HealthState::Failed => "failed",
            HealthState::Unknown => "unknown",
        }
    }
}

/// A source's indexing state. Precedence is defined by
/// [`crate::health::source_index_state`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceIndexState {
    NotIndexed,
    Queued,
    Scanning,
    Indexing,
    Finalizing,
    Publishing,
    Completed,
    Retrying,
    Failed,
    Stalled,
    Offline,
    Disabled,
    Unknown,
}

impl SourceIndexState {
    pub const ALL: [SourceIndexState; 13] = [
        SourceIndexState::NotIndexed,
        SourceIndexState::Queued,
        SourceIndexState::Scanning,
        SourceIndexState::Indexing,
        SourceIndexState::Finalizing,
        SourceIndexState::Publishing,
        SourceIndexState::Completed,
        SourceIndexState::Retrying,
        SourceIndexState::Failed,
        SourceIndexState::Stalled,
        SourceIndexState::Offline,
        SourceIndexState::Disabled,
        SourceIndexState::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SourceIndexState::NotIndexed => "not_indexed",
            SourceIndexState::Queued => "queued",
            SourceIndexState::Scanning => "scanning",
            SourceIndexState::Indexing => "indexing",
            SourceIndexState::Finalizing => "finalizing",
            SourceIndexState::Publishing => "publishing",
            SourceIndexState::Completed => "completed",
            SourceIndexState::Retrying => "retrying",
            SourceIndexState::Failed => "failed",
            SourceIndexState::Stalled => "stalled",
            SourceIndexState::Offline => "offline",
            SourceIndexState::Disabled => "disabled",
            SourceIndexState::Unknown => "unknown",
        }
    }

    /// A pass is working on the source right now.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            SourceIndexState::Queued
                | SourceIndexState::Scanning
                | SourceIndexState::Indexing
                | SourceIndexState::Finalizing
                | SourceIndexState::Publishing
        )
    }

    /// Needs attention (`docmonk status --errors`).
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            SourceIndexState::Retrying
                | SourceIndexState::Failed
                | SourceIndexState::Stalled
                | SourceIndexState::Offline
                | SourceIndexState::Unknown
        )
    }
}

/// Whether the source root is reachable (independent of index state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Online,
    Offline,
    Disabled,
    Unknown,
}

/// The source's build lifecycle as stored by the authoritative backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    /// Never published.
    NotIndexed,
    /// A pending build is being written; the active build (if any) serves.
    Building,
    Ready,
    /// The last pending build was discarded; the active build (if any) serves.
    Failed,
    /// The next pass rebuilds from scratch (versions changed, manual).
    NeedsFullRebuild,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemScope {
    Backend,
    Cluster,
    Runtime,
    Source,
    File,
}

/// Stable machine-readable problem codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemCode {
    BackendUnreachable,
    BackendSchemaMismatch,
    BackendSectionUnavailable,
    ClusterRed,
    ClusterYellow,
    ClusterHealthUnknown,
    SnapshotInconsistent,
    RunStalled,
    RunLeaseExpired,
    RunCrashed,
    RunFailed,
    RunInterrupted,
    SourceOffline,
    AllSourcesOffline,
    SourceBuildFailed,
    SourceLastError,
    FilesFailed,
    FilesRetrying,
    PendingWithoutIndexer,
    /// The relationship graph failed or is stale; the index itself is
    /// published and searchable.
    RelationshipsNotCurrent,
    /// The source's cross-source dependencies are stale or pending; the
    /// index itself is published and searchable.
    DependenciesNotCurrent,
}

impl ProblemCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProblemCode::BackendUnreachable => "backend_unreachable",
            ProblemCode::BackendSchemaMismatch => "backend_schema_mismatch",
            ProblemCode::BackendSectionUnavailable => "backend_section_unavailable",
            ProblemCode::ClusterRed => "cluster_red",
            ProblemCode::ClusterYellow => "cluster_yellow",
            ProblemCode::ClusterHealthUnknown => "cluster_health_unknown",
            ProblemCode::SnapshotInconsistent => "snapshot_inconsistent",
            ProblemCode::RunStalled => "run_stalled",
            ProblemCode::RunLeaseExpired => "run_lease_expired",
            ProblemCode::RunCrashed => "run_crashed",
            ProblemCode::RunFailed => "run_failed",
            ProblemCode::RunInterrupted => "run_interrupted",
            ProblemCode::SourceOffline => "source_offline",
            ProblemCode::AllSourcesOffline => "all_sources_offline",
            ProblemCode::SourceBuildFailed => "source_build_failed",
            ProblemCode::SourceLastError => "source_last_error",
            ProblemCode::FilesFailed => "files_failed",
            ProblemCode::FilesRetrying => "files_retrying",
            ProblemCode::PendingWithoutIndexer => "pending_without_indexer",
            ProblemCode::RelationshipsNotCurrent => "relationships_not_current",
            ProblemCode::DependenciesNotCurrent => "dependencies_not_current",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealth {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterInfo {
    /// `null` when the health call failed.
    pub health: Option<ClusterHealth>,
    pub unassigned_shards: Option<u64>,
    pub number_of_nodes: Option<u64>,
    /// Scope of the health reading (the DocMonk indexes of the prefix).
    pub indexes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub kind: BackendKind,
    /// Server mode only.
    pub index_prefix: Option<String>,
    /// Server URL with credentials removed; local mode: `null`.
    pub endpoint: Option<String>,
    pub reachable: bool,
    /// Always true: status never mixes a second, non-authoritative store.
    pub authoritative: bool,
    pub cluster: Option<ClusterInfo>,
    /// Round trip of the cheapest backend read of this snapshot.
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthSummary {
    pub state: HealthState,
    pub problem_count: usize,
}

/// Whether the host/cluster run view is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// Fresh heartbeat (and, in server mode, a valid writer lease).
    Live,
    /// Heartbeat older than the stall threshold.
    Stalled,
    /// Server lease or heartbeat expired: the run is not running.
    Expired,
    /// The run reported a terminal outcome.
    Finished,
}

/// One source pass of one run, on this host or (server mode) any host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveRun {
    pub run_id: Option<String>,
    pub source_id: String,
    pub host: Option<String>,
    /// Local mode only: a remote PID is never proof of anything.
    pub pid: Option<i64>,
    pub operation: Option<String>,
    pub stage: Option<String>,
    pub liveness: Liveness,
    pub scanned: Option<u64>,
    /// Files this pass must process; `null` until the scan finished.
    pub planned: Option<u64>,
    pub processed: Option<u64>,
    pub indexed: Option<u64>,
    pub failed: Option<u64>,
    pub retry: Option<u64>,
    /// `processed / planned`, clamped to 0..=100; `null` without a planned total.
    pub percentage: Option<f64>,
    pub started_at: Option<String>,
    pub last_progress_at: Option<String>,
    pub heartbeat_at: Option<String>,
    pub heartbeat_age_seconds: Option<f64>,
    /// Server mode: the writer-lease fencing token the run holds.
    pub lease_token: Option<i64>,
    /// Server mode: the source's current lease matches this run and is live.
    pub lease_valid: Option<bool>,
    pub outcome: Option<String>,
    /// A relationship graph being built by this pass (local runs): the
    /// phase it is in and its derivation progress, which reaches its total
    /// before resolving, linking and committing are done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationships: Option<RelationshipProgress>,
    /// The embedding stage of this pass while it runs (local runs): vectors
    /// computed of those planned, the rolling rate since the stage started
    /// and the ETA it implies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingProgress>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingProgress {
    pub processed: u64,
    pub planned: u64,
    /// Vectors per second since the embedding stage started; `null` before
    /// the first page is written.
    pub rate_per_second: Option<f64>,
    /// Seconds left at that rate; `null` without a rate.
    pub eta_seconds: Option<f64>,
}

impl EmbeddingProgress {
    /// Progress at `elapsed` seconds into the stage.
    pub fn at(processed: u64, planned: u64, elapsed: Option<f64>) -> Self {
        let rate = elapsed
            .filter(|e| *e > 0.0 && processed > 0)
            .map(|e| processed as f64 / e);
        let eta = rate.map(|r| (planned.saturating_sub(processed) as f64 / r).round());
        Self {
            processed,
            planned,
            rate_per_second: rate.map(|r| (r * 10.0).round() / 10.0),
            eta_seconds: eta,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipProgress {
    /// `deriving`, `resolving`, `linking`, `committing`; on a server also
    /// `staging`, `writing`, `closing`, `promoting`.
    pub phase: Option<String>,
    /// Files derived so far, of `planned`.
    pub processed: Option<u64>,
    pub planned: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexerScope {
    /// This host's runs (local mode).
    Host,
    /// Every host writing to the server prefix.
    Cluster,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockInfo {
    /// `free`, `held` or `unknown`.
    pub state: String,
    pub pid: Option<i64>,
    pub operation: Option<String>,
    pub hostname: Option<String>,
    pub source_id: Option<String>,
    pub acquired_at: Option<String>,
}

/// The last run recorded on this host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    pub run_id: Option<String>,
    pub operation: Option<String>,
    /// `completed`, `failed`, `interrupted` or `crashed`.
    pub outcome: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexerInfo {
    pub scope: IndexerScope,
    pub active_run_count: usize,
    pub active_source_count: usize,
    pub max_parallel_sources: Option<u64>,
    /// Every visible source pass, active ones first.
    pub workers: Vec<LiveRun>,
    /// Resource-governor permits (local runs only, as observed).
    pub resources: Option<serde_json::Value>,
    /// This host's whole-home lock (local mode).
    pub lock: Option<LockInfo>,
    pub last_run: Option<LastRun>,
    pub stall_threshold_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceCounts {
    pub registered: u64,
    pub enabled: u64,
    pub active: u64,
    pub failed: u64,
    pub offline: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileCounts {
    /// Files known to the published builds (every status); `null` when a
    /// published section is unavailable.
    pub discovered: Option<u64>,
    pub published_indexed: Option<u64>,
    pub failed: Option<u64>,
    pub retrying: Option<u64>,
    /// Planned minus processed over live passes; `null` when no live pass
    /// knows its planned total.
    pub pending_in_current_runs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub sources: SourceCounts,
    pub files: FileCounts,
    pub entities: Option<u64>,
    pub documents: Option<u64>,
    pub chunks: Option<u64>,
    #[serde(default)]
    pub passages: Option<u64>,
    pub relationships: Option<u64>,
    /// Only from two comparable snapshots (`--watch`).
    pub throughput_files_per_second: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceBuild {
    pub active_id: Option<String>,
    pub pending_id: Option<String>,
    pub state: BuildState,
    pub published_at: Option<String>,
}

/// Counters of the source's published build.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PublishedCounts {
    pub files: u64,
    pub indexed: u64,
    pub failed: u64,
    pub retrying: u64,
    pub code_entities: u64,
    pub documents: u64,
    pub chunks: u64,
    /// Implementation passages of code files (ADR 0037).
    #[serde(default)]
    pub passages: u64,
    /// Indexed files with a recoverable extraction problem (syntax errors
    /// indexed as plain text, ADR 0037).
    #[serde(default)]
    pub degraded: u64,
    pub relationships: u64,
    pub links: u64,
    pub max_attempt_count: u64,
    pub next_retry_at: Option<String>,
}

/// A source's relationship graph, independent of its index state: the
/// base index is searchable whatever this says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipStatus {
    /// `disabled`, `pending`, `building`, `ready`, `stale` or `failed`
    /// (a `ready` graph of another base generation reports `stale`).
    pub state: String,
    /// The published graph generation.
    pub generation: Option<String>,
    /// The base build the graph was derived from.
    pub base_build_id: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub stale_reason: Option<String>,
}

/// A source's cross-source dependencies as a consumer (bindings to the
/// producers it references).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyStatus {
    /// `current`, `stale` or `pending`.
    pub state: String,
    pub reason: Option<String>,
    pub updated_at: Option<String>,
    /// The producers the bindings were computed against.
    pub producers: Vec<String>,
    /// The targeted retry scheduled on this host, if any.
    pub retry: Option<DependencyRetry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyRetry {
    pub code: String,
    pub last_error: String,
    pub attempts: u32,
    pub last_attempt_at: Option<String>,
    pub next_attempt_at: Option<String>,
}

/// Index readiness and complete-graph readiness, reported independently:
/// a searchable index is never presented as a complete graph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readiness {
    /// A published build is searchable.
    pub index_ready: bool,
    /// The source graph is ready for the published build and every
    /// required cross-source dependency is current.
    pub complete_graph_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub source_id: String,
    pub path: String,
    pub source_type: String,
    pub enabled: bool,
    pub access: Access,
    pub index_state: SourceIndexState,
    /// The relationship graph stage (`null` when unknown).
    #[serde(default)]
    pub relationship_state: Option<RelationshipStatus>,
    /// The cross-source dependency stage (`null` when the source has no
    /// dependencies or they are unknown).
    #[serde(default)]
    pub dependency_state: Option<DependencyStatus>,
    #[serde(default)]
    pub readiness: Readiness,
    pub build: SourceBuild,
    /// `null`: the source has no published build, or the published
    /// section could not be read (see diagnostics).
    pub published: Option<PublishedCounts>,
    /// The source's pass in progress (or the latest one visible).
    pub live: Option<LiveRun>,
    pub last_scan_at: Option<String>,
    pub last_error_at: Option<String>,
    pub last_error: Option<String>,
    pub warnings: Vec<String>,
    /// Git sources only (ADR 0043; absent for folder sources).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitSourceStatus>,
}

/// A git source's remote and commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSourceStatus {
    pub url: String,
    pub branch: String,
    pub subpath: Option<String>,
    /// The commit the last successful fetch checked out.
    pub commit: Option<String>,
    /// The commit of the last pass that indexed the checkout.
    pub indexed_commit: Option<String>,
    pub fetched_at: Option<String>,
    /// Why the last fetch failed.
    pub last_error: Option<String>,
}

impl From<docmonk_indexing::git::GitRemote> for GitSourceStatus {
    fn from(g: docmonk_indexing::git::GitRemote) -> Self {
        Self {
            url: g.url,
            branch: g.branch,
            subpath: g.subpath,
            commit: g.commit,
            indexed_commit: g.indexed_commit,
            fetched_at: g.fetched_at,
            last_error: g.last_error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    pub code: ProblemCode,
    pub severity: Severity,
    pub scope: ProblemScope,
    pub source_id: Option<String>,
    pub host: Option<String>,
    pub message: String,
    /// How to look into it.
    pub hint: Option<String>,
    pub observed_at: Option<String>,
}

/// One recorded file/source error, globally ordered by `occurred_at`
/// (newest first; ties by source id then path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub source_id: String,
    pub path: Option<String>,
    pub code: String,
    pub message: String,
    /// `retry` or `failed` for file errors of the published build.
    pub status: Option<String>,
    pub attempt_count: Option<u64>,
    pub occurred_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    /// Every aggregate was read against one frozen source/build set.
    Consistent,
    /// A publication happened while reading; the changed sources were re-read.
    Retried,
    /// A publication or garbage collection kept changing the view; the
    /// listed sources may be incomplete.
    Inconsistent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedSection {
    pub section: String,
    pub age_seconds: f64,
    pub ttl_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostics {
    pub partial: bool,
    pub missing_sections: Vec<String>,
    pub collection_ms: Option<u64>,
    /// Backend round trips of this snapshot (server: HTTP requests;
    /// local: SQL statements).
    pub request_count: Option<u64>,
    pub cached_sections: Vec<CachedSection>,
    pub consistency: Consistency,
    pub inconsistent_sources: Vec<String>,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub observed_at: String,
    pub mode: Mode,
    pub backend: BackendInfo,
    pub health: HealthSummary,
    pub indexer: IndexerInfo,
    pub summary: Summary,
    pub sources: Vec<SourceStatus>,
    pub problems: Vec<Problem>,
    pub recent_errors: Vec<ErrorEvent>,
    pub diagnostics: Diagnostics,
}

impl StatusReport {
    pub fn source(&self, id: &str) -> Option<&SourceStatus> {
        self.sources.iter().find(|s| s.source_id == id)
    }

    /// Process exit code: a backend that could not be inspected is `4`
    /// (unavailable); anything else `0` (health is reported, not raised).
    pub fn exit_code(&self) -> i32 {
        if self.backend.reachable {
            0
        } else {
            4
        }
    }
}

/// Why a snapshot could not be collected at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectError {
    /// The authoritative backend is unreachable.
    Unavailable(String),
    /// The backend exists but has another schema identity.
    SchemaMismatch(String),
    /// A required local database is corrupt or unreadable.
    Database(String),
    /// Configuration prevents collection.
    Config(String),
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollectError::Unavailable(m)
            | CollectError::SchemaMismatch(m)
            | CollectError::Database(m)
            | CollectError::Config(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CollectError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_serialize_as_documented() {
        let names: Vec<String> = SourceIndexState::ALL
            .iter()
            .map(|s| {
                serde_json::to_value(s)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "not_indexed",
                "queued",
                "scanning",
                "indexing",
                "finalizing",
                "publishing",
                "completed",
                "retrying",
                "failed",
                "stalled",
                "offline",
                "disabled",
                "unknown"
            ]
        );
        for s in SourceIndexState::ALL {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
        assert_eq!(
            serde_json::to_value(BackendKind::Elasticsearch).unwrap(),
            "elasticsearch"
        );
        assert_eq!(
            serde_json::to_value(ProblemCode::ClusterRed).unwrap(),
            ProblemCode::ClusterRed.as_str()
        );
    }

    #[test]
    fn unknown_fields_are_not_aliases() {
        // The contract has no alias for any earlier field name.
        let v = serde_json::json!({"state": "healthy", "problem_count": 0, "status": "x"});
        let h: HealthSummary = serde_json::from_value(v).unwrap();
        assert_eq!(h.state, HealthState::Healthy);
        assert!(serde_json::from_value::<HealthSummary>(
            serde_json::json!({"status": "healthy", "problem_count": 0})
        )
        .is_err());
    }
}
