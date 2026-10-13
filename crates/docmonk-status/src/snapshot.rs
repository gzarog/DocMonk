//! Collector-neutral snapshot facts and the one report builder.
//!
//! The local and server collectors only gather facts ([`Collected`]);
//! [`assemble`] turns them into the canonical [`StatusReport`] through the
//! single health evaluator, so the two modes can never drift apart.

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};

use crate::health;
use crate::model::*;

/// What a caller wants from a snapshot.
#[derive(Debug, Clone)]
pub struct CollectOptions {
    /// Recent errors to keep (clamped to [`MAX_ERROR_LIMIT`]).
    pub error_limit: usize,
    /// Only this source's errors in `recent_errors`.
    pub error_source: Option<String>,
    /// Replace file paths in errors with `<redacted>`.
    pub redact_paths: bool,
    /// Heartbeat age after which a run is stalled.
    pub stall_threshold_seconds: f64,
}

impl Default for CollectOptions {
    fn default() -> Self {
        Self {
            error_limit: DEFAULT_ERROR_LIMIT,
            error_source: None,
            redact_paths: false,
            stall_threshold_seconds: 120.0,
        }
    }
}

impl CollectOptions {
    pub fn error_limit(&self) -> usize {
        self.error_limit.min(MAX_ERROR_LIMIT)
    }
}

/// One source as the authoritative backend records it.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFacts {
    pub source_id: String,
    pub path: String,
    pub source_type: String,
    pub enabled: bool,
    pub access: Access,
    pub build_state: BuildState,
    pub active_build_id: Option<String>,
    pub pending_build_id: Option<String>,
    pub published_at: Option<String>,
    pub last_scan_at: Option<String>,
    pub last_error: Option<String>,
    pub last_error_at: Option<String>,
    /// `None`: no published build, or (see `published_missing`) unreadable.
    pub published: Option<PublishedCounts>,
    /// The published section of this source could not be read.
    pub published_missing: bool,
    /// Server mode: the source-state writer lease.
    pub lease: Option<LeaseFacts>,
    /// The relationship graph (`None` when unknown).
    pub relationships: Option<RelationshipStatus>,
}

/// A source's server writer lease as stored in its state document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseFacts {
    pub owner: String,
    pub token: i64,
    pub expires_at: Option<String>,
    pub live: bool,
}

/// Everything a collector observed in one snapshot.
#[derive(Debug, Clone)]
pub struct Collected {
    pub mode: Mode,
    pub backend: BackendInfo,
    pub sources: Vec<SourceFacts>,
    /// Source passes visible to this snapshot (liveness already judged).
    pub runs: Vec<LiveRun>,
    pub scope: IndexerScope,
    pub max_parallel_sources: Option<u64>,
    pub resources: Option<serde_json::Value>,
    pub lock: Option<LockInfo>,
    pub last_run: Option<LastRun>,
    /// `None`: the error feed could not be read.
    pub errors: Option<Vec<ErrorEvent>>,
    pub missing_sections: Vec<String>,
    pub request_count: Option<u64>,
    pub cached_sections: Vec<CachedSection>,
    pub consistency: Consistency,
    pub inconsistent_sources: Vec<String>,
}

