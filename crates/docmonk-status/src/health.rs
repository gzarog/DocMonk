//! The one health evaluator: per-source index state, the problem list and
//! the overall verdict. Pure functions over collected facts, the same for
//! local and server mode.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::model::*;
use crate::snapshot::{iso, parse_time, SourceFacts};

/// The canonical stage → index state mapping of a live pass.
pub fn stage_state(stage: Option<&str>) -> SourceIndexState {
    match stage {
        None | Some("starting" | "queued") => SourceIndexState::Queued,
        Some("scanning") => SourceIndexState::Scanning,
        Some("finalizing") => SourceIndexState::Finalizing,
        Some("publishing") => SourceIndexState::Publishing,
        Some(_) => SourceIndexState::Indexing,
    }
}

/// Per-source index state. Precedence (first match wins):
///
/// 1. `disabled` — the source is disabled;
/// 2. `offline` — its root was unreachable on the last scan;
/// 3. a live pass: `queued | scanning | indexing | finalizing | publishing`
///    from its stage;
/// 4. `stalled` — a pass that stopped heartbeating, or whose server lease
///    expired, without a terminal outcome;
/// 5. `unknown` — the published section could not be read;
/// 6. `retrying` — files of the published build wait for a retry;
/// 7. `failed` — the last build failed and nothing was ever published;
/// 8. `completed` — a published build serves the source;
/// 9. `not_indexed`.
pub fn source_index_state(f: &SourceFacts, live: Option<&LiveRun>) -> SourceIndexState {
    if !f.enabled {
        return SourceIndexState::Disabled;
    }
    if f.access == Access::Offline {
        return SourceIndexState::Offline;
    }
    match live.map(|r| r.liveness) {
        Some(Liveness::Live) => return stage_state(live.and_then(|r| r.stage.as_deref())),
        Some(Liveness::Stalled | Liveness::Expired) => return SourceIndexState::Stalled,
        Some(Liveness::Finished) | None => {}
    }
    if f.published_missing {
        return SourceIndexState::Unknown;
    }
    if f.published.as_ref().is_some_and(|p| p.retrying > 0) {
        return SourceIndexState::Retrying;
    }
    if f.active_build_id.is_none() && f.build_state == BuildState::Failed {
        return SourceIndexState::Failed;
    }
    if f.active_build_id.is_some() {
        return SourceIndexState::Completed;
    }
    SourceIndexState::NotIndexed
}

/// Short per-source notes shown next to the state.
pub fn source_warnings(f: &SourceFacts, live: Option<&LiveRun>) -> Vec<String> {
    let mut w = Vec::new();
    let running = live.is_some_and(|r| r.liveness == Liveness::Live);
    if f.active_build_id.is_some() && f.build_state == BuildState::Failed {
        w.push("last build failed; the previous published build still serves".into());
    }
    if f.build_state == BuildState::Building && !running {
        w.push("a pending build has no live pass (interrupted)".into());
    }
    if f.build_state == BuildState::NeedsFullRebuild && f.active_build_id.is_some() {
        w.push("the next pass rebuilds from scratch".into());
    }
    if f.published_missing {
        w.push("published counters unavailable".into());
    }
    w
}

/// Inputs of [`problems`].
pub struct ProblemInput<'a> {
    pub backend: &'a BackendInfo,
    pub facts: &'a [SourceFacts],
    pub runs: &'a [LiveRun],
    pub last_run: Option<&'a LastRun>,
    pub missing_sections: &'a [String],
    pub consistency: Consistency,
    pub inconsistent_sources: &'a [String],
    pub now: DateTime<Utc>,
}

struct Builder {
    out: Vec<Problem>,
    seen: BTreeSet<(ProblemCode, Option<String>, Option<String>, String)>,
    observed_at: String,
}

impl Builder {
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        code: ProblemCode,
        severity: Severity,
        scope: ProblemScope,
        source_id: Option<&str>,
        host: Option<&str>,
        message: String,
        hint: Option<&str>,
    ) {
        let message = crate::errors::redact_text(&message);
        let key = (
            code,
            source_id.map(str::to_owned),
            host.map(str::to_owned),
            message.clone(),
        );
        if !self.seen.insert(key) {
            return;
        }
        self.out.push(Problem {
            code,
            severity,
            scope,
            source_id: source_id.map(str::to_owned),
            host: host.map(str::to_owned),
            message,
            hint: hint.map(str::to_owned),
            observed_at: Some(self.observed_at.clone()),
        });
    }
}