/// Builds the report: per-source states, summary, problems and health.
pub fn assemble(
    c: Collected,
    opts: &CollectOptions,
    now: DateTime<Utc>,
    collection_ms: Option<u64>,
) -> StatusReport {
    let observed_at = iso(now);
    let mut sources: Vec<SourceStatus> = c
        .sources
        .iter()
        .map(|f| {
            let live = c
                .runs
                .iter()
                .filter(|r| r.source_id == f.source_id)
                .min_by_key(|r| run_rank(r))
                .cloned();
            let index_state = health::source_index_state(f, live.as_ref());
            let warnings = health::source_warnings(f, live.as_ref());
            SourceStatus {
                source_id: f.source_id.clone(),
                path: f.path.clone(),
                source_type: f.source_type.clone(),
                enabled: f.enabled,
                access: f.access,
                index_state,
                relationship_state: f.relationships.clone(),
                build: SourceBuild {
                    active_id: f.active_build_id.clone(),
                    pending_id: f.pending_build_id.clone(),
                    state: f.build_state,
                    published_at: f.published_at.clone(),
                },
                published: f.published.clone(),
                live,
                last_scan_at: f.last_scan_at.clone(),
                last_error_at: f.last_error_at.clone(),
                last_error: f.last_error.as_deref().map(crate::errors::redact_text),
                warnings,
            }
        })
        .collect();
    sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));

    let mut runs = c.runs.clone();
    runs.sort_by(|a, b| {
        run_rank(a)
            .cmp(&run_rank(b))
            .then_with(|| a.source_id.cmp(&b.source_id))
            .then_with(|| a.host.cmp(&b.host))
    });
    let active: Vec<&LiveRun> = runs
        .iter()
        .filter(|r| matches!(r.liveness, Liveness::Live | Liveness::Stalled))
        .collect();
    let mut run_ids: Vec<(&Option<String>, &Option<String>)> =
        active.iter().map(|r| (&r.run_id, &r.host)).collect();
    run_ids.sort();
    run_ids.dedup();
    let indexer = IndexerInfo {
        scope: c.scope,
        active_run_count: run_ids.len(),
        active_source_count: active.len(),
        max_parallel_sources: c.max_parallel_sources,
        // Finished passes stay visible per source (`sources[].live`).
        workers: runs
            .iter()
            .filter(|r| r.liveness != Liveness::Finished)
            .cloned()
            .collect(),
        resources: c.resources.clone(),
        lock: c.lock.clone(),
        last_run: c.last_run.clone(),
        stall_threshold_seconds: opts.stall_threshold_seconds,
    };

    let summary = summarize(&c, &sources, &active);
    let recent_errors = c.errors.as_ref().map_or_else(Vec::new, |e| {
        crate::errors::select(e.clone(), opts.error_limit(), opts.redact_paths)
    });
    let mut missing = c.missing_sections.clone();
    missing.sort();
    missing.dedup();
    let partial = !missing.is_empty() || c.consistency == Consistency::Inconsistent;
    let problems = health::problems(&health::ProblemInput {
        backend: &c.backend,
        facts: &c.sources,
        runs: &runs,
        last_run: c.last_run.as_ref(),
        missing_sections: &missing,
        consistency: c.consistency,
        inconsistent_sources: &c.inconsistent_sources,
        now,
    });
    let state = health::overall(&problems, partial);
    StatusReport {
        schema_version: SCHEMA_VERSION,
        snapshot_id: snapshot_id(now),
        observed_at,
        mode: c.mode,
        backend: c.backend,
        health: HealthSummary {
            state,
            problem_count: problems.len(),
        },
        indexer,
        summary,
        sources,
        problems,
        recent_errors,
        diagnostics: Diagnostics {
            partial,
            missing_sections: missing,
            collection_ms,
            request_count: c.request_count,
            cached_sections: c.cached_sections,
            consistency: c.consistency,
            inconsistent_sources: c.inconsistent_sources,
        },
    }
}

/// Active passes first, then stalled, expired, finished.
fn run_rank(r: &LiveRun) -> u8 {
    match r.liveness {
        Liveness::Live => 0,
        Liveness::Stalled => 1,
        Liveness::Expired => 2,
        Liveness::Finished => 3,
    }
}