/// Every problem of a snapshot, most severe first (stable within a severity).
pub fn problems(i: &ProblemInput<'_>) -> Vec<Problem> {
    use ProblemCode as C;
    use ProblemScope as Sc;
    use Severity::*;
    let mut b = Builder {
        out: Vec::new(),
        seen: BTreeSet::new(),
        observed_at: iso(i.now),
    };

    // Backend and cluster.
    if !i.backend.reachable {
        b.add(
            C::BackendUnreachable,
            Error,
            Sc::Backend,
            None,
            None,
            format!("{} backend is unreachable", i.backend.kind.as_str()),
            Some("check storage.server.url, credentials and the cluster"),
        );
    }
    if let Some(cluster) = &i.backend.cluster {
        let unassigned = cluster
            .unassigned_shards
            .map(|n| format!(" ({n} unassigned shard(s))"))
            .unwrap_or_default();
        match cluster.health {
            Some(ClusterHealth::Red) => b.add(
                C::ClusterRed,
                Error,
                Sc::Cluster,
                None,
                None,
                format!("cluster health is red{unassigned}: some RagMonk data is not searchable"),
                Some("check GET /_cluster/allocation/explain on the cluster"),
            ),
            Some(ClusterHealth::Yellow) => b.add(
                C::ClusterYellow,
                Warning,
                Sc::Cluster,
                None,
                None,
                format!("cluster health is yellow{unassigned}: replicas are not allocated"),
                Some(if cluster.number_of_nodes == Some(1) {
                    "single-node cluster: replica shards can never be allocated; set the RagMonk indexes' number_of_replicas to 0"
                } else {
                    "check GET /_cluster/allocation/explain on the cluster"
                }),
            ),
            Some(ClusterHealth::Green) => {}
            None => b.add(
                C::ClusterHealthUnknown,
                Warning,
                Sc::Cluster,
                None,
                None,
                "cluster health could not be read".into(),
                None,
            ),
        }
    }
    for section in i.missing_sections {
        b.add(
            C::BackendSectionUnavailable,
            Warning,
            Sc::Backend,
            None,
            None,
            format!("status section unavailable: {section}"),
            Some("the values of this section are shown as unknown, not zero"),
        );
    }
    if i.consistency == Consistency::Inconsistent {
        b.add(
            C::SnapshotInconsistent,
            Warning,
            Sc::Backend,
            None,
            None,
            format!(
                "publications kept changing while reading {} source(s); their counters may be incomplete",
                i.inconsistent_sources.len()
            ),
            Some("refresh ragmonk status"),
        );
    }

    // Runs.
    for r in i.runs {
        let who = run_label(r);
        match r.liveness {
            Liveness::Stalled => b.add(
                C::RunStalled,
                Error,
                Sc::Runtime,
                Some(&r.source_id),
                r.host.as_deref(),
                format!(
                    "{who} is stalled: no heartbeat for {}s (stage {})",
                    r.heartbeat_age_seconds.map_or_else(|| "?".into(), |a| format!("{a:.0}")),
                    r.stage.as_deref().unwrap_or("unknown")
                ),
                Some("check the indexing host's logs; raise indexing.status_stall_threshold_seconds for very large files"),
            ),
            Liveness::Expired => b.add(
                C::RunLeaseExpired,
                Warning,
                Sc::Runtime,
                Some(&r.source_id),
                r.host.as_deref(),
                format!(
                    "{who} stopped without finishing (last stage {}); it is not running",
                    r.stage.as_deref().unwrap_or("unknown")
                ),
                Some("the next pass of this source resumes; check the indexing host's logs"),
            ),
            Liveness::Live | Liveness::Finished => {}
        }
    }
    let any_live = i.runs.iter().any(|r| r.liveness == Liveness::Live);
    if let (Some(last), false) = (i.last_run, any_live) {
        let op = last.operation.as_deref().unwrap_or("index");
        let error = last.error.as_deref().unwrap_or("");
        match last.outcome.as_str() {
            "crashed" => b.add(
                C::RunCrashed,
                Error,
                Sc::Runtime,
                None,
                None,
                format!("the last {op} run exited without finishing"),
                Some("run 'ragmonk index' again; the previous published builds still serve"),
            ),
            "failed" => b.add(
                C::RunFailed,
                Error,
                Sc::Runtime,
                None,
                None,
                format!("the last {op} run failed: {error}"),
                Some("ragmonk doctor"),
            ),
            "interrupted" => b.add(
                C::RunInterrupted,
                Warning,
                Sc::Runtime,
                None,
                None,
                format!("the last {op} run was interrupted"),
                None,
            ),
            _ => {}
        }
    }

    // Sources.
    let enabled: Vec<&SourceFacts> = i.facts.iter().filter(|f| f.enabled).collect();
    let offline = enabled
        .iter()
        .filter(|f| f.access == Access::Offline)
        .count();
    if !enabled.is_empty() && offline == enabled.len() {
        b.add(
            C::AllSourcesOffline,
            Error,
            Sc::Source,
            None,
            None,
            "every enabled source is offline".into(),
            Some("check that the source roots are mounted"),
        );
    }
    for f in &enabled {
        let id = Some(f.source_id.as_str());
        if f.access == Access::Offline {
            b.add(
                C::SourceOffline,
                Warning,
                Sc::Source,
                id,
                None,
                format!(
                    "source offline: {}",
                    f.last_error.as_deref().unwrap_or("root unreachable")
                ),
                Some("its published knowledge is kept until the root is back"),
            );
            continue;
        }
        let build_failed = f.build_state == BuildState::Failed;
        if build_failed {
            let (sev, msg) = if f.active_build_id.is_some() {
                (
                    Warning,
                    "the last build failed; the previous published build still serves",
                )
            } else {
                (Error, "the source's build failed and nothing is published")
            };
            let detail = f
                .last_error
                .as_deref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            b.add(
                C::SourceBuildFailed,
                sev,
                Sc::Source,
                id,
                None,
                format!("{msg}{detail}"),
                Some("ragmonk index --source <id>"),
            );
        } else if let Some(e) = f.last_error.as_deref().filter(|e| !e.is_empty()) {
            b.add(
                C::SourceLastError,
                Warning,
                Sc::Source,
                id,
                None,
                e.to_owned(),
                None,
            );
        }
        if let Some(r) = f.relationships.as_ref().filter(|r| {
            f.active_build_id.is_some() && matches!(r.state.as_str(), "failed" | "stale")
        }) {
            let why = r
                .last_error
                .as_deref()
                .or(r.stale_reason.as_deref())
                .map(|w| format!(": {}", crate::errors::redact_text(w)))
                .unwrap_or_default();
            b.add(
                C::RelationshipsNotCurrent,
                Warning,
                Sc::Source,
                id,
                None,
                format!(
                    "relationship graph is {}{why}; the index is published and searchable",
                    r.state
                ),
                Some("ragmonk relationships build"),
            );
        }
        if let Some(p) = &f.published {
            if p.failed > 0 {
                b.add(
                    C::FilesFailed,
                    Warning,
                    Sc::File,
                    id,
                    None,
                    format!("{} file(s) failed in the published build", p.failed),
                    Some("ragmonk status --errors"),
                );
            }
            if p.retrying > 0 {
                let next = p
                    .next_retry_at
                    .as_deref()
                    .map(|t| format!("; next attempt {t}"))
                    .unwrap_or_default();
                b.add(
                    C::FilesRetrying,
                    Warning,
                    Sc::File,
                    id,
                    None,
                    format!(
                        "{} file(s) wait for a retry in the published build{next}",
                        p.retrying
                    ),
                    Some("a later pass retries them; this does not mean a pass is running"),
                );
                let due = p
                    .next_retry_at
                    .as_deref()
                    .and_then(parse_time)
                    .is_some_and(|t| t <= i.now);
                if due && !any_live {
                    b.add(
                        C::PendingWithoutIndexer,
                        Info,
                        Sc::Runtime,
                        id,
                        None,
                        format!(
                            "{} retry file(s) are due with no indexer running",
                            p.retrying
                        ),
                        Some("run 'ragmonk index' or start the daemon"),
                    );
                }
            }
        }
    }
    let mut out = b.out;
    out.sort_by(|a, b| b.severity.cmp(&a.severity));
    out
}

fn run_label(r: &LiveRun) -> String {
    let mut s = format!("the pass of {}", r.source_id);
    if let Some(h) = &r.host {
        s += &format!(" on {h}");
    }
    if let Some(id) = &r.run_id {
        s += &format!(" (run {id})");
    }
    s
}

/// Overall health: any error → `failed`; a partial snapshot →
/// `unknown` (it cannot vouch for what it did not read); any warning →
/// `degraded`; otherwise `healthy`. Info problems never degrade health.
pub fn overall(problems: &[Problem], partial: bool) -> HealthState {
    if problems.iter().any(|p| p.severity == Severity::Error) {
        HealthState::Failed
    } else if partial {
        HealthState::Unknown
    } else if problems.iter().any(|p| p.severity == Severity::Warning) {
        HealthState::Degraded
    } else {
        HealthState::Healthy
    }
}