fn summarize(c: &Collected, sources: &[SourceStatus], active: &[&LiveRun]) -> Summary {
    let count = |p: &dyn Fn(&SourceStatus) -> bool| sources.iter().filter(|s| p(s)).count() as u64;
    let published_known = !c.sources.iter().any(|f| f.published_missing);
    let sum = |f: &dyn Fn(&PublishedCounts) -> u64| -> Option<u64> {
        published_known.then(|| {
            sources
                .iter()
                .filter_map(|s| s.published.as_ref())
                .map(f)
                .sum()
        })
    };
    let pending: Option<u64> = active
        .iter()
        .map(|r| Some(r.planned?.saturating_sub(r.processed.unwrap_or(0))))
        .sum::<Option<u64>>()
        .filter(|_| !active.is_empty());
    Summary {
        sources: SourceCounts {
            registered: sources.len() as u64,
            enabled: count(&|s| s.enabled),
            active: count(&|s| s.index_state.is_active()),
            failed: count(&|s| s.index_state == SourceIndexState::Failed),
            offline: count(&|s| s.index_state == SourceIndexState::Offline),
        },
        files: FileCounts {
            discovered: sum(&|p| p.files),
            published_indexed: sum(&|p| p.indexed),
            failed: sum(&|p| p.failed),
            retrying: sum(&|p| p.retrying),
            pending_in_current_runs: pending,
        },
        entities: sum(&|p| p.code_entities),
        documents: sum(&|p| p.documents),
        chunks: sum(&|p| p.chunks),
        relationships: sum(&|p| p.relationships),
        throughput_files_per_second: None,
    }
}

/// `processed / planned` as a percentage in `0..=100`; `None` without a
/// planned total.
pub fn percentage(processed: Option<u64>, planned: Option<u64>) -> Option<f64> {
    let planned = planned?;
    if planned == 0 {
        return Some(100.0);
    }
    let pct = processed.unwrap_or(0) as f64 * 100.0 / planned as f64;
    Some((pct.clamp(0.0, 100.0) * 10.0).round() / 10.0)
}

/// `snap-<utc>-<pid>`: unique per process and millisecond.
pub fn snapshot_id(now: DateTime<Utc>) -> String {
    format!(
        "snap-{}-{}",
        now.format("%Y%m%dT%H%M%S%.3fZ"),
        std::process::id()
    )
}

/// RFC 3339 UTC with microseconds and `Z`: every timestamp in a report
/// has this one format, so they sort as strings.
pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Parses RFC 3339, naive ISO (UTC) and epoch milliseconds (server dates).
pub fn parse_time(v: &str) -> Option<DateTime<Utc>> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if v.bytes().all(|b| b.is_ascii_digit()) {
        return v
            .parse::<i64>()
            .ok()
            .and_then(DateTime::from_timestamp_millis);
    }
    DateTime::parse_from_rfc3339(v)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })
}

/// A stored timestamp in the report format; unparseable values are dropped
/// (never shown as a time they are not).
pub fn normalize_time(v: Option<&str>) -> Option<String> {
    v.and_then(parse_time).map(iso)
}

/// Seconds from `then` to `now`, never negative, rounded to 0.1.
pub fn age_seconds(then: Option<&str>, now: DateTime<Utc>) -> Option<f64> {
    let t = parse_time(then?)?;
    let secs = (now - t).num_milliseconds().max(0) as f64 / 1000.0;
    Some((secs * 10.0).round() / 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_normalize_from_every_stored_format() {
        let want = Some("2026-10-08T15:00:00.000000Z".to_owned());
        assert_eq!(normalize_time(Some("1791471600000")), want);
        assert_eq!(
            normalize_time(Some("2026-10-08T15:00:00.000000+00:00")),
            want
        );
        assert_eq!(normalize_time(Some("2026-10-08T15:00:00")), want);
        assert_eq!(normalize_time(Some("2026-10-08T17:00:00+02:00")), want);
        assert_eq!(normalize_time(Some("yesterday")), None);
        assert_eq!(normalize_time(None), None);
    }

    #[test]
    fn percentage_is_bounded_and_null_without_a_total() {
        assert_eq!(percentage(Some(5), None), None);
        assert_eq!(percentage(Some(0), Some(0)), Some(100.0));
        assert_eq!(percentage(Some(1), Some(3)), Some(33.3));
        assert_eq!(percentage(Some(9), Some(3)), Some(100.0));
        assert_eq!(percentage(None, Some(3)), Some(0.0));
    }

    #[test]
    fn ages_are_never_negative() {
        let now = parse_time("2026-10-08T15:00:00Z").unwrap();
        assert_eq!(age_seconds(Some("2026-10-08T14:59:30Z"), now), Some(30.0));
        assert_eq!(age_seconds(Some("2026-10-08T15:00:30Z"), now), Some(0.0));
        assert_eq!(age_seconds(None, now), None);
    }
}
